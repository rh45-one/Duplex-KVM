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

const INDEX_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Duplex-KVM Control Center</title>
<style>
  :root {
    --bg: #0f172a;
    --panel: #1e293b;
    --panel-border: #334155;
    --text: #f8fafc;
    --muted: #94a3b8;
    --accent: #38bdf8;
    --green: #22c55e;
    --amber: #f59e0b;
    --red: #ef4444;
  }
  * { box-sizing: border-box; margin: 0; padding: 0; }
  body {
    background: var(--bg);
    color: var(--text);
    font-family: system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
    padding: 24px;
    max-width: 860px;
    margin: 0 auto;
  }
  header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    background: var(--panel);
    border: 1px solid var(--panel-border);
    border-radius: 12px;
    padding: 18px 22px;
    margin-bottom: 18px;
  }
  .title h1 { font-size: 20px; font-weight: 700; letter-spacing: -0.02em; }
  .title p { font-size: 13px; color: var(--muted); margin-top: 4px; }
  .badges { display: flex; gap: 10px; align-items: center; }
  .badge {
    padding: 6px 12px;
    border-radius: 999px;
    font-size: 12px;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.04em;
  }
  .badge.ok { background: rgba(34, 197, 94, 0.18); color: #4ade80; border: 1px solid rgba(34, 197, 94, 0.4); }
  .badge.warn { background: rgba(245, 158, 11, 0.18); color: #fbbf24; border: 1px solid rgba(245, 158, 11, 0.4); }
  .badge.err { background: rgba(239, 68, 68, 0.18); color: #f87171; border: 1px solid rgba(239, 68, 68, 0.4); }

  .grid {
    display: grid;
    grid-template-columns: repeat(3, 1fr);
    gap: 12px;
    margin-bottom: 18px;
  }
  .card {
    background: var(--panel);
    border: 1px solid var(--panel-border);
    border-radius: 10px;
    padding: 14px 16px;
  }
  .card .label { font-size: 11px; text-transform: uppercase; letter-spacing: 0.05em; color: var(--muted); }
  .card .value { font-size: 16px; font-weight: 700; margin-top: 6px; font-variant-numeric: tabular-nums; }
  .card .sub { font-size: 12px; color: var(--muted); margin-top: 3px; font-variant-numeric: tabular-nums; }

  .section {
    background: var(--panel);
    border: 1px solid var(--panel-border);
    border-radius: 12px;
    padding: 18px 22px;
    margin-bottom: 18px;
  }
  .section h2 {
    font-size: 14px;
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--muted);
    margin-bottom: 14px;
  }
  .mode-buttons {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 12px;
  }
  button {
    cursor: pointer;
    border-radius: 9px;
    border: 1px solid var(--panel-border);
    background: #26354d;
    color: var(--text);
    padding: 13px 16px;
    text-align: left;
    transition: all 0.12s ease;
    font-family: inherit;
  }
  button:hover { background: #314463; border-color: var(--accent); }
  button.active {
    background: rgba(56, 189, 248, 0.18);
    border-color: var(--accent);
    box-shadow: 0 0 0 1px var(--accent);
  }
  button .btn-title { font-size: 14px; font-weight: 700; display: block; }
  button .btn-desc { font-size: 12px; color: var(--muted); margin-top: 4px; display: block; }

  .actions-row {
    display: flex;
    gap: 12px;
  }
  .btn-start {
    flex: 1;
    background: rgba(34, 197, 94, 0.16);
    border-color: rgba(34, 197, 94, 0.5);
    text-align: center;
  }
  .btn-start:hover { background: rgba(34, 197, 94, 0.28); border-color: #4ade80; }
  .btn-panic {
    flex: 1.4;
    background: rgba(239, 68, 68, 0.2);
    border-color: rgba(239, 68, 68, 0.65);
    text-align: center;
  }
  .btn-panic:hover { background: rgba(239, 68, 68, 0.35); border-color: #f87171; }
  #toast {
    margin-top: 12px;
    font-size: 13px;
    color: var(--accent);
    min-height: 18px;
  }
</style>
</head>
<body>
  <header>
    <div class="title">
      <h1>Duplex-KVM Control Center</h1>
      <p>1 Gbps Bidirectional Wired KVM, Virtual Display Streamer &amp; 250ms Dead-Man's Switch</p>
    </div>
    <div class="badges">
      <span id="daemon-badge" class="badge err">DAEMON OFFLINE</span>
      <span id="peer-badge" class="badge warn">PEER IDLE</span>
    </div>
  </header>

  <div class="grid">
    <div class="card">
      <div class="label">Operational Mode</div>
      <div id="val-mode" class="value">MODE 0: IDLE</div>
      <div id="sub-mode" class="sub">Local peripherals &amp; displays independent</div>
    </div>
    <div class="card">
      <div class="label">Grab &amp; Virtual Displays</div>
      <div id="val-grab" class="value">Grab: OFF | Displays: 0</div>
      <div id="sub-hb" class="sub">Daemon HB: -- ms | Peer HB: -- ms</div>
    </div>
    <div class="card">
      <div class="label">1GbE Link RTT (Min / Cur / Max)</div>
      <div id="val-rtt" class="value">0.0 / 0.0 / 0.0 &mu;s</div>
      <div class="sub">DSCP CS6 (0xC0) 50 Hz Heartbeat</div>
    </div>
    <div class="card">
      <div class="label">Encrypted HID Batches (TX / RX)</div>
      <div id="val-hid" class="value">0 / 0</div>
      <div class="sub">UDP 48100 &bull; ChaCha20-Poly1305 &bull; DSCP EF</div>
    </div>
    <div class="card">
      <div class="label">HEVC Video Frames (TX / RX)</div>
      <div id="val-vid" class="value">0 / 0</div>
      <div class="sub">UDP 48102-3 &bull; 60 Mbps Cap &bull; DSCP AF41</div>
    </div>
    <div class="card">
      <div class="label">PLI Keyframe Recoveries</div>
      <div id="val-pli" class="value">0</div>
      <div class="sub">Zero-Artifact Gap Detection</div>
    </div>
  </div>

  <div class="section">
    <h2>Switch Operational Mode</h2>
    <div class="mode-buttons">
      <button id="btn-mode-0" onclick="setMode('idle')">
        <span class="btn-title">Mode 0 &mdash; Idle (Local Only)</span>
        <span class="btn-desc">Release all input grabs &amp; virtual displays; keep 1GbE internet passthrough active.</span>
      </button>
      <button id="btn-mode-3" onclick="setMode('pc_kvm')">
        <span class="btn-title">Mode 1 &mdash; PC Primary (KVM Input Only)</span>
        <span class="btn-desc">Debian Laptop keyboard &amp; trackpad control the Windows 11 Desktop.</span>
      </button>
      <button id="btn-mode-1" onclick="setMode('pc_extend')">
        <span class="btn-title">Mode 1+ &mdash; PC Primary (+ Extend Display)</span>
        <span class="btn-desc">Laptop controls Windows PC + extends 1 Windows virtual monitor onto Laptop screen.</span>
      </button>
      <button id="btn-mode-2" onclick="setMode('laptop')">
        <span class="btn-title">Mode 2 &mdash; Dock PC Dual Monitors to Laptop</span>
        <span class="btn-desc">Windows USB keyboard/mouse &amp; 2 physical monitors dock to the Debian Laptop.</span>
      </button>
    </div>
    <div id="toast"></div>
  </div>

  <div class="section">
    <h2>Daemon Supervision, 1-Click Network Setup &amp; Emergency Safety</h2>
    <div class="actions-row" style="margin-bottom: 12px;">
      <button class="btn-start" onclick="startDaemon()">
        <span class="btn-title">Start Local Daemon</span>
        <span class="btn-desc">Spawns background daemon + 250ms watchdog from this single executable</span>
      </button>
      <button class="btn-start" onclick="setupNetwork()">
        <span class="btn-title">1-Click Setup 1GbE Network</span>
        <span class="btn-desc">Configures 192.168.137.x static subnet, NAT passthrough &amp; udev/Firewall</span>
      </button>
    </div>
    <div class="actions-row">
      <button class="btn-panic" onclick="triggerPanic()">
        <span class="btn-title">EMERGENCY PANIC / UNGRAB ALL (Ctrl+Alt+Shift+Bksp)</span>
        <span class="btn-desc">Immediately releases input grabs, unsticks modifiers &amp; forces Mode 0 Idle</span>
      </button>
    </div>
  </div>

<script>
async function postAction(url) {
  try {
    const res = await fetch(url, { method: 'POST' });
    const data = await res.json();
    document.getElementById('toast').textContent = data.message || 'Command sent.';
    refreshStatus();
  } catch (e) {
    document.getElementById('toast').textContent = 'Request failed: ' + e;
  }
}
function setMode(target) { postAction('/api/mode?target=' + encodeURIComponent(target)); }
function triggerPanic() { postAction('/api/panic'); }
function startDaemon() { postAction('/api/start-daemon'); }
function setupNetwork() { postAction('/api/setup-network'); }

async function refreshStatus() {
  try {
    const res = await fetch('/api/status');
    const s = await res.json();
    const dBadge = document.getElementById('daemon-badge');
    if (s.daemon_online) {
      dBadge.className = 'badge ok';
      dBadge.textContent = 'DAEMON ONLINE (PID ' + s.daemon_pid + ')';
    } else {
      dBadge.className = 'badge err';
      dBadge.textContent = 'DAEMON OFFLINE';
    }
    const pBadge = document.getElementById('peer-badge');
    if (s.peer_connected) {
      pBadge.className = 'badge ok';
      pBadge.textContent = '1GbE PEER CONNECTED';
    } else {
      pBadge.className = 'badge warn';
      pBadge.textContent = 'PEER LINK STANDBY';
    }
    document.getElementById('val-mode').textContent = s.mode_label;
    document.getElementById('sub-mode').textContent = s.mode_subtitle;
    document.getElementById('val-grab').textContent =
      'Grab: ' + (s.input_grab_active ? 'ACTIVE' : 'OFF') + ' | Displays: ' + s.virtual_displays_active;
    document.getElementById('sub-hb').textContent =
      'Daemon HB: ' + s.daemon_age_ms.toFixed(1) + ' ms | Peer HB: ' + s.peer_age_ms.toFixed(1) + ' ms';
    document.getElementById('val-rtt').textContent =
      s.min_rtt_us.toFixed(1) + ' / ' + s.last_rtt_us.toFixed(1) + ' / ' + s.max_rtt_us.toFixed(1) + ' \u00b5s';
    document.getElementById('val-hid').textContent = s.tx_hid_packets + ' / ' + s.rx_hid_packets;
    document.getElementById('val-vid').textContent = s.tx_video_frames + ' / ' + s.rx_video_frames;
    document.getElementById('val-pli').textContent = String(s.pli_keyframes_requested);

    for (const code of [0, 1, 2, 3]) {
      const btn = document.getElementById('btn-mode-' + code);
      if (btn) btn.className = (s.mode_code === code && s.daemon_online) ? 'active' : '';
    }
  } catch (_) {}
}
setInterval(refreshStatus, 100);
refreshStatus();
</script>
</body>
</html>
"#;

fn handle_http_client(mut stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));

    let mut buf = [0u8; 2048];
    let n = match stream.read(&mut buf) {
        Ok(n) if n > 0 => n,
        _ => return,
    };
    let req = String::from_utf8_lossy(&buf[..n]);
    let first_line = req.lines().next().unwrap_or("");
    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or("GET");
    let path = parts.next().unwrap_or("/");

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
        let json = snap.to_json();
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

    if method == "POST" && path.starts_with("/api/start-daemon") {
        let msg = match start_local_daemon() {
            Ok(m) => m,
            Err(e) => e,
        };
        write_json_message(&mut stream, &msg);
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

    let not_found = "HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nNot Found";
    let _ = stream.write_all(not_found.as_bytes());
}

fn write_json_message(stream: &mut TcpStream, msg: &str) {
    let escaped = msg.replace('\\', "\\\\").replace('"', "\\\"");
    let body = format!("{{\"message\":\"{escaped}\"}}");
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(resp.as_bytes());
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
