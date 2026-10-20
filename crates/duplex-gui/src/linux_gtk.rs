//! Native GTK+ 3 Wayland/X11 Graphical Control Center for Debian Linux (`#[cfg(target_os = "linux")]`).
//!
//! Uses direct FFI bindings to `libgtk-3`, `libgdk-3`, `libgobject-2.0`, and `libglib-2.0` so that
//! `duplex-gui` compiles in <1s with zero external crate downloads and runs natively on both
//! Wayland (GNOME/KDE) and X11 sessions.

use crate::design;
use crate::{
    open_url_in_browser, request_mode, restart_local_daemon, run_embedded_network_setup,
    start_local_daemon, trigger_panic_release, TelemetrySnapshot,
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
    fn gtk_scrolled_window_new(h: *mut c_void, v: *mut c_void) -> *mut c_void;
    fn gtk_scrolled_window_set_policy(w: *mut c_void, h: c_int, v: c_int);
    fn gtk_label_set_line_wrap(w: *mut c_void, wrap: c_int);
    fn gtk_label_set_max_width_chars(w: *mut c_void, n: c_int);
    fn gtk_flow_box_new() -> *mut c_void;
    fn gtk_flow_box_set_selection_mode(w: *mut c_void, mode: c_int);
    fn gtk_flow_box_set_max_children_per_line(w: *mut c_void, n: c_uint);
    fn gtk_flow_box_set_min_children_per_line(w: *mut c_void, n: c_uint);
    fn gtk_flow_box_set_homogeneous(w: *mut c_void, h: c_int);
    fn gtk_flow_box_set_row_spacing(w: *mut c_void, n: c_uint);
    fn gtk_flow_box_set_column_spacing(w: *mut c_void, n: c_uint);
    fn gtk_button_new() -> *mut c_void;
    fn gtk_widget_get_style_context(w: *mut c_void) -> *mut c_void;
    fn gtk_style_context_add_class(w: *mut c_void, name: *const c_char);
    fn gtk_style_context_remove_class(w: *mut c_void, name: *const c_char);

    fn gtk_widget_show_all(widget: *mut c_void);
    fn gtk_widget_set_tooltip_text(widget: *mut c_void, text: *const c_char);
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

static LIGHT_MODE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static CSS_PROVIDER: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
static HERO_LABEL: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
static HERO_DESCRIPTION: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
static PANIC_BUTTON: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
static LINK_LABEL: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
static METRIC_LABELS: Mutex<Vec<usize>> = Mutex::new(Vec::new());
static MODE_BUTTONS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

unsafe fn apply_appearance() {
    let p = if LIGHT_MODE.load(Ordering::Relaxed) {
        design::LIGHT
    } else {
        design::DARK
    };
    // Keep the application's typography and controls out of GTK's header bar and
    // window decorations, which must follow the user's desktop theme.
    let css = format!(
        r#"
        #duplex-content, #duplex-scroll {{ background: {bg}; color: {text}; }}
        #duplex-content, #duplex-content * {{ font-family: "Space Grotesk"; font-size: 16px; text-shadow: none; box-shadow: none; }}
        #duplex-content label {{ color: {text}; }}
        #duplex-content .label, #duplex-content .label label {{ font-family: "Space Mono"; font-size: 11px; color: {muted}; }}
        #duplex-content .hero {{ font-family: "Duplex Dots"; font-size: 64px; font-weight: bold; color: {display}; }}
        #duplex-content .muted {{ color: {muted}; }}
        #duplex-content button {{ background-image: none; background-color: transparent; color: {text}; border: 1px solid {border}; border-radius: 6px; padding: 12px 16px; min-height: 20px; }}
        #duplex-content button.technical, #duplex-content button.technical label {{ font-family: "Space Mono"; font-size: 11px; }}
        #duplex-content button:disabled {{ color: {muted}; border-color: {border}; }}
        #duplex-content button:hover {{ background: {raised}; border-color: {muted}; }}
        #duplex-content button:focus {{ outline: 2px solid {display}; outline-offset: 2px; }}
        #duplex-content button.active {{ background: {surface}; border-color: {display}; }}
        #duplex-content #btn-panic.armed {{ color: {red}; border-color: {red}; }}
        #duplex-content #btn-panic.armed label {{ color: {red}; }}
        #duplex-content flowboxchild {{ padding: 4px; }}
        #duplex-content flowboxchild:selected {{ background: transparent; }}
    "#,
        bg = design::hex(p.black),
        text = design::hex(p.text_primary),
        muted = design::hex(p.text_secondary),
        display = design::hex(p.text_display),
        border = design::hex(p.border_visible),
        raised = design::hex(p.surface_raised),
        surface = design::hex(p.surface),
        red = design::hex(p.accent)
    );
    let css = CString::new(css).unwrap();
    gtk_css_provider_load_from_data(
        CSS_PROVIDER.load(Ordering::Relaxed),
        css.as_ptr(),
        -1,
        ptr::null_mut(),
    );
}

unsafe extern "C" fn on_appearance(_widget: *mut c_void, _data: *mut c_void) {
    LIGHT_MODE.fetch_xor(true, Ordering::Relaxed);
    design::save_light_appearance(LIGHT_MODE.load(Ordering::Relaxed));
    apply_appearance();
    on_telemetry_tick(ptr::null_mut());
}

#[link(name = "fontconfig")]
extern "C" {
    fn FcConfigGetCurrent() -> *mut c_void;
    fn FcConfigAppFontAddFile(config: *mut c_void, file: *const u8) -> c_int;
}

unsafe fn register_fonts() -> Option<std::path::PathBuf> {
    let dir = std::env::temp_dir().join(format!("duplex-fonts-{}", std::process::id()));
    std::fs::create_dir_all(&dir).ok()?;
    for (name, bytes) in design::fonts::ALL {
        let path = dir.join(name);
        std::fs::write(&path, bytes).ok()?;
        let path = CString::new(path.to_string_lossy().as_bytes()).ok()?;
        FcConfigAppFontAddFile(FcConfigGetCurrent(), path.as_ptr().cast());
    }
    Some(dir)
}

use core::sync::atomic::{AtomicPtr, AtomicU16, AtomicU64, Ordering};
use duplex_pal::monotonic_now_ns;
use std::sync::Mutex;

static STATUS_LABEL_PTR: AtomicPtr<c_void> = AtomicPtr::new(ptr::null_mut());
static WEB_PORT_LINUX: AtomicU16 = AtomicU16::new(48180);
static LAST_BTN_ACTION_NS: AtomicU64 = AtomicU64::new(0);

fn allow_button_action() -> bool {
    let now = monotonic_now_ns();
    let prev = LAST_BTN_ACTION_NS.load(Ordering::Acquire);
    if now.saturating_sub(prev) < 250_000_000 {
        return false;
    }
    LAST_BTN_ACTION_NS.store(now, Ordering::Release);
    true
}

static PENDING_STATUS: Mutex<Option<String>> = Mutex::new(None);

fn set_status_banner(msg: &str) {
    if let Ok(mut status) = PENDING_STATUS.lock() {
        *status = Some(msg.to_string());
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
    let res = if TelemetrySnapshot::read_current().daemon_online {
        restart_local_daemon()
    } else {
        start_local_daemon()
    };
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

unsafe extern "C" fn on_check_update(_widget: *mut c_void, _data: *mut c_void) {
    if !allow_button_action() {
        return;
    }
    set_status_banner("Checking repository releases for updates...");
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
            Err(e) => format!("Update check: {e}"),
        };
        set_status_banner(&msg);
    });
}

unsafe extern "C" fn on_panic(_widget: *mut c_void, _data: *mut c_void) {
    let res = trigger_panic_release();
    set_status_banner(&res.unwrap_or_else(|e| e));
    on_telemetry_tick(ptr::null_mut());
}

unsafe fn set_label(w: *mut c_void, text: &str) {
    if !w.is_null() {
        let escaped = text
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        if let Ok(c) = CString::new(escaped) {
            gtk_label_set_markup(w, c.as_ptr());
        }
    }
}

unsafe extern "C" fn on_telemetry_tick(_data: *mut c_void) -> c_int {
    if let Ok(mut status) = PENDING_STATUS.lock() {
        if let Some(text) = status.take() {
            set_label(STATUS_LABEL_PTR.load(Ordering::Relaxed), &text);
        }
    }
    let s = TelemetrySnapshot::read_current();
    let mode = design::mode_spec(s.mode_code);
    set_label(HERO_LABEL.load(Ordering::Relaxed), mode.glyph);
    let p = if LIGHT_MODE.load(Ordering::Relaxed) {
        design::LIGHT
    } else {
        design::DARK
    };
    let description = CString::new(format!(
        "{}\n<span foreground=\"{}\">{}</span>",
        mode.title,
        design::hex(p.text_secondary),
        mode.description
    ))
    .unwrap();
    gtk_label_set_markup(
        HERO_DESCRIPTION.load(Ordering::Relaxed),
        description.as_ptr(),
    );
    let (daemon, daemon_level) = design::daemon_status(s.daemon_online);
    let (peer, peer_level) = design::peer_status(s.daemon_online, s.peer_connected);
    let age = if s.peer_connected {
        design::fmt_ms(s.peer_age_ms)
    } else {
        "—".into()
    };
    let filled = design::heartbeat_filled_segments(s.peer_age_ms, s.peer_connected);
    let bar = format!(
        "<span foreground=\"{}\">{}</span><span foreground=\"{}\">{}</span>",
        design::hex(p.text_primary),
        "▰".repeat(filled),
        design::hex(p.border_visible),
        "▰".repeat(design::HEARTBEAT_SEGMENTS - filled)
    );
    let link = CString::new(format!("LOCAL DAEMON    <span foreground=\"{}\">{daemon}</span>\nWIRED PEER      <span foreground=\"{}\">{peer}</span>\n\nHEARTBEAT  <span foreground=\"{}\">{age} / 250 MS</span>\n{bar}",
        design::hex(daemon_level.color(&p)), design::hex(peer_level.color(&p)),
        design::hex(design::heartbeat_level(s.peer_age_ms, s.peer_connected).color(&p)))).unwrap();
    gtk_label_set_markup(LINK_LABEL.load(Ordering::Relaxed), link.as_ptr());
    let panic = PANIC_BUTTON.load(Ordering::Relaxed);
    if !panic.is_null() {
        let armed = CString::new("armed").unwrap();
        let ctx = gtk_widget_get_style_context(panic);
        if s.input_grab_active {
            gtk_style_context_add_class(ctx, armed.as_ptr());
        } else {
            gtk_style_context_remove_class(ctx, armed.as_ptr());
        }
    }
    let values = [
        format!(
            "RTT / MIN · CUR · MAX\n{:.0} / {:.0} / {:.0} µs\n1 GBE / 50 HZ HEARTBEAT",
            s.min_rtt_us, s.last_rtt_us, s.max_rtt_us
        ),
        format!(
            "INPUT / DISPLAYS\n{} / {}\nDAEMON HB / {:.1} MS",
            if s.input_grab_active { "ACTIVE" } else { "OFF" },
            s.virtual_displays_active,
            s.daemon_age_ms
        ),
        format!(
            "HID / TX · RX\n{} / {}\nCHACHA20-POLY1305",
            design::fmt_count(s.tx_hid_packets),
            design::fmt_count(s.rx_hid_packets)
        ),
        format!(
            "VIDEO / TX · RX\n{} / {}\nNATIVE RES · LOSSLESS TEXT",
            design::fmt_count(s.tx_video_frames),
            design::fmt_count(s.rx_video_frames)
        ),
        format!(
            "KEYFRAME RECOVERIES\n{}\nPLI / GAP DETECTION",
            design::fmt_count(s.pli_keyframes_requested)
        ),
    ];
    if let Ok(labels) = METRIC_LABELS.lock() {
        for (&w, value) in labels.iter().zip(values) {
            let mut lines = value.lines();
            let label = lines.next().unwrap_or("");
            let value = lines.next().unwrap_or("");
            let detail = lines.next().unwrap_or("");
            let escaped = |s: &str| {
                s.replace('&', "&amp;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;")
            };
            let markup = CString::new(format!("<span font_desc=\"Space Mono 8.25\" foreground=\"{}\">{}</span>\n{}\n<span font_desc=\"Space Mono 8.25\" foreground=\"{}\">{}</span>", design::hex(p.text_secondary), escaped(label), escaped(value), design::hex(p.text_disabled), escaped(detail))).unwrap();
            gtk_label_set_markup(w as *mut c_void, markup.as_ptr());
        }
    }
    if let Ok(buttons) = MODE_BUTTONS.lock() {
        let class = CString::new("active").unwrap();
        for (&w, mode) in buttons.iter().zip(design::MODES) {
            let ctx = gtk_widget_get_style_context(w as *mut c_void);
            if s.daemon_online && mode.code == s.mode_code {
                gtk_style_context_add_class(ctx, class.as_ptr());
            } else {
                gtk_style_context_remove_class(ctx, class.as_ptr());
            }
        }
    }
    1
}

/// Standard GTK activation supports mouse, keyboard and assistive technology.
unsafe fn connect_button_action(
    button: *mut c_void,
    callback: unsafe extern "C" fn(*mut c_void, *mut c_void),
) {
    g_signal_connect_data(
        button,
        CString::new("clicked").unwrap().as_ptr(),
        core::mem::transmute::<unsafe extern "C" fn(*mut c_void, *mut c_void), unsafe extern "C" fn()>(
            callback,
        ),
        ptr::null_mut(),
        ptr::null(),
        0,
    );
}

unsafe fn class(w: *mut c_void, name: &str) {
    let name = CString::new(name).unwrap();
    gtk_style_context_add_class(gtk_widget_get_style_context(w), name.as_ptr());
}
unsafe fn label(text: &str, css: &str) -> *mut c_void {
    let text = CString::new(text).unwrap();
    let w = gtk_label_new(text.as_ptr());
    gtk_label_set_xalign(w, 0.0);
    gtk_label_set_line_wrap(w, 1);
    gtk_label_set_max_width_chars(w, 48);
    class(w, css);
    w
}
unsafe fn pack(box_: *mut c_void, w: *mut c_void, padding: u32) {
    gtk_box_pack_start(box_, w, 0, 1, padding);
}
unsafe fn button(
    text: &str,
    callback: unsafe extern "C" fn(*mut c_void, *mut c_void),
) -> *mut c_void {
    let text = CString::new(text).unwrap();
    let w = gtk_button_new_with_label(text.as_ptr());
    class(w, "technical");
    connect_button_action(w, callback);
    w
}
unsafe fn flow(columns: u32) -> *mut c_void {
    let w = gtk_flow_box_new();
    gtk_flow_box_set_selection_mode(w, 0);
    gtk_flow_box_set_min_children_per_line(w, 1);
    gtk_flow_box_set_max_children_per_line(w, columns);
    gtk_flow_box_set_homogeneous(w, 1);
    gtk_flow_box_set_row_spacing(w, 8);
    gtk_flow_box_set_column_spacing(w, 8);
    w
}

pub fn run_native_gtk_window(web_port: u16) -> bool {
    unsafe {
        let fonts_dir = register_fonts();
        if gtk_init_check(ptr::null_mut(), ptr::null_mut()) == 0 {
            if let Some(dir) = fonts_dir {
                let _ = std::fs::remove_dir_all(dir);
            }
            return false;
        }
        LIGHT_MODE.store(design::load_light_appearance(), Ordering::Relaxed);
        let provider = gtk_css_provider_new();
        CSS_PROVIDER.store(provider, Ordering::Relaxed);
        apply_appearance();
        gtk_style_context_add_provider_for_screen(
            gdk_screen_get_default(),
            provider,
            GTK_STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        let win = gtk_window_new(GTK_WINDOW_TOPLEVEL);
        gtk_window_set_title(
            win,
            CString::new("Duplex — Control center").unwrap().as_ptr(),
        );
        gtk_window_set_default_size(win, 1120, 660);
        gtk_window_set_position(win, 1);
        g_signal_connect_data(
            win,
            CString::new("destroy").unwrap().as_ptr(),
            on_window_destroy,
            ptr::null_mut(),
            ptr::null(),
            0,
        );
        let scroll = gtk_scrolled_window_new(ptr::null_mut(), ptr::null_mut());
        gtk_scrolled_window_set_policy(scroll, 2, 1);
        gtk_widget_set_name(scroll, CString::new("duplex-scroll").unwrap().as_ptr());
        gtk_container_add(win, scroll);
        let root = gtk_box_new(GTK_ORIENTATION_VERTICAL, 0);
        gtk_widget_set_name(root, CString::new("duplex-content").unwrap().as_ptr());
        gtk_container_set_border_width(root, 16);
        gtk_container_add(scroll, root);
        let header = gtk_box_new(GTK_ORIENTATION_HORIZONTAL, 24);
        gtk_box_pack_start(header, label("duplex / kvm", "wordmark"), 1, 1, 0);
        pack(
            header,
            label(
                &format!("V{} / CONTROL CENTER", crate::updater::CURRENT_VERSION),
                "label",
            ),
            0,
        );
        pack(header, button("DARK / LIGHT", on_appearance), 0);
        pack(root, header, 0);
        let overview = flow(2);
        gtk_flow_box_set_homogeneous(overview, 0);
        let current = gtk_box_new(GTK_ORIENTATION_VERTICAL, 0);
        pack(current, label("CURRENT MODE", "label"), 0);
        let hero = gtk_box_new(GTK_ORIENTATION_HORIZONTAL, 24);
        let glyph = label("—", "hero");
        HERO_LABEL.store(glyph, Ordering::Relaxed);
        pack(hero, glyph, 0);
        let desc = label("Connecting", "body");
        gtk_label_set_max_width_chars(desc, 36);
        HERO_DESCRIPTION.store(desc, Ordering::Relaxed);
        gtk_box_pack_start(hero, desc, 1, 1, 0);
        pack(current, hero, 0);
        gtk_container_add(overview, current);
        let link = label("CONNECTING", "label");
        LINK_LABEL.store(link, Ordering::Relaxed);
        gtk_container_add(overview, link);
        pack(root, overview, 4);
        let routing = gtk_box_new(GTK_ORIENTATION_HORIZONTAL, 24);
        gtk_box_pack_start(routing, label("01 / Routing", "body"), 1, 1, 0);
        pack(routing, label("CTRL + ALT + P TO CYCLE", "label"), 0);
        pack(root, routing, 4);
        let modes = flow(4);
        let callbacks = [
            on_mode_idle,
            on_mode_pc_kvm,
            on_mode_pc_extend,
            on_mode_laptop,
        ];
        for (mode, callback) in design::MODES.iter().zip(callbacks) {
            let w = gtk_button_new();
            let content = gtk_box_new(GTK_ORIENTATION_VERTICAL, 8);
            pack(content, label(&format!("MODE {}", mode.glyph), "label"), 0);
            let title = label(mode.title, "body");
            gtk_label_set_max_width_chars(title, 22);
            pack(content, title, 0);
            pack(content, label(mode.shortcut, "label"), 0);
            gtk_widget_set_tooltip_text(w, CString::new(mode.description).unwrap().as_ptr());
            gtk_widget_set_size_request(content, 180, 76);
            gtk_container_add(w, content);
            connect_button_action(w, callback);
            MODE_BUTTONS.lock().unwrap().push(w as usize);
            gtk_container_add(modes, w);
        }
        pack(root, modes, 4);
        let status = label("[READY]", "label");
        STATUS_LABEL_PTR.store(status, Ordering::Relaxed);
        pack(root, status, 2);
        pack(root, label("02 / Telemetry", "body"), 4);
        let metrics = flow(5);
        for _ in 0..5 {
            let w = label("CONNECTING", "body");
            gtk_label_set_max_width_chars(w, 24);
            gtk_widget_set_size_request(w, 190, 64);
            METRIC_LABELS.lock().unwrap().push(w as usize);
            gtk_container_add(metrics, w);
        }
        pack(root, metrics, 0);
        pack(root, label("03 / System", "body"), 4);
        let actions = flow(4);
        for (text, cb) in [
            (
                "START / RESTART DAEMON",
                on_start_daemon as unsafe extern "C" fn(*mut c_void, *mut c_void),
            ),
            ("SET UP NETWORK", on_setup_network),
            ("OPEN BROWSER UI", on_open_web),
            ("CHECK UPDATES", on_check_update),
        ] {
            gtk_container_add(actions, button(text, cb));
        }
        pack(root, actions, 0);
        let panic = gtk_button_new();
        gtk_container_add(
            panic,
            label("EMERGENCY RELEASE / CTRL+ALT+SHIFT+BACKSPACE", "label"),
        );
        class(panic, "technical");
        PANIC_BUTTON.store(panic, Ordering::Relaxed);
        connect_button_action(panic, on_panic);
        gtk_widget_set_name(panic, CString::new("btn-panic").unwrap().as_ptr());
        pack(root, panic, 2);
        WEB_PORT_LINUX.store(web_port, Ordering::Relaxed);
        on_telemetry_tick(ptr::null_mut());
        g_timeout_add(400, on_telemetry_tick, ptr::null_mut());
        gtk_widget_show_all(win);
        gtk_main();
        if let Some(dir) = fonts_dir {
            let _ = std::fs::remove_dir_all(dir);
        }
        true
    }
}
