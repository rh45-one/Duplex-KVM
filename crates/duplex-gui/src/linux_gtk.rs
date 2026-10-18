//! Native GTK+ 3 Wayland/X11 Graphical Control Center for Debian Linux (`#[cfg(target_os = "linux")]`).
//!
//! Uses direct FFI bindings to `libgtk-3`, `libgdk-3`, `libgobject-2.0`, and `libglib-2.0` so that
//! `duplex-gui` compiles in <1s with zero external crate downloads and runs natively on both
//! Wayland (GNOME/KDE) and X11 sessions.

use crate::{
    open_url_in_browser, request_mode, run_embedded_network_setup, start_local_daemon,
    trigger_panic_release, TelemetrySnapshot,
};
use core::ffi::{c_char, c_int, c_uint, c_ulong, c_void};
use core::ptr;
use duplex_proto::OperationalMode;
use std::ffi::CString;

const GTK_WINDOW_TOPLEVEL: c_int = 0;
const GTK_ORIENTATION_HORIZONTAL: c_int = 0;
const GTK_ORIENTATION_VERTICAL: c_int = 1;
const GTK_STYLE_PROVIDER_PRIORITY_APPLICATION: c_uint = 600;

#[link(name = "gtk-3")]
#[link(name = "gdk-3")]
#[link(name = "gobject-2.0")]
#[link(name = "glib-2.0")]
extern "C" {
    fn gtk_init_check(argc: *mut c_int, argv: *mut *mut *mut c_char) -> c_int;
    fn gtk_main();
    fn gtk_main_quit();

    fn gtk_window_new(type_: c_int) -> *mut c_void;
    fn gtk_window_set_title(window: *mut c_void, title: *const c_char);
    fn gtk_window_set_default_size(window: *mut c_void, width: c_int, height: c_int);
    fn gtk_window_set_position(window: *mut c_void, position: c_int);

    fn gtk_box_new(orientation: c_int, spacing: c_int) -> *mut c_void;
    fn gtk_box_pack_start(
        box_: *mut c_void,
        child: *mut c_void,
        expand: c_int,
        fill: c_int,
        padding: c_uint,
    );

    fn gtk_container_add(container: *mut c_void, widget: *mut c_void);
    fn gtk_container_set_border_width(container: *mut c_void, border_width: c_uint);

    fn gtk_label_new(str: *const c_char) -> *mut c_void;
    fn gtk_label_set_markup(label: *mut c_void, str: *const c_char);
    fn gtk_label_set_xalign(label: *mut c_void, xalign: f32);

    fn gtk_button_new_with_label(label: *const c_char) -> *mut c_void;
    fn gtk_separator_new(orientation: c_int) -> *mut c_void;

    fn gtk_widget_show_all(widget: *mut c_void);
    fn gtk_widget_set_name(widget: *mut c_void, name: *const c_char);
    fn gtk_widget_set_size_request(widget: *mut c_void, width: c_int, height: c_int);

    fn gtk_css_provider_new() -> *mut c_void;
    fn gtk_css_provider_load_from_data(
        css_provider: *mut c_void,
        data: *const c_char,
        length: isize,
        error: *mut *mut c_void,
    ) -> c_int;
    fn gdk_screen_get_default() -> *mut c_void;
    fn gtk_style_context_add_provider_for_screen(
        screen: *mut c_void,
        provider: *mut c_void,
        priority: c_uint,
    );

    fn g_signal_connect_data(
        instance: *mut c_void,
        detailed_signal: *const c_char,
        c_handler: unsafe extern "C" fn(),
        data: *mut c_void,
        destroy_data: *const c_void,
        connect_flags: c_int,
    ) -> c_ulong;

    fn g_timeout_add(
        interval: c_uint,
        function: unsafe extern "C" fn(*mut c_void) -> c_int,
        data: *mut c_void,
    ) -> c_uint;
}

const DARK_CSS: &str = "
window {
    background-color: #0f172a;
    color: #f8fafc;
}
label {
    color: #f8fafc;
}
button {
    background-image: none;
    background-color: #1e293b;
    color: #f8fafc;
    border: 1px solid #334155;
    border-radius: 8px;
    padding: 10px 14px;
    font-weight: bold;
}
button:hover {
    background-color: #293850;
    border-color: #38bdf8;
}
button:active {
    background-color: #0284c7;
    border-color: #7dd3fc;
}
#btn-start {
    background-color: #14532d;
    border-color: #22c55e;
}
#btn-start:hover {
    background-color: #166534;
}
#btn-panic {
    background-color: #7f1d1d;
    border-color: #ef4444;
    color: #ffffff;
}
#btn-panic:hover {
    background-color: #991b1b;
}
";

use core::sync::atomic::{AtomicPtr, AtomicU16, AtomicU64, Ordering};
use duplex_pal::monotonic_now_ns;
use std::sync::Mutex;

static TELEMETRY_LABEL_PTR: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
static STATUS_LABEL_PTR: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
static WEB_PORT_LINUX: AtomicU16 = AtomicU16::new(48180);
static LAST_BTN_ACTION_NS: AtomicU64 = AtomicU64::new(0);
static LAST_TELEMETRY_MARKUP: Mutex<String> = Mutex::new(String::new());

fn allow_button_action() -> bool {
    let now = monotonic_now_ns();
    let prev = LAST_BTN_ACTION_NS.load(Ordering::Acquire);
    if now.saturating_sub(prev) < 250_000_000 {
        return false;
    }
    LAST_BTN_ACTION_NS.store(now, Ordering::Release);
    true
}

fn set_status_banner(msg: &str) {
    unsafe {
        let status_label = STATUS_LABEL_PTR.load(Ordering::Acquire);
        if !status_label.is_null() {
            let escaped = msg
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;");
            let markup = format!("<span foreground=\"#38bdf8\" weight=\"bold\">{escaped}</span>");
            if let Ok(cstr) = CString::new(markup) {
                gtk_label_set_markup(status_label, cstr.as_ptr());
            }
        }
    }
}

unsafe extern "C" fn on_window_destroy() {
    gtk_main_quit();
}

unsafe extern "C" fn on_mode_idle(_widget: *mut c_void, _data: *mut c_void) {
    if !allow_button_action() {
        return;
    }
    let res = request_mode(OperationalMode::Mode0Idle);
    set_status_banner(&res.unwrap_or_else(|e| e));
    on_telemetry_tick(ptr::null_mut());
}

unsafe extern "C" fn on_mode_pc_kvm(_widget: *mut c_void, _data: *mut c_void) {
    if !allow_button_action() {
        return;
    }
    let res = request_mode(OperationalMode::Mode1PcPrimary {
        extend_display: false,
    });
    set_status_banner(&res.unwrap_or_else(|e| e));
    on_telemetry_tick(ptr::null_mut());
}

unsafe extern "C" fn on_mode_pc_extend(_widget: *mut c_void, _data: *mut c_void) {
    if !allow_button_action() {
        return;
    }
    let res = request_mode(OperationalMode::Mode1PcPrimary {
        extend_display: true,
    });
    set_status_banner(&res.unwrap_or_else(|e| e));
    on_telemetry_tick(ptr::null_mut());
}

unsafe extern "C" fn on_mode_laptop(_widget: *mut c_void, _data: *mut c_void) {
    if !allow_button_action() {
        return;
    }
    let res = request_mode(OperationalMode::Mode2LaptopPrimary);
    set_status_banner(&res.unwrap_or_else(|e| e));
    on_telemetry_tick(ptr::null_mut());
}

unsafe extern "C" fn on_start_daemon(_widget: *mut c_void, _data: *mut c_void) {
    if !allow_button_action() {
        return;
    }
    let res = start_local_daemon();
    set_status_banner(&res.unwrap_or_else(|e| e));
    on_telemetry_tick(ptr::null_mut());
}

unsafe extern "C" fn on_setup_network(_widget: *mut c_void, _data: *mut c_void) {
    if !allow_button_action() {
        return;
    }
    let res = run_embedded_network_setup();
    set_status_banner(&res.unwrap_or_else(|e| e));
}

unsafe extern "C" fn on_open_web(_widget: *mut c_void, _data: *mut c_void) {
    if !allow_button_action() {
        return;
    }
    let port = WEB_PORT_LINUX.load(Ordering::Relaxed);
    let url = format!("http://127.0.0.1:{port}");
    open_url_in_browser(&url);
    set_status_banner(&format!("Opened Web Dashboard at {url}"));
}

unsafe extern "C" fn on_panic(_widget: *mut c_void, _data: *mut c_void) {
    if !allow_button_action() {
        return;
    }
    let res = trigger_panic_release();
    set_status_banner(&res.unwrap_or_else(|e| e));
    on_telemetry_tick(ptr::null_mut());
}

unsafe extern "C" fn on_telemetry_tick(_data: *mut c_void) -> c_int {
    let telemetry_label = TELEMETRY_LABEL_PTR.load(Ordering::Acquire);
    if !telemetry_label.is_null() {
        let snap = TelemetrySnapshot::read_current();
        let daemon_badge = if snap.daemon_online {
            format!(
                "<span foreground=\"#4ade80\" weight=\"bold\">ONLINE (PID {})</span>",
                snap.daemon_pid
            )
        } else {
            "<span foreground=\"#f87171\" weight=\"bold\">OFFLINE (Click 'Start Local Daemon' below)</span>"
                .to_string()
        };

        let peer_badge = if snap.peer_connected {
            "<span foreground=\"#4ade80\" weight=\"bold\">CONNECTED (1GbE Active)</span>"
        } else {
            "<span foreground=\"#fbbf24\" weight=\"bold\">STANDBY / WAITING FOR PEER</span>"
        };

        let grab_badge = if snap.input_grab_active {
            "<span foreground=\"#38bdf8\" weight=\"bold\">ACTIVE</span>"
        } else {
            "OFF"
        };

        let safe_mode_label = snap
            .mode_label
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        let safe_mode_sub = snap
            .mode_subtitle
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");

        let markup = format!(
            "<span size=\"large\" weight=\"bold\" foreground=\"#38bdf8\">{mode_label}</span>\n\
             <span foreground=\"#94a3b8\">{mode_sub}</span>\n\n\
             <tt>Daemon Status       : </tt>{daemon_badge}\n\
             <tt>Peer 1GbE Link      : </tt>{peer_badge}\n\
             <tt>Input Grab &amp; Disp   : </tt>{grab_badge}  |  Virtual Displays: <b>{vdisp}</b>\n\
             <tt>Global Mode Hotkey  : </tt><b>Ctrl+Alt+P</b> (Cycle)  |  <b>Ctrl+Alt+0..3</b> (Direct)\n\
             <tt>Link RTT (min/cur/max): </tt><b>{min_rtt:.0} µs</b> / <b>{cur_rtt:.0} µs</b> / <b>{max_rtt:.0} µs</b>\n\
             <tt>HID Batches (TX/RX) : </tt><b>{tx_hid}</b> / <b>{rx_hid}</b>  <span foreground=\"#94a3b8\">(UDP 48100 DSCP EF 0xB8)</span>\n\
             <tt>Video Frames (TX/RX): </tt><b>{tx_vid}</b> / <b>{rx_vid}</b>  <span foreground=\"#94a3b8\">(UDP 48102-3 DSCP AF41 0x88)</span>\n\
             <tt>PLI Keyframe Recover: </tt><b>{pli}</b>",
            mode_label = safe_mode_label,
            mode_sub = safe_mode_sub,
            vdisp = snap.virtual_displays_active,
            min_rtt = snap.min_rtt_us,
            cur_rtt = snap.last_rtt_us,
            max_rtt = snap.max_rtt_us,
            tx_hid = snap.tx_hid_packets,
            rx_hid = snap.rx_hid_packets,
            tx_vid = snap.tx_video_frames,
            rx_vid = snap.rx_video_frames,
            pli = snap.pli_keyframes_requested,
        );

        let should_update = if let Ok(mut prev) = LAST_TELEMETRY_MARKUP.lock() {
            if *prev != markup {
                *prev = markup.clone();
                true
            } else {
                false
            }
        } else {
            true
        };

        if should_update {
            if let Ok(cstr) = CString::new(markup) {
                gtk_label_set_markup(telemetry_label, cstr.as_ptr());
            }
        }
    }
    1 // G_SOURCE_CONTINUE
}

/// Connects both `"pressed"` (fires immediately on mouse-down / touchpad tap so periodic label updates
/// can never cancel a click in progress) and `"clicked"` (for keyboard Space/Enter activation),
/// guarded by `allow_button_action()` 250ms debounce.
unsafe fn connect_button_action(
    button: *mut c_void,
    callback: unsafe extern "C" fn(*mut c_void, *mut c_void),
) {
    let cb_ptr: unsafe extern "C" fn() = core::mem::transmute(callback);
    let sig_pressed = CString::new("pressed").unwrap();
    g_signal_connect_data(
        button,
        sig_pressed.as_ptr(),
        cb_ptr,
        ptr::null_mut(),
        ptr::null(),
        0,
    );
    let sig_clicked = CString::new("clicked").unwrap();
    g_signal_connect_data(
        button,
        sig_clicked.as_ptr(),
        cb_ptr,
        ptr::null_mut(),
        ptr::null(),
        0,
    );
}

/// Attempts to launch the native GTK3 desktop window. Returns `false` if no Wayland/X11 display is available.
pub fn run_native_gtk_window(web_port: u16) -> bool {
    unsafe {
        if gtk_init_check(ptr::null_mut(), ptr::null_mut()) == 0 {
            return false;
        }

        // Apply dark theme stylesheet
        let provider = gtk_css_provider_new();
        if let Ok(css_cstr) = CString::new(DARK_CSS) {
            gtk_css_provider_load_from_data(
                provider,
                css_cstr.as_ptr(),
                DARK_CSS.len() as isize,
                ptr::null_mut(),
            );
        }
        let screen = gdk_screen_get_default();
        if !screen.is_null() {
            gtk_style_context_add_provider_for_screen(
                screen,
                provider,
                GTK_STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }

        let win = gtk_window_new(GTK_WINDOW_TOPLEVEL);
        let title = CString::new("Duplex-KVM — Graphical Control & Telemetry Center").unwrap();
        gtk_window_set_title(win, title.as_ptr());
        gtk_window_set_default_size(win, 680, 580);
        gtk_window_set_position(win, 1); // GTK_WIN_POS_CENTER
        gtk_container_set_border_width(win, 18);

        let destroy_sig = CString::new("destroy").unwrap();
        g_signal_connect_data(
            win,
            destroy_sig.as_ptr(),
            on_window_destroy,
            ptr::null_mut(),
            ptr::null(),
            0,
        );

        let root_vbox = gtk_box_new(GTK_ORIENTATION_VERTICAL, 12);
        gtk_container_add(win, root_vbox);

        // Header
        let header_lbl = gtk_label_new(ptr::null());
        let header_markup = CString::new(
            "<span size=\"x-large\" weight=\"bold\">Duplex-KVM Control Center</span>\n\
             <span foreground=\"#94a3b8\">Shortcut: <b>Ctrl+Alt+P</b> to Cycle Modes  |  <b>Ctrl+Alt+0..3</b> Direct Mode Select</span>",
        )
        .unwrap();
        gtk_label_set_markup(header_lbl, header_markup.as_ptr());
        gtk_label_set_xalign(header_lbl, 0.0);
        gtk_box_pack_start(root_vbox, header_lbl, 0, 0, 2);

        gtk_box_pack_start(
            root_vbox,
            gtk_separator_new(GTK_ORIENTATION_HORIZONTAL),
            0,
            0,
            4,
        );

        // Live Telemetry Panel (fixed minimum size prevents layout reallocation from interrupting button clicks)
        let telemetry_lbl = gtk_label_new(ptr::null());
        gtk_label_set_xalign(telemetry_lbl, 0.0);
        gtk_widget_set_size_request(telemetry_lbl, 630, 210);
        gtk_box_pack_start(root_vbox, telemetry_lbl, 0, 0, 4);

        gtk_box_pack_start(
            root_vbox,
            gtk_separator_new(GTK_ORIENTATION_HORIZONTAL),
            0,
            0,
            4,
        );

        // Mode Switcher Buttons
        let btn_m0 = gtk_button_new_with_label(
            CString::new("Mode 0 — Idle (Local Only + 1GbE Passthrough)  [Ctrl+Alt+0]")
                .unwrap()
                .as_ptr(),
        );
        connect_button_action(btn_m0, on_mode_idle);
        gtk_box_pack_start(root_vbox, btn_m0, 0, 1, 2);

        let btn_m1_kvm = gtk_button_new_with_label(
            CString::new("Mode 1 — PC Primary (Laptop Controls Windows PC, Input Only)  [Ctrl+Alt+1]")
                .unwrap()
                .as_ptr(),
        );
        connect_button_action(btn_m1_kvm, on_mode_pc_kvm);
        gtk_box_pack_start(root_vbox, btn_m1_kvm, 0, 1, 2);

        let btn_m1_ext = gtk_button_new_with_label(
            CString::new("Mode 1+ — PC Primary (Laptop Controls PC + Extend Display)  [Ctrl+Alt+2]")
                .unwrap()
                .as_ptr(),
        );
        connect_button_action(btn_m1_ext, on_mode_pc_extend);
        gtk_box_pack_start(root_vbox, btn_m1_ext, 0, 1, 2);

        let btn_m2 = gtk_button_new_with_label(
            CString::new("Mode 2 — Laptop Primary (Dock Windows Dual Monitors to Laptop)  [Ctrl+Alt+3]")
                .unwrap()
                .as_ptr(),
        );
        connect_button_action(btn_m2, on_mode_laptop);
        gtk_box_pack_start(root_vbox, btn_m2, 0, 1, 2);

        gtk_box_pack_start(
            root_vbox,
            gtk_separator_new(GTK_ORIENTATION_HORIZONTAL),
            0,
            0,
            4,
        );

        // Daemon, 1-Click Network Setup & Web UI Row
        let action_hbox = gtk_box_new(GTK_ORIENTATION_HORIZONTAL, 10);
        let btn_start =
            gtk_button_new_with_label(CString::new("Start Local Daemon").unwrap().as_ptr());
        let start_name = CString::new("btn-start").unwrap();
        gtk_widget_set_name(btn_start, start_name.as_ptr());
        connect_button_action(btn_start, on_start_daemon);
        gtk_box_pack_start(action_hbox, btn_start, 1, 1, 0);

        let btn_setup =
            gtk_button_new_with_label(CString::new("1-Click Setup Network").unwrap().as_ptr());
        connect_button_action(btn_setup, on_setup_network);
        gtk_box_pack_start(action_hbox, btn_setup, 1, 1, 0);

        let btn_web = gtk_button_new_with_label(
            CString::new(format!("Open Browser UI (127.0.0.1:{web_port})"))
                .unwrap()
                .as_ptr(),
        );
        connect_button_action(btn_web, on_open_web);
        gtk_box_pack_start(action_hbox, btn_web, 1, 1, 0);

        gtk_box_pack_start(root_vbox, action_hbox, 0, 1, 2);

        // Emergency Panic Button
        let btn_panic = gtk_button_new_with_label(
            CString::new("EMERGENCY PANIC / UNGRAB ALL (Ctrl+Alt+Shift+Backspace)")
                .unwrap()
                .as_ptr(),
        );
        let panic_name = CString::new("btn-panic").unwrap();
        gtk_widget_set_name(btn_panic, panic_name.as_ptr());
        connect_button_action(btn_panic, on_panic);
        gtk_box_pack_start(root_vbox, btn_panic, 0, 1, 4);

        // Status feedback bar
        let status_lbl = gtk_label_new(ptr::null());
        gtk_label_set_xalign(status_lbl, 0.0);
        gtk_box_pack_start(root_vbox, status_lbl, 0, 0, 2);

        TELEMETRY_LABEL_PTR.store(telemetry_lbl, Ordering::Release);
        STATUS_LABEL_PTR.store(status_lbl, Ordering::Release);
        WEB_PORT_LINUX.store(web_port, Ordering::Release);

        set_status_banner(&format!(
            "Ready — Press Ctrl+Alt+P anytime to cycle modes (Web UI: http://127.0.0.1:{web_port})"
        ));
        on_telemetry_tick(ptr::null_mut());
        g_timeout_add(400, on_telemetry_tick, ptr::null_mut());

        gtk_widget_show_all(win);
        gtk_main();
        true
    }
}
