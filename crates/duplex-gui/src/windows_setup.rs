//! Explicit UAC elevation of this executable; no writable temporary script or
//! nested PowerShell launcher. Network commands remain readable in the source.

use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows::Win32::UI::Shell::{IsUserAnAdmin, ShellExecuteW};
use windows::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, MB_ICONINFORMATION, MB_OK, SW_SHOWNORMAL,
};

pub fn request_elevated_setup() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe: Vec<u16> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
    let result = unsafe {
        ShellExecuteW(
            HWND::default(),
            w!("runas"),
            PCWSTR(exe.as_ptr()),
            w!("--setup-elevated"),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    if result.0 as isize <= 32 {
        return Err("Windows network setup was cancelled or could not be elevated".into());
    }
    Ok("Requested Windows network setup through UAC. A separate window reports the result.".into())
}

fn configure_network() -> Result<(), String> {
    if !unsafe { IsUserAnAdmin() }.as_bool() {
        return Err("Network setup requires administrator approval through UAC".into());
    }
    // Resolve the OS tool without searching a writable working directory or PATH.
    let mut directory = [0u16; 32768];
    let len = unsafe { GetSystemDirectoryW(Some(&mut directory)) } as usize;
    if len == 0 || len >= directory.len() {
        return Err("Cannot resolve the Windows system directory".into());
    }
    let powershell = PathBuf::from(OsString::from_wide(&directory[..len]))
        .join("WindowsPowerShell/v1.0/powershell.exe");
    // Use the compile-time commands directly. No temporary script can be swapped
    // between the original user's launch and elevated execution. No execution
    // policy setting, encoded payload, or protection exclusion is used.
    let commands = format!("& {{\n{}\n}}", super::EMBEDDED_WINDOWS_SETUP_PS1);
    let status = Command::new(powershell)
        .args(["-NoProfile", "-NonInteractive", "-Command", &commands])
        .stdin(Stdio::null())
        .status()
        .map_err(|e| format!("Cannot run Windows network setup: {e}"))?;
    if !status.success() {
        return Err(format!(
            "Windows network setup failed ({status}). Review the setup console output."
        ));
    }
    Ok(())
}

pub fn run_elevated_setup() {
    let result = configure_network();
    let text = match &result {
        Ok(()) => "Windows network settings were saved. The USB Ethernet adapter must be recognized and its physical cable link must be up; start both daemons after connecting the laptop.".to_string(),
        Err(error) => error.clone(),
    };
    let text: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    unsafe {
        MessageBoxW(
            HWND::default(),
            PCWSTR(text.as_ptr()),
            w!("Duplex-KVM network setup"),
            MB_OK | MB_ICONINFORMATION,
        );
    }
    if result.is_err() {
        std::process::exit(1);
    }
}

pub fn open_browser(url: &str) {
    let url: Vec<u16> = url.encode_utf16().chain(Some(0)).collect();
    unsafe {
        ShellExecuteW(
            HWND::default(),
            w!("open"),
            PCWSTR(url.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}
