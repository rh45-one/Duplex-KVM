//! User-specific persisted pairing key. No shared/public default is accepted.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub fn key_path() -> Result<PathBuf, String> {
    #[cfg(target_os = "windows")]
    let directory = std::env::var_os("APPDATA").map(|p| PathBuf::from(p).join("Duplex-KVM"));
    #[cfg(not(target_os = "windows"))]
    let directory = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|p| !p.is_empty())
        .map(|p| PathBuf::from(p).join("duplex-kvm"))
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".config/duplex-kvm")));
    directory
        .map(|p| p.join("link.key"))
        .ok_or_else(|| "Cannot locate your user configuration directory for pairing".to_string())
}

pub fn parse_key(value: &str) -> Result<[u8; 32], String> {
    let value = value.trim();
    if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("The pairing key must contain exactly 64 hexadecimal digits".into());
    }
    let mut key = [0; 32];
    for (i, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[i * 2..i * 2 + 2], 16).unwrap();
    }
    if key == *b"DuplexKvm-1GbE-Symmetric-Key-V01" || key == [0; 32] {
        return Err(
            "Generate a private random pairing key; public and all-zero keys are unsafe".into(),
        );
    }
    Ok(key)
}

pub fn load_key() -> Result<[u8; 32], String> {
    match std::env::var("DUPLEX_PSK_HEX") {
        Ok(value) => parse_key(&value).map_err(|e| format!("DUPLEX_PSK_HEX: {e}")),
        Err(std::env::VarError::NotPresent) => load_saved_key(&key_path()?),
        Err(_) => Err("DUPLEX_PSK_HEX is not valid text".into()),
    }
}

fn load_saved_key(path: &Path) -> Result<[u8; 32], String> {
    let text = fs::read_to_string(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            "Pairing is not configured. Open the Web control center → Pair computers. Generate a key on one computer, then save that same key on the other.".to_string()
        } else {
            format!("Cannot read saved pairing key: {error}")
        }
    })?;
    parse_key(&text)
}

pub fn generate_key() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|e| format!("Cannot generate pairing key: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

pub fn save_key(value: &str) -> Result<(), String> {
    // An inherited override still wins on daemon startup; don't pretend a saved
    // value will fix an invalid/different environment override.
    if std::env::var_os("DUPLEX_PSK_HEX").is_some() {
        return Err("DUPLEX_PSK_HEX is set for this application. Clear it and reopen the application to use saved pairing, or set the same override on both computers.".into());
    }
    save_key_at(&key_path()?, value)
}

fn save_key_at(path: &Path, value: &str) -> Result<(), String> {
    parse_key(value)?;
    let directory = path.parent().ok_or("Pairing directory is missing")?;
    fs::create_dir_all(directory).map_err(|e| format!("Cannot create pairing directory: {e}"))?;
    let staging = directory.join(format!(
        ".link-key-{}-{}",
        std::process::id(),
        generate_key()?
    ));
    let result = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&staging)
            .map_err(|e| format!("Cannot create pairing file: {e}"))?;
        file.write_all(value.trim().to_lowercase().as_bytes())
            .map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        drop(file);
        #[cfg(not(target_os = "windows"))]
        fs::rename(&staging, path).map_err(|e| format!("Cannot save pairing: {e}"))?;
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::ffi::OsStrExt;
            use windows::core::PCWSTR;
            use windows::Win32::Storage::FileSystem::{
                MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
            };
            let source: Vec<u16> = staging.as_os_str().encode_wide().chain(Some(0)).collect();
            let target: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            unsafe {
                MoveFileExW(
                    PCWSTR(source.as_ptr()),
                    PCWSTR(target.as_ptr()),
                    MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
                )
            }
            .map_err(|e| format!("Cannot save pairing: {e}"))?;
        }
        Ok(())
    })();
    let _ = fs::remove_file(&staging);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_keys_are_distinct_valid_and_not_public() {
        let first = generate_key().unwrap();
        let second = generate_key().unwrap();
        assert_ne!(first, second);
        assert!(parse_key(&first).is_ok());
        assert!(parse_key(&"0".repeat(64)).is_err());
        assert!(parse_key("not a key").is_err());
    }

    #[test]
    fn pairing_persists_and_invalid_import_does_not_replace_it() {
        let dir =
            std::env::temp_dir().join(format!("duplex-pair-test-{}", generate_key().unwrap()));
        let path = dir.join("link.key");
        assert!(load_saved_key(&path)
            .unwrap_err()
            .contains("Pair computers"));
        let first = generate_key().unwrap();
        save_key_at(&path, &first.to_uppercase()).unwrap();
        assert_eq!(load_saved_key(&path).unwrap(), parse_key(&first).unwrap());
        assert!(save_key_at(&path, "bad").is_err());
        assert_eq!(load_saved_key(&path).unwrap(), parse_key(&first).unwrap());
        let second = generate_key().unwrap();
        save_key_at(&path, &second).unwrap();
        assert_eq!(load_saved_key(&path).unwrap(), parse_key(&second).unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        fs::remove_dir_all(dir).unwrap();
    }
}
