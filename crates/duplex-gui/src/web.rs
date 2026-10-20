//! Embedded localhost Web UI & JSON Control API (`127.0.0.1:48180`) for `duplex-gui`.
//!
//! Runs automatically in a background thread alongside the native GTK3 / Win32 desktop window
//! (or in the foreground when `--web` is passed or no display server is available).

use crate::{
    open_url_in_browser, request_mode, run_embedded_network_setup, start_local_daemon,
    trigger_panic_release, TelemetrySnapshot,
};
use duplex_proto::OperationalMode;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

pub const DEFAULT_WEB_PORT: u16 = 48180;

const INDEX_HTML: &str = include_str!("../assets/web/index.html");

fn handle_http_client(mut stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));

    let mut bytes = Vec::new();
    let mut buf = [0u8; 2048];
    let header_end = loop {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => bytes.extend_from_slice(&buf[..n]),
        }
        if bytes.len() > 16_384 {
            return;
        }
        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
    let content_length = headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.trim().parse::<usize>())
        .transpose();
    let body_len = match content_length {
        Ok(value) => value.unwrap_or(0),
        Err(_) => return,
    };
    if header_end + body_len > 16_384 {
        return;
    }
    while bytes.len() < header_end + body_len {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => bytes.extend_from_slice(&buf[..n]),
        }
    }
    let body = &bytes[header_end..header_end + body_len];
    let req = headers;
    let first_line = req.lines().next().unwrap_or("");
    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or("GET");
    let path = parts.next().unwrap_or("/");

    // Restrict the Host as well as Origin: pairing returns a newly generated
    // secret and must not be exposed through a DNS-rebinding origin.
    let host = req
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.eq_ignore_ascii_case("host"))
        .map(|(_, v)| v.trim());
    let port = match stream.local_addr() {
        Ok(addr) => addr.port(),
        Err(_) => return,
    };
    if ![format!("127.0.0.1:{port}"), format!("localhost:{port}")]
        .iter()
        .any(|expected| Some(expected.as_str()) == host)
    {
        let _ = stream
            .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    }

    // Browser requests that mutate local state must originate from this control UI.
    if method == "POST" {
        let cross_site = req
            .lines()
            .filter_map(|line| line.split_once(':'))
            .any(|(key, value)| {
                key.eq_ignore_ascii_case("sec-fetch-site") && value.trim() == "cross-site"
            });
        let host = req
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(key, _)| key.eq_ignore_ascii_case("host"))
            .map(|(_, v)| v.trim());
        let wrong_origin = req
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(key, _)| key.eq_ignore_ascii_case("origin"))
            .is_some_and(|(_, origin)| {
                host.map(|h| format!("http://{h}")).as_deref() != Some(origin.trim())
            });
        if cross_site || wrong_origin {
            let _ = stream.write_all(
                b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
            return;
        }
    }

    if method == "GET" {
        let asset: Option<(&str, &[u8])> = match path {
            "/control.css" => Some((
                "text/css; charset=utf-8",
                include_bytes!("../assets/web/control.css"),
            )),
            "/control.js" => Some((
                "text/javascript; charset=utf-8",
                include_bytes!("../assets/web/control.js"),
            )),
            _ => path.strip_prefix("/fonts/").and_then(|name| {
                crate::design::fonts::ALL
                    .iter()
                    .find(|(file, _)| *file == name)
                    .map(|(_, bytes)| ("font/ttf", *bytes))
            }),
        };
        if let Some((content_type, bytes)) = asset {
            let header = format!("HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n", bytes.len());
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(bytes);
            return;
        }
    }

    if method == "GET" && (path == "/" || path.starts_with("/index")) {
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{}",
            INDEX_HTML.len(),
            INDEX_HTML
        );
        let _ = stream.write_all(resp.as_bytes());
        return;
    }

    if method == "GET" && path.starts_with("/api/status") {
        let snap = TelemetrySnapshot::read_current();
        let mut status: serde_json::Value = serde_json::from_str(&snap.to_json()).unwrap();
        status["link"] = serde_json::to_value(crate::diagnostics::link_status(&snap)).unwrap();
        let json = status.to_string();
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{}",
            json.len(),
            json
        );
        let _ = stream.write_all(resp.as_bytes());
        return;
    }

    if method == "POST" && path.starts_with("/api/mode") {
        let target = path
            .split("target=")
            .nth(1)
            .unwrap_or("idle")
            .split('&')
            .next()
            .unwrap_or("idle");
        let mode = match target {
            "cycle" => {
                let snap = TelemetrySnapshot::read_current();
                OperationalMode::from_u8_lossy(snap.mode_code).cycle_next()
            }
            "pc_kvm" => OperationalMode::Mode1PcPrimary {
                extend_display: false,
            },
            "pc_extend" | "pc" => OperationalMode::Mode1PcPrimary {
                extend_display: true,
            },
            "laptop" => OperationalMode::Mode2LaptopPrimary,
            _ => OperationalMode::Mode0Idle,
        };
        let msg = match request_mode(mode) {
            Ok(m) => m,
            Err(e) => e,
        };
        write_json_message(&mut stream, &msg);
        return;
    }

    if method == "POST" && path.starts_with("/api/panic") {
        let msg = match trigger_panic_release() {
            Ok(m) => m,
            Err(e) => e,
        };
        write_json_message(&mut stream, &msg);
        return;
    }

    if method == "POST" && path == "/api/restart-daemon" {
        let result = crate::restart_local_daemon();
        write_json_value(
            &mut stream,
            serde_json::json!({"ok": result.is_ok(),
            "message": result.unwrap_or_else(|error| error)}),
        );
        return;
    }

    if method == "POST" && path.starts_with("/api/start-daemon") {
        let msg = match start_local_daemon() {
            Ok(m) => m,
            Err(e) => e,
        };
        write_json_message(&mut stream, &msg);
        return;
    }

    if method == "POST" && path == "/api/connect-link" {
        let result = crate::diagnostics::connect_pc_link();
        write_json_value(
            &mut stream,
            serde_json::json!({"ok": result.is_ok(),
            "message": result.unwrap_or_else(|error| error)}),
        );
        return;
    }

    if method == "POST" && path.starts_with("/api/setup-network") {
        let msg = match run_embedded_network_setup() {
            Ok(m) => m,
            Err(e) => e,
        };
        write_json_message(&mut stream, &msg);
        return;
    }

    if method == "GET" && path == "/api/pairing" {
        use sha2::{Digest, Sha256};
        let status = match duplexd::pairing::load_key() {
            Ok(key) => {
                let fingerprint: String = Sha256::digest(key)
                    .iter()
                    .take(8)
                    .map(|b| format!("{b:02x}"))
                    .collect();
                serde_json::json!({"configured": true, "fingerprint": &fingerprint[..16],
                    "environment_override": std::env::var_os("DUPLEX_PSK_HEX").is_some()})
            }
            Err(error) => serde_json::json!({"configured": false, "error": error}),
        };
        write_json_value(&mut stream, status);
        return;
    }

    if method == "POST" && matches!(path, "/api/pairing/generate" | "/api/pairing/save") {
        if TelemetrySnapshot::read_current().daemon_online {
            write_json_value(
                &mut stream,
                serde_json::json!({"ok": false, "message": "Stop the daemon before changing its pairing key."}),
            );
            return;
        }
        let key = if path.ends_with("generate") {
            duplexd::pairing::generate_key()
        } else {
            serde_json::from_slice::<serde_json::Value>(body)
                .ok()
                .and_then(|value| value.get("key").and_then(|v| v.as_str()).map(str::to_owned))
                .ok_or_else(|| "Paste the pairing key from your other computer".to_string())
        };
        let result = key.and_then(|key| {
            duplexd::pairing::save_key(&key)?;
            Ok(key.trim().to_lowercase())
        });
        let response = match result {
            Ok(key) if path.ends_with("generate") => serde_json::json!({"ok": true, "key": key,
                "message": "Key saved on this computer. Copy it privately and save it on the other computer, then start both daemons."}),
            Ok(_) => {
                serde_json::json!({"ok": true, "message": "Pairing key saved. Start the daemon on both computers."})
            }
            Err(error) => serde_json::json!({"ok": false, "message": error}),
        };
        write_json_value(&mut stream, response);
        return;
    }

    if method == "GET" && path.starts_with("/api/update") {
        let status = crate::updater::get_update_status();
        let cfg = crate::updater::AppConfig::load();
        let json = serde_json::json!({
            "status": status,
            "repo": cfg.effective_repo(),
            "has_token": cfg.effective_token().is_some(),
            "masked_token": cfg.masked_token(),
            "auto_check": cfg.auto_check_updates,
            "auto_install": cfg.auto_install_updates,
        })
        .to_string();
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{}",
            json.len(),
            json
        );
        let _ = stream.write_all(resp.as_bytes());
        return;
    }

    if method == "POST" && path == "/api/update/check" {
        thread::spawn(|| {
            let _ = crate::updater::check_for_updates(true);
        });
        write_json_message(
            &mut stream,
            "Checking repository releases; progress appears under Software & updates.",
        );
        return;
    }

    if method == "POST" && path == "/api/update/install" {
        thread::spawn(|| {
            let _ = crate::updater::install_update();
        });
        write_json_message(
            &mut stream,
            "Installing update; progress appears under Software & updates.",
        );
        return;
    }

    if method == "POST" && path == "/api/config/updates" {
        #[derive(serde::Deserialize)]
        struct Preferences {
            auto_check: bool,
            auto_install: bool,
        }
        let preferences = match serde_json::from_slice::<Preferences>(body) {
            Ok(value) => value,
            Err(_) => {
                write_json_message(&mut stream, "Invalid update preferences");
                return;
            }
        };
        let mut config = crate::updater::AppConfig::load();
        config.auto_check_updates = preferences.auto_check;
        config.auto_install_updates = preferences.auto_install;
        let message = config
            .save()
            .map(|()| "Update preferences saved".to_string())
            .unwrap_or_else(|e| e);
        write_json_message(&mut stream, &message);
        return;
    }

    if method == "POST" && path == "/api/config/token" {
        let decoded = match serde_json::from_slice::<serde_json::Value>(body) {
            Ok(value) => match value.get("token").and_then(|v| v.as_str()) {
                Some(token) => token.to_string(),
                None => {
                    write_json_message(&mut stream, "Missing token field");
                    return;
                }
            },
            Err(_) => {
                write_json_message(&mut stream, "Invalid token request");
                return;
            }
        };
        let mut cfg = crate::updater::AppConfig::load();
        cfg.github_token = if decoded.trim().is_empty() {
            None
        } else {
            Some(decoded.trim().to_string())
        };
        let msg = match cfg.save() {
            Ok(()) => "GitHub Personal Access Token saved successfully.".to_string(),
            Err(e) => format!("Failed to save token: {e}"),
        };
        write_json_message(&mut stream, &msg);
        return;
    }

    let not_found =
        "HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nNot Found";
    let _ = stream.write_all(not_found.as_bytes());
}

fn write_json_message(stream: &mut TcpStream, msg: &str) {
    write_json_value(stream, serde_json::json!({"message": msg}));
}

fn write_json_value(stream: &mut TcpStream, value: serde_json::Value) {
    let body = value.to_string();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{}", body.len(), body);
    let _ = stream.write_all(response.as_bytes());
}

/// Spawns the localhost Web Control Center on `127.0.0.1:port` in a background daemon thread.
pub fn spawn_background_web_server(port: u16) -> Option<SocketAddr> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = TcpListener::bind(addr).ok()?;
    let bound_addr = listener.local_addr().ok()?;

    thread::Builder::new()
        .name("duplex-gui-web".to_string())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                handle_http_client(stream);
            }
        })
        .ok()?;

    Some(bound_addr)
}

/// Runs the localhost Web Control Center in the foreground and optionally opens the default browser.
pub fn run_web_server_blocking(port: u16, open_browser: bool) {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = match TcpListener::bind(addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Failed to bind Duplex-KVM Web UI on http://{addr}: {e}");
            std::process::exit(1);
        }
    };
    let url = format!("http://{addr}");
    println!("Duplex-KVM Graphical Control Center listening on {url}");
    if open_browser {
        open_url_in_browser(&url);
    }
    for stream in listener.incoming().flatten() {
        handle_http_client(stream);
    }
}
