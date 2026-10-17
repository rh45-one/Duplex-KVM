//! Native Win32 Desktop Graphical Control Center for Windows 11 (`#[cfg(target_os = "windows")]`).
//!
//! Creates a native Win32 control panel window with `Segoe UI` typography, 10 Hz (`100ms` `SetTimer`)
//! live telemetry polling from `ShmWatchdogBlock`, one-click mode transition pushbuttons,
//! local `duplexd.exe` launcher, browser dashboard launcher, and an emergency panic release button.

use crate::{
    open_url_in_browser, request_mode, start_local_daemon, trigger_panic_release, TelemetrySnapshot,
};
use duplex_proto::OperationalMode;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetStockObject, COLOR_BTNFACE, DEFAULT_GUI_FONT, HBRUSH};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, PostQuitMessage,
    RegisterClassW, SendMessageW, SetTimer, SetWindowTextW, ShowWindow, TranslateMessage,
    BS_PUSHBUTTON, CW_USEDEFAULT, HMENU, MSG, SW_SHOW, WINDOW_EX_STYLE,
    WINDOW_STYLE, WM_COMMAND, WM_CREATE, WM_DESTROY, WM_SETFONT, WM_TIMER, WNDCLASSW, WS_CHILD,
    WS_OVERLAPPED, WS_SYSMENU, WS_MINIMIZEBOX, WS_CAPTION, WS_VISIBLE,
};

const ID_BTN_MODE_IDLE: usize = 1001;
const ID_BTN_MODE_PC_KVM: usize = 1002;
const ID_BTN_MODE_PC_EXT: usize = 1003;
const ID_BTN_MODE_LAPTOP: usize = 1004;
const ID_BTN_START_DAEMON: usize = 1005;
const ID_BTN_OPEN_WEB: usize = 1006;
const ID_BTN_PANIC: usize = 1007;
const ID_TIMER_TELEMETRY: usize = 2001;

use core::ffi::c_void;
use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicU16, Ordering};

static TELEMETRY_HWND: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
static STATUS_HWND: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
static WEB_PORT_WIN: AtomicU16 = AtomicU16::new(48180);

fn wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(core::iter::once(0)).collect()
}

unsafe fn set_window_text_str(hwnd: HWND, text: &str) {
    if !hwnd.0.is_null() {
        let w = wide_null(text);
        let _ = SetWindowTextW(hwnd, PCWSTR(w.as_ptr()));
    }
}

unsafe fn set_status_text(text: &str) {
    let hwnd = HWND(STATUS_HWND.load(Ordering::Acquire));
    set_window_text_str(hwnd, text);
}

unsafe fn update_telemetry_label() {
    let snap = TelemetrySnapshot::read_current();
    let daemon_str = if snap.daemon_online {
        format!("ONLINE (PID {})", snap.daemon_pid)
    } else {
        "OFFLINE (Click 'Start Local Daemon' below)".to_string()
    };
    let peer_str = if snap.peer_connected {
        "CONNECTED (1GbE Active)"
    } else {
        "STANDBY / WAITING FOR PEER"
    };
    let grab_str = if snap.input_grab_active { "ACTIVE" } else { "OFF" };

    let text = format!(
        "{}\r\n{}\r\n\r\n\
         Daemon Status        : {}\r\n\
         Peer 1GbE Link       : {}\r\n\
         Input Grab & Disp    : {}  |  Virtual Displays: {}\r\n\
         Heartbeat Age        : Daemon: {:.1} ms  |  Peer: {:.1} ms\r\n\
         Link RTT (min/cur/max): {:.1} us / {:.1} us / {:.1} us\r\n\
         HID Batches (TX/RX)  : {} / {}  (UDP 48100 DSCP EF 0xB8)\r\n\
         Video Frames (TX/RX) : {} / {}  (UDP 48102-3 DSCP AF41 0x88)\r\n\
         PLI Keyframe Recover : {}",
        snap.mode_label,
        snap.mode_subtitle,
        daemon_str,
        peer_str,
        grab_str,
        snap.virtual_displays_active,
        snap.daemon_age_ms,
        snap.peer_age_ms,
        snap.min_rtt_us,
        snap.last_rtt_us,
        snap.max_rtt_us,
        snap.tx_hid_packets,
        snap.rx_hid_packets,
        snap.tx_video_frames,
        snap.rx_video_frames,
        snap.pli_keyframes_requested,
    );

    let hwnd = HWND(TELEMETRY_HWND.load(Ordering::Acquire));
    set_window_text_str(hwnd, &text);
}

unsafe fn create_child_control(
    parent: HWND,
    hinstance: HINSTANCE,
    class_name: &str,
    label: &str,
    style: WINDOW_STYLE,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    id: usize,
) -> HWND {
    let cls_w = wide_null(class_name);
    let lbl_w = wide_null(label);
    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        PCWSTR(cls_w.as_ptr()),
        PCWSTR(lbl_w.as_ptr()),
        WS_CHILD | WS_VISIBLE | style,
        x,
        y,
        w,
        h,
        parent,
        HMENU(id as *mut c_void),
        hinstance,
        None,
    )
    .unwrap_or_default();

    let hfont = GetStockObject(DEFAULT_GUI_FONT);
    let _ = SendMessageW(hwnd, WM_SETFONT, WPARAM(hfont.0 as usize), LPARAM(1));
    hwnd
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_CREATE => {
            let hinst: HINSTANCE = GetModuleHandleW(None).unwrap_or_default().into();
            let port = WEB_PORT_WIN.load(Ordering::Relaxed);

            let tel_hwnd = create_child_control(
                hwnd,
                hinst,
                "STATIC",
                "Initializing Duplex-KVM Telemetry...",
                WINDOW_STYLE(0),
                18,
                16,
                590,
                195,
                0,
            );
            TELEMETRY_HWND.store(tel_hwnd.0, Ordering::Release);

            let btn_style = WINDOW_STYLE(BS_PUSHBUTTON as u32);
            let _ = create_child_control(
                hwnd,
                hinst,
                "BUTTON",
                "Mode 0 - Idle (Local Only + 1GbE Internet Passthrough)",
                btn_style,
                18,
                222,
                590,
                34,
                ID_BTN_MODE_IDLE,
            );
            let _ = create_child_control(
                hwnd,
                hinst,
                "BUTTON",
                "Mode 1 - PC Primary (Laptop Controls Windows PC, KVM Input Only)",
                btn_style,
                18,
                262,
                590,
                34,
                ID_BTN_MODE_PC_KVM,
            );
            let _ = create_child_control(
                hwnd,
                hinst,
                "BUTTON",
                "Mode 1+ - PC Primary (Laptop Controls PC + Extend 1 Virtual Display)",
                btn_style,
                18,
                302,
                590,
                34,
                ID_BTN_MODE_PC_EXT,
            );
            let _ = create_child_control(
                hwnd,
                hinst,
                "BUTTON",
                "Mode 2 - Laptop Primary (Dock Windows Dual Monitors & Peripherals to Laptop)",
                btn_style,
                18,
                342,
                590,
                34,
                ID_BTN_MODE_LAPTOP,
            );

            let _ = create_child_control(
                hwnd,
                hinst,
                "BUTTON",
                "Start Local Daemon (duplexd.exe)",
                btn_style,
                18,
                390,
                288,
                34,
                ID_BTN_START_DAEMON,
            );
            let _ = create_child_control(
                hwnd,
                hinst,
                "BUTTON",
                &format!("Open Browser UI (127.0.0.1:{port})"),
                btn_style,
                320,
                390,
                288,
                34,
                ID_BTN_OPEN_WEB,
            );

            let _ = create_child_control(
                hwnd,
                hinst,
                "BUTTON",
                "EMERGENCY PANIC / UNGRAB ALL (Ctrl+Alt+Shift+Backspace)",
                btn_style,
                18,
                434,
                590,
                38,
                ID_BTN_PANIC,
            );

            let stat_hwnd = create_child_control(
                hwnd,
                hinst,
                "STATIC",
                &format!("Ready - Web UI also active at http://127.0.0.1:{port}"),
                WINDOW_STYLE(0),
                18,
                482,
                590,
                24,
                0,
            );
            STATUS_HWND.store(stat_hwnd.0, Ordering::Release);

            update_telemetry_label();
            let _ = SetTimer(hwnd, ID_TIMER_TELEMETRY, 100, None);
            LRESULT(0)
        }
        WM_TIMER => {
            if wparam.0 == ID_TIMER_TELEMETRY {
                update_telemetry_label();
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            let cmd_id = wparam.0 & 0xFFFF;
            match cmd_id {
                ID_BTN_MODE_IDLE => {
                    let res = request_mode(OperationalMode::Mode0Idle);
                    set_status_text(&res.unwrap_or_else(|e| e));
                    update_telemetry_label();
                }
                ID_BTN_MODE_PC_KVM => {
                    let res = request_mode(OperationalMode::Mode1PcPrimary {
                        extend_display: false,
                    });
                    set_status_text(&res.unwrap_or_else(|e| e));
                    update_telemetry_label();
                }
                ID_BTN_MODE_PC_EXT => {
                    let res = request_mode(OperationalMode::Mode1PcPrimary {
                        extend_display: true,
                    });
                    set_status_text(&res.unwrap_or_else(|e| e));
                    update_telemetry_label();
                }
                ID_BTN_MODE_LAPTOP => {
                    let res = request_mode(OperationalMode::Mode2LaptopPrimary);
                    set_status_text(&res.unwrap_or_else(|e| e));
                    update_telemetry_label();
                }
                ID_BTN_START_DAEMON => {
                    let res = start_local_daemon();
                    set_status_text(&res.unwrap_or_else(|e| e));
                    update_telemetry_label();
                }
                ID_BTN_OPEN_WEB => {
                    let port = WEB_PORT_WIN.load(Ordering::Relaxed);
                    let url = format!("http://127.0.0.1:{port}");
                    open_url_in_browser(&url);
                    set_status_text(&format!("Opened Web Dashboard at {url}"));
                }
                ID_BTN_PANIC => {
                    let res = trigger_panic_release();
                    set_status_text(&res.unwrap_or_else(|e| e));
                    update_telemetry_label();
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

pub fn run_native_win32_window(web_port: u16) -> bool {
    unsafe {
        WEB_PORT_WIN.store(web_port, Ordering::Release);
        let hinstance: HINSTANCE = match GetModuleHandleW(None) {
            Ok(h) => h.into(),
            Err(_) => return false,
        };

        let class_name = wide_null("DuplexKvmGuiWindowClass");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinstance,
            lpszClassName: PCWSTR(class_name.as_ptr()),
            hbrBackground: HBRUSH((COLOR_BTNFACE.0 + 1) as *mut core::ffi::c_void),
            ..Default::default()
        };
        let _ = RegisterClassW(&wc);

        let title = wide_null("Duplex-KVM - Graphical Control & Telemetry Center");
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_VISIBLE;
        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class_name.as_ptr()),
            PCWSTR(title.as_ptr()),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            640,
            555,
            HWND(core::ptr::null_mut()),
            HMENU(core::ptr::null_mut()),
            hinstance,
            None,
        ) {
            Ok(h) if !h.0.is_null() => h,
            _ => return false,
        };

        let _ = ShowWindow(hwnd, SW_SHOW);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, HWND(core::ptr::null_mut()), 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        true
    }
}
