use crate::TelemetrySnapshot;
use duplex_pal::local_link_address_available;
use std::net::Ipv4Addr;

#[derive(serde::Serialize)]
pub struct LinkStatus {
    pub local_ip: Ipv4Addr,
    pub address_configured: bool,
    pub daemon_outdated: bool,
    pub message: String,
    pub last_error: Option<String>,
}

pub fn link_status(snapshot: &TelemetrySnapshot) -> LinkStatus {
    let local_ip = if cfg!(target_os = "windows") {
        Ipv4Addr::new(192, 168, 137, 1)
    } else {
        Ipv4Addr::new(192, 168, 137, 2)
    };
    let address_configured = local_link_address_available(local_ip);
    let daemon_outdated = different_daemon_build(snapshot);
    let message = if !address_configured {
        if ethernet_carrier_off() {
            "Ethernet cable/adapter link is down. Check both cable ends and that the other computer is awake with Ethernet enabled.".into()
        } else {
            format!("PC-link address {local_ip} is missing. On Linux, connect the PC cable and click Connect PC link; on Windows, run Set up network.")
        }
    } else if !snapshot.daemon_online {
        "PC-link address is configured. Start the daemon on both computers.".into()
    } else if !snapshot.peer_connected {
        "No authenticated peer heartbeat. Check the other daemon, matching pairing fingerprints, and firewall UDP 48100–48103/TCP 48101.".into()
    } else {
        "Authenticated peer connected. Mode 1 sends laptop input to Windows; Mode 2 sends Windows input to the laptop.".into()
    };
    LinkStatus {
        local_ip,
        address_configured,
        daemon_outdated,
        message,
        last_error: recent_error(),
    }
}

#[cfg(target_os = "linux")]
fn ethernet_carrier_off() -> bool {
    let Ok(entries) = std::fs::read_dir("/sys/class/net") else {
        return false;
    };
    let carriers: Vec<_> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if !path.join("device").exists()
                || path.join("wireless").exists()
                || std::fs::read_to_string(path.join("type")).ok()?.trim() != "1"
            {
                return None;
            }
            std::fs::read_to_string(path.join("carrier")).ok()
        })
        .collect();
    !carriers.is_empty() && carriers.iter().all(|value| value.trim() == "0")
}

#[cfg(not(target_os = "linux"))]
fn ethernet_carrier_off() -> bool {
    false
}

#[cfg(target_os = "linux")]
fn different_daemon_build(snapshot: &TelemetrySnapshot) -> bool {
    use std::os::unix::fs::MetadataExt;
    if !snapshot.daemon_online {
        return false;
    }
    match (
        std::fs::metadata(format!("/proc/{}/exe", snapshot.daemon_pid)),
        std::fs::metadata("/proc/self/exe"),
    ) {
        (Ok(daemon), Ok(gui)) => daemon.ino() != gui.ino() || daemon.dev() != gui.dev(),
        _ => false,
    }
}

#[cfg(target_os = "windows")]
pub(crate) fn different_daemon_build(snapshot: &TelemetrySnapshot) -> bool {
    use std::os::windows::ffi::OsStringExt;
    use windows::core::PWSTR;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    if !snapshot.daemon_online {
        return false;
    }
    let Ok(gui) = std::env::current_exe() else {
        return false;
    };
    let mut buffer = vec![0u16; 32768];
    let mut length = buffer.len() as u32;
    let queried = unsafe {
        let Ok(process) = OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION,
            false,
            snapshot.daemon_pid,
        ) else {
            return false;
        };
        let result = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_FORMAT(0),
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        );
        let _ = CloseHandle(process);
        result
    };
    if queried.is_err() {
        return false;
    }
    let daemon =
        std::path::PathBuf::from(std::ffi::OsString::from_wide(&buffer[..length as usize]));
    let daemon = std::fs::canonicalize(&daemon).unwrap_or(daemon);
    let gui = std::fs::canonicalize(&gui).unwrap_or(gui);
    !daemon
        .to_string_lossy()
        .eq_ignore_ascii_case(&gui.to_string_lossy())
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn different_daemon_build(_: &TelemetrySnapshot) -> bool {
    false
}

pub fn connect_pc_link() -> Result<String, String> {
    let snapshot = TelemetrySnapshot::read_current();
    if snapshot.daemon_online && snapshot.mode != duplex_proto::OperationalMode::Mode0Idle {
        return Err("Return to Idle before reconnecting the PC network link.".into());
    }
    #[cfg(target_os = "linux")]
    {
        let output = std::process::Command::new("nmcli")
            .args(["--wait", "10", "connection", "up", "duplex-kvm-1gbe"])
            .output()
            .map_err(|e| format!("Cannot activate PC link: {e}"))?;
        if !output.status.success() {
            return Err(format!("PC-link activation failed: {}. Run Set up network if the profile is missing; check the cable if no carrier is available.", String::from_utf8_lossy(&output.stderr).trim()));
        }
        Ok("PC-link profile activated. Waiting for the peer daemon; the saved internet-sharing settings are preserved.".into())
    }
    #[cfg(target_os = "windows")]
    {
        crate::run_embedded_network_setup()
    }
}

fn recent_error() -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let path = crate::updater::get_config_dir().join("logs/daemon.log");
    let mut file = std::fs::File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(length.saturating_sub(32_768)))
        .ok()?;
    let mut text = String::new();
    file.read_to_string(&mut text).ok()?;
    let line = text.lines().rev().find(|line| line.contains("ERROR"))?;
    let mut plain = String::new();
    let mut escape = false;
    for character in line.chars() {
        if character == '\u{1b}' {
            escape = true;
        } else if escape {
            if character == 'm' {
                escape = false;
            }
        } else {
            plain.push(character);
        }
    }
    Some(plain.chars().take(1200).collect())
}
