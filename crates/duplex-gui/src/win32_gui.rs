//! Native Win32 Desktop Graphical Control Center for Windows 11 (`#[cfg(target_os = "windows")]`).
//!
//! Creates a native Win32 control panel with embedded Space typography and 400ms
//! live telemetry polling from `ShmWatchdogBlock`, one-click mode transition pushbuttons,
//! local `duplexd.exe` launcher, browser dashboard launcher, and an emergency panic release button.

use crate::design;
use crate::{
    open_url_in_browser, request_mode, restart_local_daemon, run_embedded_network_setup,
    start_local_daemon, trigger_panic_release, TelemetrySnapshot,
};
use duplex_proto::OperationalMode;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, RECT};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemServices::SS_OWNERDRAW;
use windows::Win32::UI::Controls::{SetScrollInfo, DRAWITEMSTRUCT, ODS_FOCUS, ODS_SELECTED};
use windows::Win32::UI::WindowsAndMessaging::*;

const ID_BTN_MODE_IDLE: usize = 1001;
const ID_BTN_MODE_PC_KVM: usize = 1002;
const ID_BTN_MODE_PC_EXT: usize = 1003;
const ID_BTN_MODE_LAPTOP: usize = 1004;
const ID_BTN_START_DAEMON: usize = 1005;
const ID_BTN_OPEN_WEB: usize = 1006;
const ID_BTN_PANIC: usize = 1007;
const ID_BTN_SETUP_NETWORK: usize = 1008;
const ID_BTN_CHECK_UPDATE: usize = 1009;
const ID_BTN_THEME: usize = 1010;
const ID_HERO: usize = 1011;
const ID_DESCRIPTION: usize = 1012;
const ID_LINK: usize = 1013;
const ID_TELEMETRY: usize = 1014;
const ID_VERSION: usize = 1015;
const ID_TIMER_TELEMETRY: usize = 2001;

use core::ffi::c_void;
use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicU16, Ordering};
use std::sync::Mutex;

static TELEMETRY_HWND: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
static STATUS_HWND: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
static WEB_PORT_WIN: AtomicU16 = AtomicU16::new(48180);
static LAST_TELEMETRY_TEXT: Mutex<String> = Mutex::new(String::new());

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

static LIGHT_MODE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static MAIN_HWND: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
static BODY_FONT: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
static LABEL_FONT: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
static HERO_FONT: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());

fn palette() -> design::Palette {
    if LIGHT_MODE.load(Ordering::Relaxed) {
        design::LIGHT
    } else {
        design::DARK
    }
}
fn color(rgb: u32) -> COLORREF {
    COLORREF(design::colorref(rgb))
}

unsafe fn make_font(face: &str, size: i32) -> HFONT {
    CreateFontW(
        -size,
        0,
        0,
        0,
        if face == design::fonts::GDI_FACE_DOTO {
            700
        } else {
            400
        },
        0,
        0,
        0,
        DEFAULT_CHARSET.0 as u32,
        OUT_DEFAULT_PRECIS.0 as u32,
        CLIP_DEFAULT_PRECIS.0 as u32,
        CLEARTYPE_QUALITY.0 as u32,
        DEFAULT_PITCH.0 as u32,
        PCWSTR(wide_null(face).as_ptr()),
    )
}

unsafe fn update_telemetry_label() {
    let s = TelemetrySnapshot::read_current();
    let daemon_outdated = crate::diagnostics::different_daemon_build(&s);
    let hwnd = HWND(MAIN_HWND.load(Ordering::Relaxed));
    let mode = design::mode_spec(s.mode_code);
    if let Ok(w) = GetDlgItem(hwnd, ID_HERO as i32) {
        set_window_text_str(w, mode.glyph);
    }
    if let Ok(w) = GetDlgItem(hwnd, ID_DESCRIPTION as i32) {
        set_window_text_str(w, &format!("{}\r\n{}", mode.title, mode.description));
    }
    if let Ok(w) = GetDlgItem(hwnd, ID_LINK as i32) {
        let _ = InvalidateRect(w, None, false);
    }
    let upd = crate::updater::get_update_status();
    if let Ok(w) = GetDlgItem(hwnd, ID_VERSION as i32) {
        set_window_text_str(
            w,
            &format!(
                "V{} / {}",
                crate::updater::CURRENT_VERSION,
                if upd.restart_required {
                    "RESTART REQUIRED"
                } else if upd.update_available {
                    "UPDATE AVAILABLE"
                } else if daemon_outdated {
                    "DAEMON RESTART NEEDED"
                } else {
                    "CONTROL CENTER / 01"
                }
            ),
        );
    }
    let text = format!("LINK RTT / MIN · CURRENT · MAX        {:.0} / {:.0} / {:.0} µs\r\nINPUT / VIRTUAL DISPLAYS              {} / {}\r\nHID BATCHES / TX · RX                 {} / {}\r\nVIDEO FRAMES / TX · RX                {} / {}\r\nKEYFRAME RECOVERIES                   {}\r\nSOFTWARE                             v{} / {}",
        s.min_rtt_us, s.last_rtt_us, s.max_rtt_us,
        if s.input_grab_active { "ACTIVE" } else { "OFF" }, s.virtual_displays_active,
        design::fmt_count(s.tx_hid_packets), design::fmt_count(s.rx_hid_packets),
        design::fmt_count(s.tx_video_frames), design::fmt_count(s.rx_video_frames),
        s.pli_keyframes_requested, crate::updater::CURRENT_VERSION,
        if upd.restart_required { "RESTART REQUIRED" } else if upd.update_available { "UPDATE AVAILABLE" } else if daemon_outdated { "DAEMON RESTART NEEDED" } else if upd.last_error.is_some() { "CHECK FAILED" } else { "READY" });
    if let Ok(mut prev) = LAST_TELEMETRY_TEXT.lock() {
        if *prev != text {
            set_window_text_str(HWND(TELEMETRY_HWND.load(Ordering::Acquire)), &text);
            *prev = text;
            let _ = InvalidateRect(HWND(TELEMETRY_HWND.load(Ordering::Acquire)), None, false);
        }
    }
    for id in [
        ID_BTN_MODE_IDLE,
        ID_BTN_MODE_PC_KVM,
        ID_BTN_MODE_PC_EXT,
        ID_BTN_MODE_LAPTOP,
        ID_BTN_PANIC,
    ] {
        if let Ok(w) = GetDlgItem(hwnd, id as i32) {
            let _ = InvalidateRect(w, None, false);
        }
    }
}

unsafe fn draw_text(dc: HDC, text: &str, mut rect: RECT, rgb: u32, font: HFONT) {
    let previous = SelectObject(dc, font);
    let _ = SetTextColor(dc, color(rgb));
    let _ = SetBkMode(dc, TRANSPARENT);
    let mut text = wide_null(text);
    let len = text.len() - 1;
    let _ = DrawTextW(
        dc,
        &mut text[..len],
        &mut rect,
        DT_LEFT | DT_WORDBREAK | DT_NOPREFIX,
    );
    let _ = SelectObject(dc, previous);
}

unsafe fn clear_item(item: &DRAWITEMSTRUCT) {
    let bg = CreateSolidBrush(color(palette().black));
    let _ = FillRect(item.hDC, &item.rcItem, bg);
    let _ = DeleteObject(bg);
}

unsafe fn draw_link(item: &DRAWITEMSTRUCT) {
    clear_item(item);
    let p = palette();
    let s = TelemetrySnapshot::read_current();
    let font = HFONT(LABEL_FONT.load(Ordering::Relaxed));
    let (daemon, dl) = design::daemon_status(s.daemon_online);
    let (peer, pl) = design::peer_status(s.daemon_online, s.peer_connected);
    for (i, (label, value, level)) in [("LOCAL DAEMON", daemon, dl), ("WIRED PEER", peer, pl)]
        .iter()
        .enumerate()
    {
        let y = item.rcItem.top + i as i32 * 20;
        draw_text(
            item.hDC,
            label,
            RECT {
                left: 0,
                top: y,
                right: 140,
                bottom: y + 18,
            },
            p.text_secondary,
            font,
        );
        draw_text(
            item.hDC,
            value,
            RECT {
                left: 148,
                top: y,
                right: 300,
                bottom: y + 18,
            },
            level.color(&p),
            font,
        );
    }
    let age = if s.peer_connected {
        design::fmt_ms(s.peer_age_ms)
    } else {
        "—".into()
    };
    draw_text(
        item.hDC,
        &format!("HEARTBEAT  {age} / 250 MS"),
        RECT {
            left: 0,
            top: 56,
            right: 300,
            bottom: 74,
        },
        p.text_secondary,
        font,
    );
    let filled = design::heartbeat_filled_segments(s.peer_age_ms, s.peer_connected);
    for i in 0..design::HEARTBEAT_SEGMENTS {
        let brush = CreateSolidBrush(color(if i < filled {
            p.text_primary
        } else {
            p.segment_empty
        }));
        let _ = FillRect(
            item.hDC,
            &RECT {
                left: i as i32 * 12,
                top: 82,
                right: i as i32 * 12 + 8,
                bottom: 94,
            },
            brush,
        );
        let _ = DeleteObject(brush);
    }
}

unsafe fn draw_telemetry(item: &DRAWITEMSTRUCT) {
    clear_item(item);
    let p = palette();
    let s = TelemetrySnapshot::read_current();
    let metrics = [
        (
            "RTT / MIN · CUR · MAX",
            format!(
                "{:.0} / {:.0} / {:.0} µs",
                s.min_rtt_us, s.last_rtt_us, s.max_rtt_us
            ),
            "1 GBE / 50 HZ HEARTBEAT".to_string(),
        ),
        (
            "INPUT / DISPLAYS",
            format!(
                "{} / {}",
                if s.input_grab_active { "ACTIVE" } else { "OFF" },
                s.virtual_displays_active
            ),
            format!("DAEMON HB / {:.1} MS", s.daemon_age_ms),
        ),
        (
            "HID / TX · RX",
            format!(
                "{} / {}",
                design::fmt_count(s.tx_hid_packets),
                design::fmt_count(s.rx_hid_packets)
            ),
            "CHACHA20-POLY1305".into(),
        ),
        (
            "VIDEO / TX · RX",
            format!(
                "{} / {}",
                design::fmt_count(s.tx_video_frames),
                design::fmt_count(s.rx_video_frames)
            ),
            "NATIVE RES · LOSSLESS TEXT".into(),
        ),
        (
            "KEYFRAME RECOVERIES",
            design::fmt_count(s.pli_keyframes_requested),
            "PLI / GAP DETECTION".into(),
        ),
    ];
    let width = (item.rcItem.right - item.rcItem.left) / 5;
    for (i, (label, value, detail)) in metrics.iter().enumerate() {
        let left = i as i32 * width;
        draw_text(
            item.hDC,
            label,
            RECT {
                left,
                top: 0,
                right: left + width - 8,
                bottom: 20,
            },
            p.text_secondary,
            HFONT(LABEL_FONT.load(Ordering::Relaxed)),
        );
        draw_text(
            item.hDC,
            value,
            RECT {
                left,
                top: 24,
                right: left + width - 8,
                bottom: 60,
            },
            p.text_primary,
            HFONT(BODY_FONT.load(Ordering::Relaxed)),
        );
        draw_text(
            item.hDC,
            detail,
            RECT {
                left,
                top: 64,
                right: left + width - 8,
                bottom: 96,
            },
            p.text_disabled,
            HFONT(LABEL_FONT.load(Ordering::Relaxed)),
        );
    }
}

unsafe fn draw_button(item: &DRAWITEMSTRUCT) {
    let p = palette();
    let id = item.CtlID as usize;
    let s = TelemetrySnapshot::read_current();
    let mode_index = [
        ID_BTN_MODE_IDLE,
        ID_BTN_MODE_PC_KVM,
        ID_BTN_MODE_PC_EXT,
        ID_BTN_MODE_LAPTOP,
    ]
    .iter()
    .position(|v| *v == id);
    let mode = mode_index.map(|i| &design::MODES[i]);
    let active = mode.is_some_and(|m| m.code == s.mode_code && s.daemon_online);
    let pressed = item.itemState.0 & ODS_SELECTED.0 != 0;
    let danger = id == ID_BTN_PANIC && (s.input_grab_active || pressed);
    clear_item(item);
    let brush = CreateSolidBrush(color(if pressed {
        p.surface_raised
    } else if active {
        p.surface
    } else {
        p.black
    }));
    let pen = CreatePen(
        PS_SOLID,
        1,
        color(if danger {
            p.accent
        } else if active {
            p.text_display
        } else {
            p.border_visible
        }),
    );
    let previous_brush = SelectObject(item.hDC, brush);
    let previous_pen = SelectObject(item.hDC, pen);
    let _ = RoundRect(
        item.hDC,
        item.rcItem.left,
        item.rcItem.top,
        item.rcItem.right,
        item.rcItem.bottom,
        8,
        8,
    );
    let _ = SelectObject(item.hDC, previous_brush);
    let _ = SelectObject(item.hDC, previous_pen);
    let _ = DeleteObject(brush);
    let _ = DeleteObject(pen);
    let mut rect = item.rcItem;
    rect.left += 16;
    rect.right -= 16;
    rect.top += 12;
    rect.bottom -= 8;
    let mono = HFONT(LABEL_FONT.load(Ordering::Relaxed));
    if let Some(mode) = mode {
        let mut row = rect;
        row.bottom = row.top + 18;
        draw_text(
            item.hDC,
            &format!("MODE {}", mode.glyph),
            row,
            p.text_secondary,
            mono,
        );
        row.top += 24;
        row.bottom = row.top + 24;
        draw_text(
            item.hDC,
            mode.title,
            row,
            p.text_primary,
            HFONT(BODY_FONT.load(Ordering::Relaxed)),
        );
        row.top = rect.bottom - 16;
        row.bottom = rect.bottom;
        draw_text(item.hDC, mode.shortcut, row, p.text_secondary, mono);
        let dot = CreateSolidBrush(color(if active {
            p.text_display
        } else {
            p.text_disabled
        }));
        let _ = FillRect(
            item.hDC,
            &RECT {
                left: rect.right - 6,
                top: rect.top + 2,
                right: rect.right,
                bottom: rect.top + 8,
            },
            dot,
        );
        let _ = DeleteObject(dot);
    } else {
        let mut text = [0u16; 512];
        let n = GetWindowTextW(item.hwndItem, &mut text);
        let text = String::from_utf16_lossy(&text[..n as usize]).to_uppercase();
        draw_text(
            item.hDC,
            &text,
            rect,
            if danger { p.accent } else { p.text_primary },
            mono,
        );
    }
    if item.itemState.0 & ODS_FOCUS.0 != 0 {
        let _ = DrawFocusRect(item.hDC, &rect);
    }
}

#[allow(clippy::too_many_arguments)]
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
        WS_CHILD
            | WS_VISIBLE
            | style
            | if class_name == "BUTTON" {
                WS_TABSTOP
            } else {
                WINDOW_STYLE(0)
            },
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

    let hfont = HFONT(BODY_FONT.load(Ordering::Relaxed));
    let _ = SendMessageW(hwnd, WM_SETFONT, WPARAM(hfont.0 as usize), LPARAM(1));
    hwnd
}

static SCROLL_X: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static SCROLL_Y: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

unsafe fn scroll_to(hwnd: HWND, bar: SCROLLBAR_CONSTANTS, requested: i32) {
    let offset = if bar == SB_VERT { &SCROLL_Y } else { &SCROLL_X };
    let old = offset.load(Ordering::Relaxed);
    let info = SCROLLINFO {
        cbSize: core::mem::size_of::<SCROLLINFO>() as u32,
        fMask: SIF_POS,
        nPos: requested,
        ..Default::default()
    };
    let pos = SetScrollInfo(hwnd, bar, &info, true);
    offset.store(pos, Ordering::Relaxed);
    let delta = old - pos;
    let _ = ScrollWindowEx(
        hwnd,
        if bar == SB_HORZ { delta } else { 0 },
        if bar == SB_VERT { delta } else { 0 },
        None,
        None,
        HRGN::default(),
        None,
        SW_SCROLLCHILDREN | SW_INVALIDATE | SW_ERASE,
    );
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_CREATE => {
            MAIN_HWND.store(hwnd.0, Ordering::Relaxed);
            let hinst: HINSTANCE = GetModuleHandleW(None).unwrap_or_default().into();
            let body = make_font(design::fonts::GDI_FACE_GROTESK, 16);
            let label = make_font(design::fonts::GDI_FACE_MONO, 11);
            let hero = make_font(
                design::fonts::GDI_FACE_DOTO,
                design::type_scale::HERO_COMPACT as i32,
            );
            BODY_FONT.store(body.0, Ordering::Relaxed);
            LABEL_FONT.store(label.0, Ordering::Relaxed);
            HERO_FONT.store(hero.0, Ordering::Relaxed);
            let static_style = WINDOW_STYLE(0);
            let btn_style = WINDOW_STYLE(BS_OWNERDRAW as u32);
            create_child_control(
                hwnd,
                hinst,
                "STATIC",
                "duplex / kvm",
                static_style,
                40,
                20,
                600,
                32,
                0,
            );
            create_child_control(
                hwnd,
                hinst,
                "BUTTON",
                "DARK / LIGHT",
                btn_style,
                800,
                12,
                160,
                44,
                ID_BTN_THEME,
            );
            let version = create_child_control(
                hwnd,
                hinst,
                "STATIC",
                "CONTROL CENTER / 01",
                static_style,
                220,
                28,
                440,
                20,
                ID_VERSION,
            );
            let _ = SendMessageW(version, WM_SETFONT, WPARAM(label.0 as usize), LPARAM(1));
            let current = create_child_control(
                hwnd,
                hinst,
                "STATIC",
                "CURRENT MODE",
                static_style,
                40,
                76,
                240,
                20,
                0,
            );
            let _ = SendMessageW(current, WM_SETFONT, WPARAM(label.0 as usize), LPARAM(1));
            let glyph = create_child_control(
                hwnd,
                hinst,
                "STATIC",
                "—",
                static_style,
                40,
                96,
                132,
                96,
                ID_HERO,
            );
            let _ = SendMessageW(glyph, WM_SETFONT, WPARAM(hero.0 as usize), LPARAM(1));
            create_child_control(
                hwnd,
                hinst,
                "STATIC",
                "Connecting",
                static_style,
                188,
                112,
                390,
                96,
                ID_DESCRIPTION,
            );
            let link = create_child_control(
                hwnd,
                hinst,
                "STATIC",
                "CONNECTING",
                WINDOW_STYLE(SS_OWNERDRAW.0),
                660,
                80,
                300,
                104,
                ID_LINK,
            );
            let _ = SendMessageW(link, WM_SETFONT, WPARAM(label.0 as usize), LPARAM(1));
            create_child_control(
                hwnd,
                hinst,
                "STATIC",
                "01 / Routing",
                static_style,
                40,
                196,
                600,
                24,
                0,
            );
            for (index, mode) in design::MODES.iter().enumerate() {
                let id = [
                    ID_BTN_MODE_IDLE,
                    ID_BTN_MODE_PC_KVM,
                    ID_BTN_MODE_PC_EXT,
                    ID_BTN_MODE_LAPTOP,
                ][index];
                create_child_control(
                    hwnd,
                    hinst,
                    "BUTTON",
                    &format!("MODE {}\r\n{}\r\n{}", mode.glyph, mode.title, mode.shortcut),
                    btn_style,
                    40 + index as i32 * 234,
                    228,
                    218,
                    100,
                    id,
                );
            }
            create_child_control(
                hwnd,
                hinst,
                "STATIC",
                "02 / Telemetry",
                static_style,
                40,
                344,
                400,
                24,
                0,
            );
            let telemetry = create_child_control(
                hwnd,
                hinst,
                "STATIC",
                "[LOADING...]",
                WINDOW_STYLE(SS_OWNERDRAW.0),
                40,
                376,
                920,
                100,
                ID_TELEMETRY,
            );
            let _ = SendMessageW(telemetry, WM_SETFONT, WPARAM(label.0 as usize), LPARAM(1));
            TELEMETRY_HWND.store(telemetry.0, Ordering::Release);
            create_child_control(
                hwnd,
                hinst,
                "STATIC",
                "03 / System",
                static_style,
                40,
                492,
                400,
                24,
                0,
            );
            for (i, (id, title)) in [
                (ID_BTN_START_DAEMON, "Start / restart daemon"),
                (ID_BTN_SETUP_NETWORK, "Set up network"),
                (ID_BTN_OPEN_WEB, "Open browser UI"),
                (ID_BTN_CHECK_UPDATE, "Check updates"),
            ]
            .iter()
            .enumerate()
            {
                create_child_control(
                    hwnd,
                    hinst,
                    "BUTTON",
                    title,
                    btn_style,
                    40 + i as i32 * 234,
                    524,
                    218,
                    44,
                    *id,
                );
            }
            create_child_control(
                hwnd,
                hinst,
                "BUTTON",
                "Emergency release / Ctrl+Alt+Shift+Backspace",
                btn_style,
                40,
                580,
                920,
                44,
                ID_BTN_PANIC,
            );
            let status = create_child_control(
                hwnd,
                hinst,
                "STATIC",
                "[READY] / CTRL+ALT+P TO CYCLE MODES",
                static_style,
                40,
                632,
                920,
                20,
                0,
            );
            let _ = SendMessageW(status, WM_SETFONT, WPARAM(label.0 as usize), LPARAM(1));
            STATUS_HWND.store(status.0, Ordering::Release);
            update_telemetry_label();
            let _ = SetTimer(hwnd, ID_TIMER_TELEMETRY, 400, None);
            LRESULT(0)
        }
        WM_ERASEBKGND => {
            let mut rect = RECT::default();
            let _ = GetClientRect(hwnd, &mut rect);
            let brush = CreateSolidBrush(color(palette().black));
            let _ = FillRect(HDC(wparam.0 as *mut c_void), &rect, brush);
            let _ = DeleteObject(brush);
            LRESULT(1)
        }
        WM_CTLCOLORSTATIC => {
            let dc = HDC(wparam.0 as *mut c_void);
            let p = palette();
            let control = HWND(lparam.0 as *mut c_void);
            let id = GetDlgCtrlID(control);
            let rgb = if id == ID_VERSION as i32 {
                p.text_secondary
            } else if id == ID_HERO as i32 {
                p.text_display
            } else {
                p.text_primary
            };
            let _ = SetTextColor(dc, color(rgb));
            let _ = SetBkColor(dc, color(p.black));
            // DC_BRUSH is owned by Windows; no per-paint allocation.
            let _ = SetDCBrushColor(dc, color(p.black));
            LRESULT(GetStockObject(DC_BRUSH).0 as isize)
        }
        WM_DRAWITEM => {
            let item = &*(lparam.0 as *const DRAWITEMSTRUCT);
            match item.CtlID as usize {
                ID_LINK => draw_link(item),
                ID_TELEMETRY => draw_telemetry(item),
                _ => draw_button(item),
            }
            LRESULT(1)
        }
        WM_SIZE => {
            let mut rect = RECT::default();
            let _ = GetClientRect(hwnd, &mut rect);
            for (bar, max, page, offset) in [
                (SB_VERT, 667, rect.bottom, &SCROLL_Y),
                (SB_HORZ, 1000, rect.right, &SCROLL_X),
            ] {
                let old = offset.load(Ordering::Relaxed);
                let info = SCROLLINFO {
                    cbSize: core::mem::size_of::<SCROLLINFO>() as u32,
                    fMask: SIF_RANGE | SIF_PAGE,
                    nMin: 0,
                    nMax: max,
                    nPage: page.max(0) as u32,
                    ..Default::default()
                };
                SetScrollInfo(hwnd, bar, &info, true);
                scroll_to(hwnd, bar, old);
            }
            LRESULT(0)
        }
        WM_VSCROLL | WM_HSCROLL => {
            let bar = if msg == WM_VSCROLL { SB_VERT } else { SB_HORZ };
            let offset = if bar == SB_VERT { &SCROLL_Y } else { &SCROLL_X };
            let old = offset.load(Ordering::Relaxed);
            let mut info = SCROLLINFO {
                cbSize: core::mem::size_of::<SCROLLINFO>() as u32,
                fMask: SIF_ALL,
                ..Default::default()
            };
            let _ = GetScrollInfo(hwnd, bar, &mut info);
            let target = match (wparam.0 & 0xffff) as i32 {
                0 => old - 40,
                1 => old + 40,
                2 => old - info.nPage as i32,
                3 => old + info.nPage as i32,
                4 | 5 => info.nTrackPos,
                6 => 0,
                7 => info.nMax,
                _ => old,
            };
            scroll_to(hwnd, bar, target);
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = ((wparam.0 >> 16) as u16) as i16 as i32;
            scroll_to(hwnd, SB_VERT, SCROLL_Y.load(Ordering::Relaxed) - delta);
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
                ID_BTN_THEME => {
                    LIGHT_MODE.fetch_xor(true, Ordering::Relaxed);
                    design::save_light_appearance(LIGHT_MODE.load(Ordering::Relaxed));
                    let _ = RedrawWindow(
                        hwnd,
                        None,
                        None,
                        RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN,
                    );
                }
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
                    let res = if TelemetrySnapshot::read_current().daemon_online {
                        restart_local_daemon()
                    } else {
                        start_local_daemon()
                    };
                    set_status_text(&res.unwrap_or_else(|e| e));
                    update_telemetry_label();
                }
                ID_BTN_SETUP_NETWORK => {
                    let res = run_embedded_network_setup();
                    set_status_text(&res.unwrap_or_else(|e| e));
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
                ID_BTN_CHECK_UPDATE => {
                    set_status_text("Checking repository releases for updates...");
                    std::thread::spawn(|| {
                        let msg = match crate::updater::check_for_updates(true) {
                            Ok(status) => {
                                if status.update_available {
                                    format!(
                                        "Update available: {} (Use 'duplex-kvm update install' or Web UI to apply)",
                                        status.latest_version.unwrap_or_default()
                                    )
                                } else {
                                    format!("Duplex-KVM is up to date ({})", status.current_version)
                                }
                            }
                            Err(e) => format!("Update check failed: {e}"),
                        };
                        unsafe {
                            set_status_text(&msg);
                            update_telemetry_label();
                        }
                    });
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            for ptr in [&BODY_FONT, &LABEL_FONT, &HERO_FONT] {
                let _ = DeleteObject(HFONT(ptr.swap(ptr::null_mut(), Ordering::Relaxed)));
            }
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

pub fn run_native_win32_window(web_port: u16) -> bool {
    unsafe {
        WEB_PORT_WIN.store(web_port, Ordering::Release);
        LIGHT_MODE.store(design::load_light_appearance(), Ordering::Relaxed);
        let mut font_handles = Vec::new();
        for (_, bytes) in design::fonts::ALL {
            let mut count = 0u32;
            // The API writes the font count through this (const-declared) pointer.
            let handle = AddFontMemResourceEx(
                bytes.as_ptr().cast(),
                bytes.len() as u32,
                None,
                core::ptr::addr_of_mut!(count).cast_const(),
            );
            font_handles.push(handle);
        }
        let hinstance: HINSTANCE = match GetModuleHandleW(None) {
            Ok(h) => h.into(),
            Err(_) => return false,
        };

        let class_name = wide_null("DuplexKvmGuiWindowClass");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinstance,
            lpszClassName: PCWSTR(class_name.as_ptr()),
            hbrBackground: HBRUSH::default(),
            ..Default::default()
        };
        let _ = RegisterClassW(&wc);

        let title = wide_null("Duplex — Control center");
        let style = WS_OVERLAPPED
            | WS_CAPTION
            | WS_SYSMENU
            | WS_MINIMIZEBOX
            | WS_MAXIMIZEBOX
            | WS_SIZEBOX
            | WS_VSCROLL
            | WS_HSCROLL
            | WS_VISIBLE;
        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class_name.as_ptr()),
            PCWSTR(title.as_ptr()),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1020,
            720,
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
            if IsDialogMessageW(hwnd, &msg).as_bool() {
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        for handle in font_handles {
            let _ = RemoveFontMemResourceEx(handle);
        }
        true
    }
}
