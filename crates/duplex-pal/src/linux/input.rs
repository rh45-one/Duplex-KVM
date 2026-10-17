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

const SYN_REPORT: u16 = 0;

const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;

const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
const BTN_SIDE: u16 = 0x113;
const BTN_EXTRA: u16 = 0x114;

// Linux ioctl encodings for evdev ('E' = 0x45) and uinput ('U' = 0x55)
const EVIOCGID: libc::c_ulong = 0x8008_4502;
const EVIOCGRAB: libc::c_ulong = 0x4004_4590;

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

        Ok(Self {
            fd,
            path: path.to_owned(),
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
    shm: Option<Arc< crate::shm::ShmWatchdog >>,
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
            for ev in [EV_SYN as libc::c_int, EV_KEY as libc::c_int, EV_REL as libc::c_int] {
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

/// Complete Debian Linux `InputBackend` combining non-blocking `epoll` + `evdev` (`EVIOCGRAB`)
/// capture and `/dev/uinput` kernel injection.
pub struct LinuxInputBackend {
    epoll_fd: RawFd,
    grabbed_devices: Vec<GrabbedEvdevDevice>,
    injector: Option<LinuxUinputInjector>,
    panic_detector: PanicDetector,
    shm: Option<Arc<crate::shm::ShmWatchdog>>,
    rel_dx_accum: i16,
    rel_dy_accum: i16,
    rel_vwheel_accum: i16,
    rel_hwheel_accum: i16,
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

        // Attempt to open /dev/uinput injector eagerly (or lazily if running unprivileged in unit tests)
        let injector = match LinuxUinputInjector::new(shm.clone()) {
            Ok(inj) => Some(inj),
            Err(e) => {
                warn!("Could not initialize /dev/uinput at startup (will retry on inject): {e}");
                None
            }
        };

        Ok(Self {
            epoll_fd,
            grabbed_devices: Vec::new(),
            injector,
            panic_detector: PanicDetector::new(),
            shm,
            rel_dx_accum: 0,
            rel_dy_accum: 0,
            rel_vwheel_accum: 0,
            rel_hwheel_accum: 0,
        })
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
        if rc == 0
            && id.vendor == DUPLEX_UINPUT_VENDOR
            && id.product == DUPLEX_UINPUT_PRODUCT
        {
            unsafe { libc::close(fd) };
            return false; // Skip our own virtual uinput device!
        }

        let mut ev_bits = [0u8; 4];
        let has_ev = unsafe { libc::ioctl(fd, eviocgbit(0, ev_bits.len()), ev_bits.as_mut_ptr()) } >= 0;
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
        let has_esc = (key_bits[1 / 8] & (1 << (1 % 8))) != 0;
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
}

impl InputBackend for LinuxInputBackend {
    fn start_capture(&mut self) -> Result<(), PalError> {
        self.stop_capture()?;
        self.panic_detector.reset();

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
                        info!(device = %path, fd = dev.fd, "Acquired exclusive EVIOCGRAB on evdev device");
                        self.grabbed_devices.push(dev);
                    }
                }
                Err(e) => {
                    warn!(device = %path, error = %e, "Skipping evdev device grab");
                }
            }
        }

        if let Some(ref shm) = self.shm {
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
        if let Some(ref shm) = self.shm {
            shm.block()
                .input_grab_active
                .store(false, Ordering::Release);
        }
        Ok(())
    }

    fn poll_captured_events(
        &mut self,
        out: &mut Vec<CapturedInputEvent>,
    ) -> Result<usize, PalError> {
        if self.grabbed_devices.is_empty() {
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
            return Ok(0);
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
                    match ev.type_ {
                        EV_KEY => {
                            // Ignore value == 2 (kernel key auto-repeat) for wire normalization
                            if ev.value != 0 && ev.value != 1 {
                                continue;
                            }
                            let pressed = ev.value == 1;

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
                                    warn!(?trigger_kind, "HARDWARE PANIC ESCAPE HATCH TRIPPED ON LINUX EVDEV");
                                    self.emergency_panic_release()?;
                                    // Flush remote modifiers on the peer before disconnecting to Mode0Idle
                                    out.push(CapturedInputEvent::Hid(
                                        HidEvent::ReleaseAllModifiers,
                                    ));
                                    out.push(CapturedInputEvent::PanicTriggered(trigger_kind));
                                    return Ok(pushed + 2);
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

        Ok(pushed)
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
        let _ = self.release_all_stuck_keys();
        unsafe {
            let _ = libc::close(self.epoll_fd);
        }
    }
}
