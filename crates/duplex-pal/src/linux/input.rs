//! Debian Linux Kernel Input PAL (`evdev` + `epoll` + `EVIOCGRAB` Capture & `/dev/uinput` Injection).
//!
//! Operates directly at the Linux kernel input subsystem layer (`<linux/input.h>` / `<linux/uinput.h>`),
//! ensuring full compatibility across modern Wayland compositors (GNOME Mutter, KDE KWin, Sway/Hyprland)
//! as well as legacy X11 sessions.

use crate::shm::monotonic_now_ns;
use crate::{CapturedInputEvent, InputBackend, PalError};
use core::sync::atomic::Ordering;
use duplex_proto::{
    hid_to_linux_evdev, linux_evdev_to_hid, DepressedKeyBitmap, HidEvent, HidMouseButton,
    OperationalMode, PanicDetector, ShmWatchdogBlock, ALL_HID_MODIFIER_USAGES, KEYMAP_TABLE,
};
use std::ffi::CString;
use std::fs;
use std::os::unix::io::RawFd;
use std::sync::Arc;
use tracing::{debug, info, warn};

// Linux <linux/input-event-codes.h> constants
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;

const SYN_REPORT: u16 = 0;

const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;

const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;

const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
const BTN_SIDE: u16 = 0x113;
const BTN_EXTRA: u16 = 0x114;
const BTN_TOUCH: u16 = 0x14A;
const BTN_TOOL_DOUBLETAP: u16 = 0x14D;
const BTN_TOOL_TRIPLETAP: u16 = 0x14E;

// Linux ioctl encodings for evdev ('E' = 0x45) and uinput ('U' = 0x55)
const EVIOCGID: libc::c_ulong = 0x8008_4502;
const EVIOCGRAB: libc::c_ulong = 0x4004_4590;
const EVIOCSCLOCKID: libc::c_ulong = 0x4004_45a0;

const UI_DEV_CREATE: libc::c_ulong = 0x5501;
const UI_DEV_DESTROY: libc::c_ulong = 0x5502;
const UI_DEV_SETUP: libc::c_ulong = 0x405C_5503;
const UI_SET_EVBIT: libc::c_ulong = 0x4004_5564;
const UI_SET_KEYBIT: libc::c_ulong = 0x4004_5565;
const UI_SET_RELBIT: libc::c_ulong = 0x4004_5566;

/// Custom Vendor/Product ID stamped onto our `/dev/uinput` virtual device so our `evdev` scanner
/// never grabs its own virtual injector (preventing feedback loops).
pub const DUPLEX_UINPUT_VENDOR: u16 = 0x1D6B;
pub const DUPLEX_UINPUT_PRODUCT: u16 = 0x0104;
pub const DUPLEX_UINPUT_VERSION: u16 = 0x0001;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct LinuxInputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
struct LinuxUinputSetup {
    id: LinuxInputId,
    name: [libc::c_char; 80],
    ff_effects_max: u32,
}

#[inline]
const fn eviocgbit(ev: u8, len: usize) -> libc::c_ulong {
    (2usize << 30 | (len & 0x3FFF) << 16 | (0x45usize << 8) | (0x20usize + ev as usize))
        as libc::c_ulong
}

/// RAII wrapper around a grabbed `/dev/input/event*` file descriptor.
///
/// Guarantees that `ioctl(fd, EVIOCGRAB, 0)` and `close(fd)` are executed whenever the handle
/// is dropped—and if the process crashes or receives `SIGKILL`, the Linux kernel automatically
/// closes `fd` and releases the `EVIOCGRAB` lock.
struct GrabbedEvdevDevice {
    fd: RawFd,
    path: String,
    monotonic_events: bool,
}

impl GrabbedEvdevDevice {
    fn open_and_grab(path: &str) -> Result<Self, PalError> {
        let c_path = CString::new(path).map_err(|e| PalError::Input(e.to_string()))?;
        let fd = unsafe {
            libc::open(
                c_path.as_ptr(),
                libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(PalError::Input(format!(
                "open({}) failed: {}",
                path,
                std::io::Error::last_os_error()
            )));
        }

        let rc = unsafe { libc::ioctl(fd, EVIOCGRAB, 1 as libc::c_int) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(PalError::Input(format!(
                "ioctl(EVIOCGRAB, 1) on {} failed: {}",
                path, err
            )));
        }

        let clock = libc::CLOCK_MONOTONIC;
        let monotonic_events = unsafe { libc::ioctl(fd, EVIOCSCLOCKID, &clock) } == 0;
        Ok(Self {
            fd,
            path: path.to_owned(),
            monotonic_events,
        })
    }
}

impl Drop for GrabbedEvdevDevice {
    fn drop(&mut self) {
        unsafe {
            let _ = libc::ioctl(self.fd, EVIOCGRAB, 0 as libc::c_int);
            let _ = libc::close(self.fd);
        }
        debug!(device = %self.path, "Released EVIOCGRAB and closed evdev fd");
    }
}

/// Kernel `/dev/uinput` virtual keyboard + mouse injector.
pub struct LinuxUinputInjector {
    fd: RawFd,
    depressed: DepressedKeyBitmap,
    shm: Option<Arc<crate::shm::ShmWatchdog>>,
}

impl LinuxUinputInjector {
    pub fn new(shm: Option<Arc<crate::shm::ShmWatchdog>>) -> Result<Self, PalError> {
        let c_path = CString::new("/dev/uinput").unwrap();
        let fd = unsafe {
            libc::open(
                c_path.as_ptr(),
                libc::O_WRONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(PalError::Input(format!(
                "Failed to open /dev/uinput: {}",
                std::io::Error::last_os_error()
            )));
        }

        unsafe {
            // Enable EV_SYN, EV_KEY, EV_REL
            for ev in [
                EV_SYN as libc::c_int,
                EV_KEY as libc::c_int,
                EV_REL as libc::c_int,
            ] {
                if libc::ioctl(fd, UI_SET_EVBIT, ev) < 0 {
                    let err = std::io::Error::last_os_error();
                    libc::close(fd);
                    return Err(PalError::Input(format!("UI_SET_EVBIT failed: {err}")));
                }
            }

            // Enable all mapped keyboard evdev keycodes
            for &(_, evdev_code, _) in KEYMAP_TABLE {
                let _ = libc::ioctl(fd, UI_SET_KEYBIT, evdev_code as libc::c_int);
            }

            // Enable standard mouse buttons
            for btn in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA] {
                let _ = libc::ioctl(fd, UI_SET_KEYBIT, btn as libc::c_int);
            }

            // Enable relative axes
            for rel in [REL_X, REL_Y, REL_WHEEL, REL_HWHEEL] {
                let _ = libc::ioctl(fd, UI_SET_RELBIT, rel as libc::c_int);
            }

            let mut setup = LinuxUinputSetup {
                id: LinuxInputId {
                    bustype: 0x03, // BUS_USB
                    vendor: DUPLEX_UINPUT_VENDOR,
                    product: DUPLEX_UINPUT_PRODUCT,
                    version: DUPLEX_UINPUT_VERSION,
                },
                name: [0; 80],
                ff_effects_max: 0,
            };
            let dev_name = b"Duplex-KVM Virtual HID";
            for (i, &b) in dev_name.iter().enumerate() {
                setup.name[i] = b as libc::c_char;
            }

            if libc::ioctl(fd, UI_DEV_SETUP, &setup as *const LinuxUinputSetup) < 0 {
                let err = std::io::Error::last_os_error();
                libc::close(fd);
                return Err(PalError::Input(format!("UI_DEV_SETUP failed: {err}")));
            }

            if libc::ioctl(fd, UI_DEV_CREATE) < 0 {
                let err = std::io::Error::last_os_error();
                libc::close(fd);
                return Err(PalError::Input(format!("UI_DEV_CREATE failed: {err}")));
            }
        }

        Ok(Self {
            fd,
            depressed: DepressedKeyBitmap::new(),
            shm,
        })
    }

    #[inline]
    fn write_raw_event(&self, type_: u16, code: u16, value: i32) -> Result<(), PalError> {
        let ev = libc::input_event {
            time: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            type_,
            code,
            value,
        };
        let written = unsafe {
            libc::write(
                self.fd,
                &ev as *const libc::input_event as *const libc::c_void,
                core::mem::size_of::<libc::input_event>(),
            )
        };
        if written < 0 {
            return Err(PalError::Input(format!(
                "/dev/uinput write failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(())
    }

    pub fn inject(&mut self, event: &HidEvent) -> Result<(), PalError> {
        match *event {
            HidEvent::Key { usage, pressed } => {
                if let Some(evdev_code) = hid_to_linux_evdev(usage) {
                    self.depressed.set_pressed(usage, pressed);
                    if let Some(ref shm) = self.shm {
                        shm.block().set_key_depressed(usage, pressed);
                    }
                    self.write_raw_event(EV_KEY, evdev_code, if pressed { 1 } else { 0 })?;
                    self.write_raw_event(EV_SYN, SYN_REPORT, 0)?;
                }
            }
            HidEvent::MouseMoveRel { dx, dy } => {
                if dx != 0 {
                    self.write_raw_event(EV_REL, REL_X, i32::from(dx))?;
                }
                if dy != 0 {
                    self.write_raw_event(EV_REL, REL_Y, i32::from(dy))?;
                }
                if dx != 0 || dy != 0 {
                    self.write_raw_event(EV_SYN, SYN_REPORT, 0)?;
                }
            }
            HidEvent::MouseButton { button, pressed } => {
                let code = match button {
                    HidMouseButton::Left => BTN_LEFT,
                    HidMouseButton::Right => BTN_RIGHT,
                    HidMouseButton::Middle => BTN_MIDDLE,
                    HidMouseButton::Back => BTN_SIDE,
                    HidMouseButton::Forward => BTN_EXTRA,
                };
                self.write_raw_event(EV_KEY, code, if pressed { 1 } else { 0 })?;
                self.write_raw_event(EV_SYN, SYN_REPORT, 0)?;
            }
            HidEvent::MouseWheel {
                vertical,
                horizontal,
            } => {
                if vertical != 0 {
                    self.write_raw_event(EV_REL, REL_WHEEL, i32::from(vertical))?;
                }
                if horizontal != 0 {
                    self.write_raw_event(EV_REL, REL_HWHEEL, i32::from(horizontal))?;
                }
                if vertical != 0 || horizontal != 0 {
                    self.write_raw_event(EV_SYN, SYN_REPORT, 0)?;
                }
            }
            HidEvent::ReleaseAllModifiers => {
                self.release_all_stuck_keys()?;
            }
        }
        Ok(())
    }

    /// Iterates all set bits in the local and shared-memory 256-bit depressed-key bitmaps,
    /// plus unconditionally releases all 8 modifier keys (`Ctrl`, `Shift`, `Alt`, `Super/Meta`).
    pub fn release_all_stuck_keys(&mut self) -> Result<(), PalError> {
        for usage in self.depressed.drain_pressed() {
            if let Some(code) = hid_to_linux_evdev(usage) {
                let _ = self.write_raw_event(EV_KEY, code, 0);
            }
        }
        if let Some(ref shm) = self.shm {
            let mut shm_bmp = shm.block().take_depressed_bitmap();
            for usage in shm_bmp.drain_pressed() {
                if let Some(code) = hid_to_linux_evdev(usage) {
                    let _ = self.write_raw_event(EV_KEY, code, 0);
                }
            }
        }
        for usage in ALL_HID_MODIFIER_USAGES {
            if let Some(code) = hid_to_linux_evdev(usage) {
                let _ = self.write_raw_event(EV_KEY, code, 0);
            }
        }
        // Also release mouse buttons
        for btn in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA] {
            let _ = self.write_raw_event(EV_KEY, btn, 0);
        }
        self.write_raw_event(EV_SYN, SYN_REPORT, 0)?;
        Ok(())
    }
}

impl Drop for LinuxUinputInjector {
    fn drop(&mut self) {
        let _ = self.release_all_stuck_keys();
        unsafe {
            let _ = libc::ioctl(self.fd, UI_DEV_DESTROY);
            let _ = libc::close(self.fd);
        }
    }
}

/// Gesture state, independent of device I/O so timing boundaries can be tested.
#[derive(Default)]
struct TouchpadState {
    touch_active: bool,
    two_finger: bool,
    last_x: Option<i32>,
    last_y: Option<i32>,
    scroll_x_rem: i32,
    scroll_y_rem: i32,
    touch_start_ns: u64,
    tap_motion: i32,
    max_fingers: u8,
    last_tap_release_ns: u64,
    dragging: bool,
    suppress_tap: bool,
    start_x: Option<i32>,
    start_y: Option<i32>,
    tolerance_x: i32,
    tolerance_y: i32,
}

impl TouchpadState {
    fn advance_time(&mut self, now_ns: u64, out: &mut Vec<CapturedInputEvent>) {
        if self.last_tap_release_ns > 0
            && !self.dragging
            && now_ns.saturating_sub(self.last_tap_release_ns) > 280_000_000
        {
            self.last_tap_release_ns = 0;
            out.push(CapturedInputEvent::Hid(HidEvent::MouseButton {
                button: HidMouseButton::Left,
                pressed: false,
            }));
        }
    }

    fn configure_resolution(&mut self, fd: RawFd) {
        // EVIOCGABS reports input_absinfo, including units per millimetre.
        for (axis, tolerance) in [
            (ABS_X, &mut self.tolerance_x),
            (ABS_Y, &mut self.tolerance_y),
        ] {
            let mut info = [0i32; 6];
            if unsafe { libc::ioctl(fd, 0x8018_4540u64 + u64::from(axis), info.as_mut_ptr()) } == 0
            {
                *tolerance = info[5].saturating_mul(3).max(45);
            }
        }
    }

    fn observe_position(&mut self, axis: u16, value: i32) -> Option<i32> {
        let (start, last, tolerance) = if axis == ABS_X {
            (
                &mut self.start_x,
                &mut self.last_x,
                self.tolerance_x.max(45),
            )
        } else {
            (
                &mut self.start_y,
                &mut self.last_y,
                self.tolerance_y.max(45),
            )
        };
        let origin = *start.get_or_insert(value);
        // Measure the furthest displacement, rather than summing sensor jitter.
        let normalized = ((i64::from(value) - i64::from(origin)).abs() * 45 / i64::from(tolerance))
            .min(i64::from(i32::MAX)) as i32;
        self.tap_motion = self.tap_motion.max(normalized);
        last.replace(value)
            .map(|previous| value.saturating_sub(previous))
    }

    /// Normalizes tap/drag transitions without accessing evdev or uinput.
    fn handle_key(
        &mut self,
        code: u16,
        pressed: bool,
        now_ns: u64,
        out: &mut Vec<CapturedInputEvent>,
    ) -> bool {
        self.advance_time(now_ns, out);
        if code == BTN_TOUCH {
            if self.touch_active == pressed {
                return true;
            }
            if pressed {
                self.suppress_tap = false;
                self.touch_active = true;
                self.touch_start_ns = now_ns;
                self.tap_motion = 0;
                self.start_x = None;
                self.start_y = None;
                self.last_x = None;
                self.last_y = None;
                if self.max_fingers == 0 {
                    self.max_fingers = 1;
                }

                // Tap-and-hold (tap-to-drag): check if this touch-down arrived within
                // 280ms of a preceding single-finger tap release
                let time_since_last_tap = now_ns.saturating_sub(self.last_tap_release_ns);
                if self.last_tap_release_ns > 0
                    && time_since_last_tap <= 280_000_000
                    && !self.two_finger
                {
                    self.dragging = true;
                    self.last_tap_release_ns = 0;
                    debug!("Trackpad tap-to-drag started");
                    // The first tap's button is still down. Sending a second
                    // down here makes Windows dispatch WM_LBUTTONDBLCLK and
                    // applications maximize/open instead of starting a drag.
                }
            } else {
                if self.dragging {
                    debug!("Trackpad tap-to-drag released");
                    self.dragging = false;
                    self.last_tap_release_ns = 0;
                    out.push(CapturedInputEvent::Hid(HidEvent::MouseButton {
                        button: HidMouseButton::Left,
                        pressed: false,
                    }));
                    if now_ns.saturating_sub(self.touch_start_ns) <= 180_000_000
                        && self.tap_motion < 45
                    {
                        // Two short taps are still a double click. A held or
                        // moved second contact is one continuous button press.
                        out.push(CapturedInputEvent::Hid(HidEvent::MouseButton {
                            button: HidMouseButton::Left,
                            pressed: true,
                        }));
                        out.push(CapturedInputEvent::Hid(HidEvent::MouseButton {
                            button: HidMouseButton::Left,
                            pressed: false,
                        }));
                    }
                } else {
                    // Tap duration and displacement are independent of report rate.
                    let dur_ns = now_ns.saturating_sub(self.touch_start_ns);
                    if self.touch_active
                        && !self.suppress_tap
                        && dur_ns <= 180_000_000
                        && self.tap_motion < 45
                    {
                        let btn = if self.max_fingers >= 2 {
                            HidMouseButton::Right
                        } else {
                            HidMouseButton::Left
                        };
                        out.push(CapturedInputEvent::Hid(HidEvent::MouseButton {
                            button: btn,
                            pressed: true,
                        }));
                        if btn == HidMouseButton::Left {
                            // Defer release during the drag decision window.
                            // A second touch continues this same button press.
                            debug!("Trackpad single tap accepted; drag armed");
                            self.last_tap_release_ns = now_ns;
                        } else {
                            out.push(CapturedInputEvent::Hid(HidEvent::MouseButton {
                                button: btn,
                                pressed: false,
                            }));
                            self.last_tap_release_ns = 0;
                        }
                    } else {
                        self.last_tap_release_ns = 0;
                    }
                }
                self.touch_active = false;
                self.last_x = None;
                self.last_y = None;
                self.scroll_x_rem = 0;
                self.scroll_y_rem = 0;
                self.max_fingers = 0;
            }
            return true;
        }

        if code == BTN_TOOL_DOUBLETAP {
            self.two_finger = pressed;
            if pressed && (self.dragging || self.last_tap_release_ns > 0) {
                self.suppress_tap = true;
                self.dragging = false;
                self.last_tap_release_ns = 0;
                out.push(CapturedInputEvent::Hid(HidEvent::MouseButton {
                    button: HidMouseButton::Left,
                    pressed: false,
                }));
            }
            // Reset reference coordinates when switching between 1-finger and 2-finger
            self.last_x = None;
            self.last_y = None;
            self.scroll_x_rem = 0;
            self.scroll_y_rem = 0;
            if pressed {
                self.max_fingers = self.max_fingers.max(2);
            }
            return true;
        }

        if code == BTN_TOOL_TRIPLETAP {
            if pressed {
                self.max_fingers = self.max_fingers.max(3);
            }
            return true;
        }

        false
    }
}

pub struct LinuxInputBackend {
    epoll_fd: RawFd,
    passive_epoll_fd: RawFd,
    grabbed_devices: Vec<GrabbedEvdevDevice>,
    passive_fds: Vec<RawFd>,
    injector: Option<LinuxUinputInjector>,
    panic_detector: PanicDetector,
    shm: Option<Arc<crate::shm::ShmWatchdog>>,
    rel_dx_accum: i16,
    rel_dy_accum: i16,
    rel_vwheel_accum: i16,
    rel_hwheel_accum: i16,
    touchpad: TouchpadState,
    last_hotkey_ns: u64,
}

impl LinuxInputBackend {
    pub fn new(shm: Option<Arc<crate::shm::ShmWatchdog>>) -> Result<Self, PalError> {
        let epoll_fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if epoll_fd < 0 {
            return Err(PalError::Input(format!(
                "epoll_create1 failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        let passive_epoll_fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };

        // Attempt to open /dev/uinput injector eagerly (or lazily if running unprivileged in unit tests)
        let injector = match LinuxUinputInjector::new(shm.clone()) {
            Ok(inj) => Some(inj),
            Err(e) => {
                warn!("Could not initialize /dev/uinput at startup (will retry on inject): {e}");
                None
            }
        };

        let mut backend = Self {
            epoll_fd,
            passive_epoll_fd,
            grabbed_devices: Vec::new(),
            passive_fds: Vec::new(),
            injector,
            panic_detector: PanicDetector::new(),
            shm,
            rel_dx_accum: 0,
            rel_dy_accum: 0,
            rel_vwheel_accum: 0,
            rel_hwheel_accum: 0,
            touchpad: TouchpadState::default(),
            last_hotkey_ns: 0,
        };
        backend.open_passive_hotkey_listeners();
        Ok(backend)
    }

    /// Opens physical keyboard `/dev/input/event*` devices in non-exclusive (`O_RDONLY | O_NONBLOCK`,
    /// NO `EVIOCGRAB`) mode so global shortcuts (`Ctrl+Alt+P`, `Ctrl+Alt+0..3`) work even in `Mode 0 (Idle)`.
    fn open_passive_hotkey_listeners(&mut self) {
        self.close_passive_hotkey_listeners();
        if self.passive_epoll_fd < 0 {
            return;
        }
        let Ok(entries) = fs::read_dir("/dev/input") else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if !name_str.starts_with("event") {
                continue;
            }
            let path = format!("/dev/input/{name_str}");
            if !Self::is_capturable_evdev_device(&path) {
                continue;
            }
            let Ok(c_path) = CString::new(path.as_str()) else {
                continue;
            };
            let fd = unsafe {
                libc::open(
                    c_path.as_ptr(),
                    libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
                )
            };
            if fd >= 0 {
                let mut ev = libc::epoll_event {
                    events: libc::EPOLLIN as u32,
                    u64: fd as u64,
                };
                let rc = unsafe {
                    libc::epoll_ctl(
                        self.passive_epoll_fd,
                        libc::EPOLL_CTL_ADD,
                        fd,
                        &mut ev as *mut libc::epoll_event,
                    )
                };
                if rc == 0 {
                    self.passive_fds.push(fd);
                } else {
                    unsafe {
                        let _ = libc::close(fd);
                    }
                }
            }
        }
    }

    fn close_passive_hotkey_listeners(&mut self) {
        for fd in self.passive_fds.drain(..) {
            unsafe {
                if self.passive_epoll_fd >= 0 {
                    let _ = libc::epoll_ctl(
                        self.passive_epoll_fd,
                        libc::EPOLL_CTL_DEL,
                        fd,
                        core::ptr::null_mut(),
                    );
                }
                let _ = libc::close(fd);
            }
        }
    }

    fn poll_passive_hotkeys(&mut self) {
        if self.passive_epoll_fd < 0 || self.passive_fds.is_empty() {
            return;
        }
        let mut epoll_events = [libc::epoll_event { events: 0, u64: 0 }; 16];
        let ready = unsafe {
            libc::epoll_wait(
                self.passive_epoll_fd,
                epoll_events.as_mut_ptr(),
                epoll_events.len() as libc::c_int,
                0,
            )
        };
        if ready <= 0 {
            return;
        }
        let now_ns = monotonic_now_ns();
        let mut raw_buf = [libc::input_event {
            time: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            type_: 0,
            code: 0,
            value: 0,
        }; 32];

        for item in epoll_events.iter().take(ready as usize) {
            let fd = item.u64 as RawFd;
            loop {
                let bytes_read = unsafe {
                    libc::read(
                        fd,
                        raw_buf.as_mut_ptr() as *mut libc::c_void,
                        core::mem::size_of_val(&raw_buf),
                    )
                };
                if bytes_read <= 0 {
                    break;
                }
                let count = (bytes_read as usize) / core::mem::size_of::<libc::input_event>();
                for ev in &raw_buf[..count] {
                    if ev.type_ == EV_KEY && (ev.value == 0 || ev.value == 1) {
                        let pressed = ev.value == 1;
                        if let Some(usage) = linux_evdev_to_hid(ev.code) {
                            let _ = self.panic_detector.observe_key(usage, pressed, now_ns);
                            if let Some(hotkey) =
                                self.panic_detector.observe_mode_hotkey(usage, pressed)
                            {
                                self.dispatch_hotkey_to_shm(hotkey, now_ns);
                            }
                        }
                    }
                }
            }
        }
    }

    fn dispatch_hotkey_to_shm(&mut self, hotkey: duplex_proto::HotkeyAction, now_ns: u64) {
        if now_ns.saturating_sub(self.last_hotkey_ns) < 350_000_000 {
            return;
        }
        self.last_hotkey_ns = now_ns;
        if let Some(ref shm) = self.shm {
            let block = shm.block();
            let cur = OperationalMode::from_u8_lossy(block.active_mode.load(Ordering::Acquire));
            let target = hotkey.resolve(cur);
            info!(
                from = ?cur,
                to = ?target,
                "Global keyboard shortcut (Ctrl+Alt+P / Ctrl+Alt+0..3) triggered mode switch"
            );
            block.requested_mode.store(target.as_u8(), Ordering::SeqCst);
        }
    }

    /// Inspects an `/dev/input/event*` node to determine if it is a physical keyboard, mouse, or trackpad
    /// (and NOT our own `Duplex-KVM Virtual HID` `/dev/uinput` device).
    fn is_capturable_evdev_device(path: &str) -> bool {
        let Ok(c_path) = CString::new(path) else {
            return false;
        };
        let fd = unsafe {
            libc::open(
                c_path.as_ptr(),
                libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return false;
        }

        let mut id = LinuxInputId::default();
        let rc = unsafe { libc::ioctl(fd, EVIOCGID, &mut id as *mut LinuxInputId) };
        if rc == 0 && id.vendor == DUPLEX_UINPUT_VENDOR && id.product == DUPLEX_UINPUT_PRODUCT {
            unsafe { libc::close(fd) };
            return false; // Skip our own virtual uinput device!
        }

        let mut ev_bits = [0u8; 4];
        let has_ev =
            unsafe { libc::ioctl(fd, eviocgbit(0, ev_bits.len()), ev_bits.as_mut_ptr()) } >= 0;
        if !has_ev {
            unsafe { libc::close(fd) };
            return false;
        }

        let supports_key = (ev_bits[0] & (1 << EV_KEY)) != 0;
        let supports_rel = (ev_bits[0] & (1 << EV_REL)) != 0;

        let mut key_bits = [0u8; 96];
        let _ = unsafe {
            libc::ioctl(
                fd,
                eviocgbit(EV_KEY as u8, key_bits.len()),
                key_bits.as_mut_ptr(),
            )
        };
        // Check if device has KEY_ESC (1) and KEY_ENTER (28) -> keyboard, or BTN_LEFT (0x110) -> mouse/trackpad
        let has_esc = (key_bits[1 / 8] & (1 << 1)) != 0;
        let has_enter = (key_bits[28 / 8] & (1 << (28 % 8))) != 0;
        let has_btn_left = (key_bits[BTN_LEFT as usize / 8] & (1 << (BTN_LEFT as usize % 8))) != 0;

        unsafe { libc::close(fd) };
        (supports_key && has_esc && has_enter) || (supports_key && has_btn_left) || supports_rel
    }

    fn latch_panic_in_shm(&self) {
        if let Some(ref shm) = self.shm {
            let block: &ShmWatchdogBlock = shm.block();
            block.panic_latched.store(true, Ordering::SeqCst);
            block.input_grab_active.store(false, Ordering::SeqCst);
            block
                .active_mode
                .store(OperationalMode::Mode0Idle.as_u8(), Ordering::SeqCst);
        }
    }

    fn reset_touchpad_state(&mut self) {
        let (tolerance_x, tolerance_y) = (self.touchpad.tolerance_x, self.touchpad.tolerance_y);
        self.touchpad = TouchpadState::default();
        self.touchpad.tolerance_x = tolerance_x;
        self.touchpad.tolerance_y = tolerance_y;
    }
}

impl InputBackend for LinuxInputBackend {
    fn start_capture(&mut self) -> Result<(), PalError> {
        self.close_passive_hotkey_listeners();
        self.stop_capture()?;
        self.close_passive_hotkey_listeners();
        self.panic_detector.reset();
        self.reset_touchpad_state();

        let entries = fs::read_dir("/dev/input")
            .map_err(|e| PalError::Input(format!("Failed to read /dev/input: {e}")))?;

        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let name_str = file_name.to_string_lossy();
            if !name_str.starts_with("event") {
                continue;
            }
            let path = format!("/dev/input/{name_str}");
            if !Self::is_capturable_evdev_device(&path) {
                continue;
            }

            match GrabbedEvdevDevice::open_and_grab(&path) {
                Ok(dev) => {
                    let mut ev = libc::epoll_event {
                        events: libc::EPOLLIN as u32,
                        u64: dev.fd as u64,
                    };
                    let rc = unsafe {
                        libc::epoll_ctl(
                            self.epoll_fd,
                            libc::EPOLL_CTL_ADD,
                            dev.fd,
                            &mut ev as *mut libc::epoll_event,
                        )
                    };
                    if rc == 0 {
                        self.touchpad.configure_resolution(dev.fd);
                        info!(device = %path, fd = dev.fd, "Acquired exclusive EVIOCGRAB on evdev device");
                        self.grabbed_devices.push(dev);
                    }
                }
                Err(e) => {
                    warn!(device = %path, error = %e, "Skipping evdev device grab");
                }
            }
        }

        if self.grabbed_devices.is_empty() {
            self.open_passive_hotkey_listeners();
            return Err(PalError::Input(
                "No keyboard, mouse or trackpad could be captured. Check /dev/input/event* permissions and membership in the input group; sign out and back in after network/input setup.".into(),
            ));
        }

        if let Some(ref shm) = self.shm {
            let now = monotonic_now_ns();
            shm.block().beat_daemon(now);
            shm.block().beat_peer(now.saturating_add(1_000_000_000));
            shm.block().input_grab_active.store(true, Ordering::Release);
        }

        Ok(())
    }

    fn stop_capture(&mut self) -> Result<(), PalError> {
        for dev in self.grabbed_devices.drain(..) {
            unsafe {
                let _ = libc::epoll_ctl(
                    self.epoll_fd,
                    libc::EPOLL_CTL_DEL,
                    dev.fd,
                    core::ptr::null_mut(),
                );
            }
            // `dev` drops here, immediately executing `ioctl(fd, EVIOCGRAB, 0)` and `close(fd)`.
        }
        self.panic_detector.reset();
        self.reset_touchpad_state();
        if let Some(ref shm) = self.shm {
            shm.block()
                .input_grab_active
                .store(false, Ordering::Release);
        }
        self.open_passive_hotkey_listeners();
        Ok(())
    }

    fn poll_captured_events(
        &mut self,
        out: &mut Vec<CapturedInputEvent>,
    ) -> Result<usize, PalError> {
        if self.grabbed_devices.is_empty() {
            self.poll_passive_hotkeys();
            return Ok(0);
        }

        let mut epoll_events = [libc::epoll_event { events: 0, u64: 0 }; 32];
        let ready = unsafe {
            libc::epoll_wait(
                self.epoll_fd,
                epoll_events.as_mut_ptr(),
                epoll_events.len() as libc::c_int,
                0, // Non-blocking zero-timeout poll
            )
        };

        if ready <= 0 {
            let before = out.len();
            self.touchpad.advance_time(monotonic_now_ns(), out);
            return Ok(out.len() - before);
        }

        let now_ns = monotonic_now_ns();
        let mut raw_buf = [libc::input_event {
            time: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            type_: 0,
            code: 0,
            value: 0,
        }; 64];

        let mut pushed = 0usize;

        for item in epoll_events.iter().take(ready as usize) {
            let fd = item.u64 as RawFd;
            let monotonic_events = self
                .grabbed_devices
                .iter()
                .any(|device| device.fd == fd && device.monotonic_events);
            loop {
                let bytes_read = unsafe {
                    libc::read(
                        fd,
                        raw_buf.as_mut_ptr() as *mut libc::c_void,
                        core::mem::size_of_val(&raw_buf),
                    )
                };
                if bytes_read <= 0 {
                    break;
                }

                let count = (bytes_read as usize) / core::mem::size_of::<libc::input_event>();
                for ev in &raw_buf[..count] {
                    let event_ns = if monotonic_events {
                        (ev.time.tv_sec as u64)
                            .saturating_mul(1_000_000_000)
                            .saturating_add((ev.time.tv_usec as u64).saturating_mul(1000))
                    } else {
                        now_ns
                    };
                    match ev.type_ {
                        EV_KEY => {
                            // Ignore value == 2 (kernel key auto-repeat) for wire normalization
                            if ev.value != 0 && ev.value != 1 {
                                continue;
                            }
                            let pressed = ev.value == 1;

                            let before = out.len();
                            let handled = self.touchpad.handle_key(ev.code, pressed, event_ns, out);
                            pushed += out.len() - before;
                            if handled {
                                continue;
                            }

                            // Check mouse buttons first
                            let mouse_btn = match ev.code {
                                BTN_LEFT => Some(HidMouseButton::Left),
                                BTN_RIGHT => Some(HidMouseButton::Right),
                                BTN_MIDDLE => Some(HidMouseButton::Middle),
                                BTN_SIDE => Some(HidMouseButton::Back),
                                BTN_EXTRA => Some(HidMouseButton::Forward),
                                _ => None,
                            };

                            if let Some(button) = mouse_btn {
                                if button == HidMouseButton::Left
                                    && (self.touchpad.dragging
                                        || self.touchpad.last_tap_release_ns > 0)
                                {
                                    self.touchpad.suppress_tap = true;
                                    self.touchpad.dragging = false;
                                    self.touchpad.last_tap_release_ns = 0;
                                    if pressed {
                                        // The physical click takes over the
                                        // already held synthetic button.
                                        continue;
                                    }
                                }
                                out.push(CapturedInputEvent::Hid(HidEvent::MouseButton {
                                    button,
                                    pressed,
                                }));
                                pushed += 1;
                                continue;
                            }

                            if let Some(usage) = linux_evdev_to_hid(ev.code) {
                                // Evaluate Hardware Panic Escape Hatch BEFORE queueing event
                                if let Some(trigger_kind) =
                                    self.panic_detector.observe_key(usage, pressed, now_ns)
                                {
                                    warn!(
                                        ?trigger_kind,
                                        "HARDWARE PANIC ESCAPE HATCH TRIPPED ON LINUX EVDEV"
                                    );
                                    self.emergency_panic_release()?;
                                    // Flush remote modifiers on the peer before disconnecting to Mode0Idle
                                    out.push(CapturedInputEvent::Hid(
                                        HidEvent::ReleaseAllModifiers,
                                    ));
                                    out.push(CapturedInputEvent::PanicTriggered(trigger_kind));
                                    return Ok(pushed + 2);
                                }

                                // Evaluate Global Mode-Switching Hotkeys (Ctrl+Alt+P / Ctrl+Alt+0..3)
                                if let Some(hotkey) =
                                    self.panic_detector.observe_mode_hotkey(usage, pressed)
                                {
                                    out.push(CapturedInputEvent::Hid(
                                        HidEvent::ReleaseAllModifiers,
                                    ));
                                    pushed += 1;
                                    self.dispatch_hotkey_to_shm(hotkey, now_ns);
                                    continue;
                                }

                                out.push(CapturedInputEvent::Hid(HidEvent::Key { usage, pressed }));
                                pushed += 1;
                            }
                        }
                        EV_REL => match ev.code {
                            REL_X => {
                                self.rel_dx_accum =
                                    self.rel_dx_accum.saturating_add(ev.value as i16);
                            }
                            REL_Y => {
                                self.rel_dy_accum =
                                    self.rel_dy_accum.saturating_add(ev.value as i16);
                            }
                            REL_WHEEL => {
                                self.rel_vwheel_accum =
                                    self.rel_vwheel_accum.saturating_add(ev.value as i16);
                            }
                            REL_HWHEEL => {
                                self.rel_hwheel_accum =
                                    self.rel_hwheel_accum.saturating_add(ev.value as i16);
                            }
                            _ => {}
                        },
                        EV_ABS => match ev.code {
                            ABS_X => {
                                if self.touchpad.touch_active || self.touchpad.two_finger {
                                    if let Some(raw_dx) =
                                        self.touchpad.observe_position(ABS_X, ev.value)
                                    {
                                        if self.touchpad.two_finger {
                                            self.touchpad.scroll_x_rem =
                                                self.touchpad.scroll_x_rem.saturating_add(raw_dx);
                                            let ticks = (self.touchpad.scroll_x_rem / 32)
                                                .clamp(i16::MIN as i32, i16::MAX as i32)
                                                as i16;
                                            if ticks != 0 {
                                                self.rel_hwheel_accum =
                                                    self.rel_hwheel_accum.saturating_sub(ticks);
                                                self.touchpad.scroll_x_rem %= 32;
                                            }
                                        } else {
                                            let delta = (raw_dx / 2)
                                                .clamp(i16::MIN as i32, i16::MAX as i32)
                                                as i16;
                                            self.rel_dx_accum =
                                                self.rel_dx_accum.saturating_add(delta);
                                        }
                                    }
                                }
                            }
                            ABS_Y
                                if (self.touchpad.touch_active || self.touchpad.two_finger) => {
                                    if let Some(raw_dy) =
                                        self.touchpad.observe_position(ABS_Y, ev.value)
                                    {
                                        if self.touchpad.two_finger {
                                            self.touchpad.scroll_y_rem =
                                                self.touchpad.scroll_y_rem.saturating_add(raw_dy);
                                            let ticks = (self.touchpad.scroll_y_rem / 28)
                                                .clamp(i16::MIN as i32, i16::MAX as i32)
                                                as i16;
                                            if ticks != 0 {
                                                // Natural two-finger scrolling: moving fingers up (raw_dy < 0) scrolls down (negative wheel)
                                                self.rel_vwheel_accum =
                                                    self.rel_vwheel_accum.saturating_add(ticks);
                                                self.touchpad.scroll_y_rem %= 28;
                                            }
                                        } else {
                                            let delta = (raw_dy / 2)
                                                .clamp(i16::MIN as i32, i16::MAX as i32)
                                                as i16;
                                            self.rel_dy_accum =
                                                self.rel_dy_accum.saturating_add(delta);
                                        }
                                    }
                                }
                            _ => {}
                        },
                        EV_SYN if ev.code == SYN_REPORT => {
                            if self.rel_dx_accum != 0 || self.rel_dy_accum != 0 {
                                out.push(CapturedInputEvent::Hid(HidEvent::MouseMoveRel {
                                    dx: self.rel_dx_accum,
                                    dy: self.rel_dy_accum,
                                }));
                                self.rel_dx_accum = 0;
                                self.rel_dy_accum = 0;
                                pushed += 1;
                            }
                            if self.rel_vwheel_accum != 0 || self.rel_hwheel_accum != 0 {
                                out.push(CapturedInputEvent::Hid(HidEvent::MouseWheel {
                                    vertical: self.rel_vwheel_accum,
                                    horizontal: self.rel_hwheel_accum,
                                }));
                                self.rel_vwheel_accum = 0;
                                self.rel_hwheel_accum = 0;
                                pushed += 1;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        let before = out.len();
        self.touchpad.advance_time(monotonic_now_ns(), out);
        Ok(pushed + out.len() - before)
    }

    fn inject_hid_event(&mut self, event: &HidEvent) -> Result<(), PalError> {
        if self.injector.is_none() {
            self.injector = Some(LinuxUinputInjector::new(self.shm.clone())?);
        }
        if let Some(ref mut inj) = self.injector {
            inj.inject(event)?;
        }
        Ok(())
    }

    fn release_all_stuck_keys(&mut self) -> Result<(), PalError> {
        if self.injector.is_none() {
            if let Ok(inj) = LinuxUinputInjector::new(self.shm.clone()) {
                self.injector = Some(inj);
            }
        }
        if let Some(ref mut inj) = self.injector {
            inj.release_all_stuck_keys()?;
        }
        Ok(())
    }

    fn emergency_panic_release(&mut self) -> Result<(), PalError> {
        self.stop_capture()?;
        let _ = self.release_all_stuck_keys();
        self.latch_panic_in_shm();
        Ok(())
    }
}

impl Drop for LinuxInputBackend {
    fn drop(&mut self) {
        let _ = self.stop_capture();
        self.close_passive_hotkey_listeners();
        let _ = self.release_all_stuck_keys();
        unsafe {
            let _ = libc::close(self.epoll_fd);
            if self.passive_epoll_fd >= 0 {
                let _ = libc::close(self.passive_epoll_fd);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(state: &mut TouchpadState, code: u16, down: bool, ms: u64) -> Vec<HidEvent> {
        let mut events = Vec::new();
        assert!(state.handle_key(code, down, ms * 1_000_000, &mut events));
        events
            .into_iter()
            .map(|ev| match ev {
                CapturedInputEvent::Hid(ev) => ev,
                _ => panic!("gesture emitted panic"),
            })
            .collect()
    }

    fn button(button: HidMouseButton, pressed: bool) -> HidEvent {
        HidEvent::MouseButton { button, pressed }
    }

    fn tap(state: &mut TouchpadState) {
        assert!(key(state, BTN_TOUCH, true, 1000).is_empty());
        assert_eq!(
            key(state, BTN_TOUCH, false, 1100),
            vec![button(HidMouseButton::Left, true)]
        );
    }

    #[test]
    fn tap_then_hold_drags_until_lift_without_extra_click() {
        let mut state = TouchpadState::default();
        tap(&mut state);
        assert!(key(&mut state, BTN_TOUCH, true, 1300).is_empty());
        state.tap_motion = 200;
        assert_eq!(
            key(&mut state, BTN_TOUCH, false, 3000),
            vec![button(HidMouseButton::Left, false)]
        );
        assert!(!state.dragging);
        assert_eq!(state.last_tap_release_ns, 0);
        assert!(key(&mut state, BTN_TOUCH, false, 3001).is_empty());
    }

    #[test]
    fn drag_time_boundary_is_280ms() {
        for (down_ms, expected_drag) in [(1380, true), (1381, false)] {
            let mut state = TouchpadState::default();
            tap(&mut state);
            let events = key(&mut state, BTN_TOUCH, true, down_ms);
            assert_eq!(state.dragging, expected_drag);
            assert_eq!(
                events,
                if expected_drag {
                    Vec::new()
                } else {
                    vec![button(HidMouseButton::Left, false)]
                }
            );
        }
    }

    #[test]
    fn single_tap_releases_after_timeout_without_another_device_event() {
        let mut state = TouchpadState::default();
        tap(&mut state);
        let mut events = Vec::new();
        state.advance_time(1_380_000_000, &mut events);
        assert!(events.is_empty());
        state.advance_time(1_381_000_000, &mut events);
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            CapturedInputEvent::Hid(HidEvent::MouseButton {
                button: HidMouseButton::Left,
                pressed: false
            })
        ));
        state.advance_time(2_000_000_000, &mut events);
        assert_eq!(events.len(), 1, "release must be emitted exactly once");
    }

    #[test]
    fn double_tap_still_produces_two_clicks_but_held_contact_does_not() {
        for (second_lift, expected_clicks) in [(1300, 2), (1600, 1)] {
            let mut state = TouchpadState::default();
            let mut events = key(&mut state, BTN_TOUCH, true, 1000);
            events.extend(key(&mut state, BTN_TOUCH, false, 1100));
            events.extend(key(&mut state, BTN_TOUCH, true, 1200));
            events.extend(key(&mut state, BTN_TOUCH, false, second_lift));
            let mut expected = Vec::new();
            for _ in 0..expected_clicks {
                expected.extend([
                    button(HidMouseButton::Left, true),
                    button(HidMouseButton::Left, false),
                ]);
            }
            assert_eq!(events, expected);
        }
    }

    #[test]
    fn sensor_jitter_does_not_cancel_a_short_tap_and_drag() {
        let mut state = TouchpadState {
            tolerance_x: 93,
            tolerance_y: 93,
            ..Default::default()
        };
        key(&mut state, BTN_TOUCH, true, 1000);
        for _ in 0..40 {
            state.observe_position(ABS_X, 1500);
            state.observe_position(ABS_X, 1503);
            state.observe_position(ABS_Y, 800);
            state.observe_position(ABS_Y, 804);
        }
        assert_eq!(key(&mut state, BTN_TOUCH, false, 1080).len(), 1);
        assert!(key(&mut state, BTN_TOUCH, true, 1200).is_empty());
        state.observe_position(ABS_X, 1800);
        state.observe_position(ABS_X, 2400);
        assert_eq!(
            key(&mut state, BTN_TOUCH, false, 2100),
            vec![button(HidMouseButton::Left, false)]
        );
    }

    #[test]
    fn excursion_beyond_physical_tap_tolerance_cannot_arm_drag() {
        let mut state = TouchpadState {
            tolerance_x: 93,
            tolerance_y: 93,
            ..Default::default()
        };
        key(&mut state, BTN_TOUCH, true, 1000);
        state.observe_position(ABS_X, 1000);
        state.observe_position(ABS_X, 1093);
        state.observe_position(ABS_X, 1000);
        assert!(key(&mut state, BTN_TOUCH, false, 1080).is_empty());
        assert!(key(&mut state, BTN_TOUCH, true, 1200).is_empty());
        assert!(!state.dragging);
    }

    #[test]
    fn long_or_moving_touch_cannot_arm_drag() {
        for (duration, motion) in [(181, 0), (100, 45), (100, 300)] {
            let mut state = TouchpadState::default();
            key(&mut state, BTN_TOUCH, true, 1000);
            state.tap_motion = motion;
            assert!(key(&mut state, BTN_TOUCH, false, 1000 + duration).is_empty());
            assert!(key(&mut state, BTN_TOUCH, true, 1250).is_empty());
            assert!(!state.dragging);
        }
    }

    #[test]
    fn two_finger_tap_right_clicks_without_arming_drag() {
        let mut state = TouchpadState::default();
        key(&mut state, BTN_TOOL_DOUBLETAP, true, 1000);
        key(&mut state, BTN_TOUCH, true, 1000);
        key(&mut state, BTN_TOOL_DOUBLETAP, false, 1090);
        assert_eq!(
            key(&mut state, BTN_TOUCH, false, 1100),
            vec![
                button(HidMouseButton::Right, true),
                button(HidMouseButton::Right, false)
            ]
        );
        assert!(key(&mut state, BTN_TOUCH, true, 1200).is_empty());
        assert!(!state.dragging);
    }

    #[test]
    fn adding_second_finger_releases_drag_without_spurious_tap() {
        let mut state = TouchpadState::default();
        tap(&mut state);
        key(&mut state, BTN_TOUCH, true, 1200);
        assert_eq!(
            key(&mut state, BTN_TOOL_DOUBLETAP, true, 1210),
            vec![button(HidMouseButton::Left, false)]
        );
        assert!(key(&mut state, BTN_TOUCH, false, 1250).is_empty());
        assert!(!state.dragging);
    }
}
