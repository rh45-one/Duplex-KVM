//! Cross-Platform In-App Automatic & Manual Updater for Duplex-KVM.
//!
//! Features:
//! - Full support for **PRIVATE** GitHub repositories using Personal Access Tokens (PAT)
//!   via `DUPLEX_GITHUB_TOKEN`, `GITHUB_TOKEN`, or local config file.
//! - Queries `api.github.com/repos/<owner>/<repo>/releases/latest`.
//! - Authenticated release asset stream download via `api.github.com/repos/.../releases/assets/<id>`.
//! - Automatic stripping of `Authorization` headers when following 302 redirects to AWS S3.
//! - Cryptographic SHA256 verification against published `SHA256SUMS.txt`.
//! - In-place atomic self-replacement of the running executable on Debian Linux and Windows 11.
//! - Automatic background checking on startup and periodic 4-hour cadence.
//! - Manual trigger via Operator CLI (`duplex-kvm update`), Web UI, and Desktop GUI.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const DEFAULT_REPO: &str = "rh45-one/Duplex-KVM";
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default = "default_repo")]
    pub repo: String,
    #[serde(default)]
    pub github_token: Option<String>,
    #[serde(default = "default_true")]
    pub auto_check_updates: bool,
    #[serde(default)]
    pub auto_install_updates: bool,
    #[serde(default)]
    pub last_check_epoch: u64,
    #[serde(default)]
    pub latest_known_version: Option<String>,
}

fn default_repo() -> String {
    DEFAULT_REPO.to_string()
}

fn default_true() -> bool {
    true
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            repo: DEFAULT_REPO.to_string(),
            github_token: None,
            auto_check_updates: true,
            auto_install_updates: false,
            last_check_epoch: 0,
            latest_known_version: None,
        }
    }
}

pub fn get_config_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        if let Ok(appdata) = env::var("APPDATA") {
            PathBuf::from(appdata).join("Duplex-KVM")
        } else {
            env::temp_dir().join("Duplex-KVM")
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        if let Ok(xdg) = env::var("XDG_CONFIG_HOME") {
            PathBuf::from(xdg).join("duplex-kvm")
        } else if let Ok(home) = env::var("HOME") {
            PathBuf::from(home).join(".config").join("duplex-kvm")
        } else {
            env::temp_dir().join("duplex-kvm")
        }
    }
}

pub fn get_config_path() -> PathBuf {
    get_config_dir().join("config.json")
}

impl AppConfig {
    pub fn load() -> Self {
        let path = get_config_path();
        if let Ok(content) = fs::read_to_string(&path) {
            if let Ok(cfg) = serde_json::from_str::<AppConfig>(&content) {
                return cfg;
            }
        }
        AppConfig::default()
    }

    pub fn save(&self) -> Result<(), String> {
        let dir = get_config_dir();
        fs::create_dir_all(&dir).map_err(|e| format!("Failed to create config dir: {e}"))?;
        let path = get_config_path();
        let content = serde_json::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize config: {e}"))?;
        write_private_config(&path, content.as_bytes())?;
        Ok(())
    }

    pub fn effective_token(&self) -> Option<String> {
        if let Ok(t) = env::var("DUPLEX_GITHUB_TOKEN") {
            if !t.trim().is_empty() {
                return Some(t.trim().to_string());
            }
        }
        if let Ok(t) = env::var("GITHUB_TOKEN") {
            if !t.trim().is_empty() {
                return Some(t.trim().to_string());
            }
        }
        self.github_token
            .as_ref()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
    }

    pub fn effective_repo(&self) -> String {
        if let Ok(r) = env::var("DUPLEX_REPO") {
            if !r.trim().is_empty() {
                return r.trim().to_string();
            }
        }
        if !self.repo.trim().is_empty() {
            self.repo.trim().to_string()
        } else {
            DEFAULT_REPO.to_string()
        }
    }

    pub fn masked_token(&self) -> Option<String> {
        self.effective_token().map(|t| mask_token(&t))
    }
}

fn write_private_config(path: &Path, content: &[u8]) -> Result<(), String> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|e| format!("Cannot open config: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("Cannot protect config: {e}"))?;
    }
    file.write_all(content)
        .map_err(|e| format!("Failed to write config: {e}"))
}

fn validate_repo(repo: &str) -> Result<(), String> {
    let parts: Vec<_> = repo.split('/').collect();
    if parts.len() != 2
        || parts.iter().any(|p| {
            p.is_empty()
                || *p == "."
                || *p == ".."
                || !p
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
        })
    {
        return Err("Repository must be an owner/repository pair".to_string());
    }
    Ok(())
}

pub fn mask_token(token: &str) -> String {
    let t = token.trim();
    if t.len() <= 8 {
        "••••••••".to_string()
    } else {
        let prefix: String = t.chars().take(4).collect();
        let suffix: String = t
            .chars()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        format!("{prefix}••••••••{suffix}")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseAsset {
    pub id: u64,
    pub name: String,
    pub size: u64,
    pub url: String,
    #[serde(default)]
    pub browser_download_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubRelease {
    pub tag_name: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub html_url: String,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateStatus {
    pub current_version: String,
    pub latest_version: Option<String>,
    pub update_available: bool,
    pub release_notes: Option<String>,
    pub release_url: Option<String>,
    pub last_checked: Option<String>,
    pub is_checking: bool,
    pub is_downloading: bool,
    pub last_error: Option<String>,
    pub token_configured: bool,
    pub restart_required: bool,
    pub target_asset_name: String,
}

impl Default for UpdateStatus {
    fn default() -> Self {
        let cfg = AppConfig::load();
        Self {
            current_version: format!("v{CURRENT_VERSION}"),
            latest_version: None,
            update_available: false,
            release_notes: None,
            release_url: None,
            last_checked: None,
            is_checking: false,
            is_downloading: false,
            last_error: None,
            token_configured: cfg.effective_token().is_some(),
            restart_required: false,
            target_asset_name: expected_asset_name().to_string(),
        }
    }
}

static UPDATE_STATUS: Mutex<Option<UpdateStatus>> = Mutex::new(None);
static UPDATE_OPERATION: Mutex<()> = Mutex::new(());

pub fn get_update_status() -> UpdateStatus {
    let mut guard = UPDATE_STATUS.lock().unwrap();
    if guard.is_none() {
        *guard = Some(UpdateStatus::default());
    }
    let mut s = guard.clone().unwrap();
    let cfg = AppConfig::load();
    s.token_configured = cfg.effective_token().is_some();
    s
}

fn set_update_status(f: impl FnOnce(&mut UpdateStatus)) {
    let mut guard = UPDATE_STATUS.lock().unwrap();
    if guard.is_none() {
        *guard = Some(UpdateStatus::default());
    }
    if let Some(ref mut s) = *guard {
        f(s);
    }
}

pub fn parse_semver(s: &str) -> (u32, u32, u32) {
    let s = s.trim().trim_start_matches('v').trim_start_matches('V');
    let mut parts = s.split('.');
    let major = parts.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    let minor = parts.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    let patch = parts
        .next()
        .and_then(|x| x.split('-').next().unwrap_or("0").parse().ok())
        .unwrap_or(0);
    (major, minor, patch)
}

pub fn is_newer_version(remote: &str, current: &str) -> bool {
    let remote = remote.trim().trim_start_matches(['v', 'V']);
    let stable = remote.split('+').next().unwrap_or(remote);
    if stable.contains('-')
        || stable.split('.').count() != 3
        || stable
            .split('.')
            .any(|part| part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()))
    {
        return false;
    }
    parse_semver(stable) > parse_semver(current)
}

pub fn expected_asset_name() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        "duplex-kvm-windows-x86_64.exe"
    }
    #[cfg(target_os = "linux")]
    {
        "duplex-kvm-linux-x86_64"
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        "duplex-kvm"
    }
}

/// Executes an HTTP GET request with optional GitHub token authorization.
pub fn http_get(url: &str, token: Option<&str>, accept_header: &str) -> Result<Vec<u8>, String> {
    if !url.starts_with("https://")
        || (token.is_some() && !url.starts_with("https://api.github.com/"))
    {
        return Err("Authenticated downloads require the HTTPS GitHub API".to_string());
    }
    let mut cmd = Command::new("curl");
    cmd.args([
        "--silent",
        "--show-error",
        "--fail",
        "--location",
        "--proto",
        "=https",
        "--proto-redir",
        "=https",
        "--connect-timeout",
        "15",
        "--max-time",
        "180",
    ]);
    cmd.args(["-H", &format!("Accept: {accept_header}")]);
    cmd.args(["-A", "duplex-kvm-updater"]);
    if let Some(tok) = token {
        cmd.args(["-H", &format!("Authorization: Bearer {tok}")]);
    }
    cmd.arg(url);

    match cmd.output() {
        Ok(output) => {
            if output.status.success() {
                return Ok(output.stdout);
            }
            let err_str = String::from_utf8_lossy(&output.stderr);
            if token.is_none() && (err_str.contains("404") || output.status.code() == Some(22)) {
                return Err(
                    "Repository is private or release was not found. A GitHub Personal Access Token is required to access private releases. Set DUPLEX_GITHUB_TOKEN or run 'duplex-kvm config set-token <token>'."
                        .to_string(),
                );
            }
            if !err_str.trim().is_empty() {
                return Err(format!("HTTP request failed: {}", err_str.trim()));
            }
            Err(format!("HTTP request failed with status {}", output.status))
        }
        Err(e) => {
            #[cfg(target_os = "windows")]
            {
                let _ = &e;
                http_get_powershell(url, token, accept_header)
            }
            #[cfg(not(target_os = "windows"))]
            {
                Err(format!("curl execution failed: {e}"))
            }
        }
    }
}

#[cfg(target_os = "windows")]
fn http_get_powershell(
    url: &str,
    token: Option<&str>,
    accept_header: &str,
) -> Result<Vec<u8>, String> {
    let tmp_dir = env::temp_dir().join(format!(
        "duplex-download-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    fs::create_dir(&tmp_dir).map_err(|e| format!("Cannot create download directory: {e}"))?;
    let tmp_file = tmp_dir.join("asset");
    // Values are passed in the environment, never interpolated into PowerShell code.
    let ps_script = r#"
$ErrorActionPreference = 'Stop'
try {
    $uri = [Uri]$env:DUPLEX_UPDATE_URL
    for ($i = 0; $i -lt 6; $i++) {
        if ($uri.Scheme -ne 'https') { throw 'Update redirect must use HTTPS' }
        $req = [System.Net.HttpWebRequest]::Create($uri)
        $req.UserAgent = 'duplex-kvm-updater'
        $req.Accept = $env:DUPLEX_UPDATE_ACCEPT
        $req.Timeout = 15000
        $req.ReadWriteTimeout = 180000
        $req.AllowAutoRedirect = $false
        if ($uri.Host -eq 'api.github.com' -and $env:DUPLEX_UPDATE_TOKEN) {
            $req.Headers.Add('Authorization', 'Bearer ' + $env:DUPLEX_UPDATE_TOKEN)
        }
        $resp = $req.GetResponse()
        if ([int]$resp.StatusCode -ge 300 -and [int]$resp.StatusCode -lt 400) {
            $uri = [Uri]::new($uri, $resp.GetResponseHeader('Location'))
            $resp.Close()
            continue
        }
        $stream = $resp.GetResponseStream()
        $file = [System.IO.File]::Create($env:DUPLEX_UPDATE_OUT)
        try { $stream.CopyTo($file) } finally { $file.Close(); $stream.Close(); $resp.Close() }
        exit 0
    }
    throw 'Too many update redirects'
} catch { [Console]::Error.WriteLine($_.Exception.Message); exit 1 }
"#;
    let result = (|| {
        let output = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", ps_script])
            .env("DUPLEX_UPDATE_URL", url)
            .env("DUPLEX_UPDATE_TOKEN", token.unwrap_or(""))
            .env("DUPLEX_UPDATE_ACCEPT", accept_header)
            .env("DUPLEX_UPDATE_OUT", &tmp_file)
            .output()
            .map_err(|e| format!("PowerShell fallback failed: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "PowerShell request failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        fs::read(&tmp_file).map_err(|e| format!("Cannot read downloaded asset: {e}"))
    })();
    let _ = fs::remove_dir_all(tmp_dir);
    result
}

/// Downloads a binary release asset directly to disk.
pub fn download_file(
    url: &str,
    token: Option<&str>,
    accept_header: &str,
    dest_path: &Path,
) -> Result<(), String> {
    if !url.starts_with("https://")
        || (token.is_some() && !url.starts_with("https://api.github.com/"))
    {
        return Err("Authenticated downloads require the HTTPS GitHub API".to_string());
    }
    let mut cmd = Command::new("curl");
    cmd.args([
        "--silent",
        "--show-error",
        "--fail",
        "--location",
        "--proto",
        "=https",
        "--proto-redir",
        "=https",
        "--connect-timeout",
        "15",
        "--max-time",
        "180",
    ]);
    cmd.args(["-H", &format!("Accept: {accept_header}")]);
    cmd.args(["-A", "duplex-kvm-updater"]);
    if let Some(tok) = token {
        cmd.args(["-H", &format!("Authorization: Bearer {tok}")]);
    }
    cmd.arg(url);
    cmd.args(["-o", &dest_path.to_string_lossy()]);

    match cmd.output() {
        Ok(output) => {
            if output.status.success() {
                if dest_path.exists()
                    && fs::metadata(dest_path)
                        .map(|m| m.len() > 0)
                        .unwrap_or(false)
                {
                    return Ok(());
                }
                return Err("Downloaded asset file is missing or invalid".to_string());
            }
            let err_str = String::from_utf8_lossy(&output.stderr);
            if token.is_none() && (err_str.contains("404") || output.status.code() == Some(22)) {
                return Err(
                    "Repository is private or asset not found. A GitHub Personal Access Token is required to download assets from private repositories."
                        .to_string(),
                );
            }
            Err(format!("Asset download failed: {}", err_str.trim()))
        }
        Err(_) => {
            #[cfg(target_os = "windows")]
            {
                let bytes = http_get_powershell(url, token, accept_header)?;
                fs::write(dest_path, bytes)
                    .map_err(|e| format!("Failed to write asset to disk: {e}"))?;
                Ok(())
            }
            #[cfg(not(target_os = "windows"))]
            {
                Err("curl is required to download updates".to_string())
            }
        }
    }
}

pub fn verify_sha256(file_path: &Path, expected_hex: &str) -> Result<bool, String> {
    use std::io::Read;
    let mut file =
        fs::File::open(file_path).map_err(|e| format!("Failed to open file for SHA256: {e}"))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("Failed to read file for hashing: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let hash = hasher.finalize();
    let mut calculated = String::with_capacity(64);
    for b in hash.iter() {
        use std::fmt::Write;
        let _ = write!(&mut calculated, "{:02x}", b);
    }
    Ok(calculated.eq_ignore_ascii_case(expected_hex.trim()))
}

pub fn find_checksum_in_sums(sums_txt: &str, filename: &str) -> Option<String> {
    for line in sums_txt.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 {
            let hash = parts[0];
            let name = parts[1].trim_start_matches('*');
            if name == filename && hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Some(hash.to_string());
            }
        }
    }
    None
}

fn verify_release_download(
    path: &Path,
    asset: &ReleaseAsset,
    assets: &[ReleaseAsset],
    fetch_sums: impl FnOnce(&ReleaseAsset) -> Result<Vec<u8>, String>,
) -> Result<(), String> {
    let size = fs::metadata(path).map_err(|e| e.to_string())?.len();
    if size != asset.size || size == 0 {
        return Err("Downloaded asset size does not match GitHub metadata".to_string());
    }
    let sums_asset = assets
        .iter()
        .find(|a| a.name == "SHA256SUMS.txt")
        .ok_or_else(|| "Release is missing SHA256SUMS.txt".to_string())?;
    let sums_bytes = fetch_sums(sums_asset)?;
    let sums = std::str::from_utf8(&sums_bytes).map_err(|e| e.to_string())?;
    let expected_hash = find_checksum_in_sums(sums, &asset.name)
        .ok_or_else(|| "Release checksum is missing or malformed".to_string())?;
    if !verify_sha256(path, &expected_hash)? {
        return Err("SHA256 checksum verification failed for downloaded binary".to_string());
    }
    Ok(())
}

/// Atomically replaces target executable with the newly downloaded binary.
pub fn replace_executable_at(new_binary: &Path, target_exe: &Path) -> Result<(), String> {
    let target_exe = target_exe.canonicalize().unwrap_or_else(|_| target_exe.to_path_buf());

    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(new_binary, fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("Failed to set permissions: {e}"))?;

        if let Err(e) = fs::rename(new_binary, &target_exe) {
            if e.kind() == std::io::ErrorKind::PermissionDenied {
                return Err(format!(
                    "Permission denied updating '{}'. Run with sudo / elevated permissions: {e}",
                    target_exe.display()
                ));
            }
            if let Some(parent) = target_exe.parent() {
                let tmp_staged = parent.join(format!(".duplex-kvm-upd-{}", std::process::id()));
                fs::copy(new_binary, &tmp_staged)
                    .map_err(|e| format!("Cannot stage update: {e}"))?;
                fs::set_permissions(&tmp_staged, fs::Permissions::from_mode(0o755))
                    .map_err(|e| format!("Cannot make update executable: {e}"))?;
                fs::rename(&tmp_staged, &target_exe).map_err(|e2| {
                    format!(
                        "Failed to replace executable '{}': {e2}",
                        target_exe.display()
                    )
                })?;
                let _ = fs::remove_file(new_binary);
                return Ok(());
            }
            return Err(format!(
                "Failed to replace binary '{}': {e}",
                target_exe.display()
            ));
        }
        Ok(())
    }

    #[cfg(target_os = "windows")]
    {
        let backup_exe = target_exe.with_extension("exe.old");
        if backup_exe.exists() {
            let _ = fs::remove_file(&backup_exe);
        }

        fs::rename(&target_exe, &backup_exe)
            .map_err(|e| format!("Failed to rename running executable to '{backup_exe:?}': {e}"))?;

        if let Err(e) = fs::rename(new_binary, &target_exe) {
            let _ = fs::rename(&backup_exe, &target_exe);
            return Err(format!(
                "Failed to install new executable into '{target_exe:?}': {e}"
            ));
        }

        let _ = fs::remove_file(&backup_exe);
        Ok(())
    }
}

/// Atomically replaces the current running executable with the newly downloaded binary.
pub fn self_replace_executable(new_binary: &Path) -> Result<(), String> {
    let current_exe =
        env::current_exe().map_err(|e| format!("Failed to resolve current executable: {e}"))?;
    replace_executable_at(new_binary, &current_exe)
}

pub fn clean_old_backups() {
    #[cfg(target_os = "windows")]
    {
        if let Ok(exe) = env::current_exe() {
            let old = exe.with_extension("exe.old");
            if old.exists() {
                let _ = fs::remove_file(old);
            }
        }
    }
}

/// Checks the repository releases for a newer version.
pub fn check_for_updates(force: bool) -> Result<UpdateStatus, String> {
    let _operation = UPDATE_OPERATION
        .try_lock()
        .map_err(|_| "An update operation is already running".to_string())?;
    let result = check_for_updates_inner(force);
    if let Err(ref error) = result {
        set_update_status(|s| {
            s.is_checking = false;
            s.last_error = Some(error.clone());
        });
    }
    result
}

fn fetch_latest_release(config: &AppConfig) -> Result<GitHubRelease, String> {
    let repo = config.effective_repo();
    validate_repo(&repo)?;
    let bytes = http_get(
        &format!("https://api.github.com/repos/{repo}/releases/latest"),
        config.effective_token().as_deref(),
        "application/vnd.github+json",
    )?;
    serde_json::from_slice(&bytes).map_err(|e| format!("Invalid GitHub release JSON: {e}"))
}

fn check_for_updates_inner(force: bool) -> Result<UpdateStatus, String> {
    set_update_status(|s| {
        s.is_checking = true;
        s.last_error = None;
    });

    let config = AppConfig::load();
    let release = match fetch_latest_release(&config) {
        Ok(release) => release,
        Err(error) => {
            set_update_status(|s| {
                s.is_checking = false;
                s.update_available = false;
                s.last_error = Some(error.clone());
            });
            return Err(error);
        }
    };

    let remote_ver = release.tag_name.clone();
    let has_newer = is_newer_version(&remote_ver, CURRENT_VERSION);

    // Save timestamp & version to config
    let now_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let mut cfg = AppConfig::load();
    cfg.last_check_epoch = now_epoch;
    cfg.latest_known_version = Some(remote_ver.clone());
    let _ = cfg.save();

    let expected_asset = expected_asset_name();
    let has_target_asset = release.assets.iter().any(|a| a.name == expected_asset);

    set_update_status(|s| {
        s.is_checking = false;
        s.latest_version = Some(remote_ver.clone());
        s.update_available = has_newer && has_target_asset;
        s.release_notes = release.body.clone();
        s.release_url = Some(release.html_url.clone());
        s.last_checked = Some(format!("{now_epoch}"));
        s.target_asset_name = expected_asset.to_string();
        if has_newer && !has_target_asset {
            s.last_error = Some(format!(
                "Release {remote_ver} exists, but asset '{expected_asset}' is not attached."
            ));
        }
    });

    // Handle auto-install if enabled and an update is ready
    if has_newer && has_target_asset && config.auto_install_updates && !force {
        install_update_inner()?;
    }

    Ok(get_update_status())
}

/// Downloads and installs the latest release update.
pub fn install_update() -> Result<String, String> {
    let _operation = UPDATE_OPERATION
        .try_lock()
        .map_err(|_| "An update operation is already running".to_string())?;
    let result = install_update_inner();
    if let Err(ref error) = result {
        set_update_status(|s| {
            s.is_downloading = false;
            s.last_error = Some(error.clone());
        });
    }
    result
}

struct InstallStatusGuard;
impl Drop for InstallStatusGuard {
    fn drop(&mut self) {
        set_update_status(|s| s.is_downloading = false);
    }
}

fn install_update_inner() -> Result<String, String> {
    if get_update_status().restart_required {
        return Err("Restart the application to finish the installed update".to_string());
    }
    let _status_guard = InstallStatusGuard;
    set_update_status(|s| {
        s.is_downloading = true;
        s.last_error = None;
    });

    let config = AppConfig::load();
    let repo = config.effective_repo();
    let token = config.effective_token();

    // Refresh with the same repository/token snapshot used for asset downloads.
    let release = fetch_latest_release(&config)?;
    if !is_newer_version(&release.tag_name, CURRENT_VERSION) {
        return Err("No newer stable release is available".to_string());
    }

    let expected_asset = expected_asset_name();
    let target_asset = release
        .assets
        .iter()
        .find(|a| a.name == expected_asset)
        .ok_or_else(|| {
            let err = format!(
                "Target binary asset '{expected_asset}' was not found in release {}",
                release.tag_name
            );
            set_update_status(|s| {
                s.is_downloading = false;
                s.last_error = Some(err.clone());
            });
            err
        })?;

    // Prepare temp file path in same directory as current executable (or temp dir)
    let current_exe = env::current_exe().map_err(|e| format!("Current exe error: {e}"))?;
    let temp_download = current_exe
        .parent()
        .map(|p| p.join(format!(".duplex-kvm-download-{}", std::process::id())))
        .unwrap_or_else(|| env::temp_dir().join(format!("duplex-kvm-{}.tmp", std::process::id())));

    // For private repo releases, download via the API asset URL with Accept: application/octet-stream
    let download_url = format!(
        "https://api.github.com/repos/{repo}/releases/assets/{}",
        target_asset.id
    );

    let dl_res = download_file(
        &download_url,
        token.as_deref(),
        "application/octet-stream",
        &temp_download,
    );

    if let Err(e) = dl_res {
        let _ = fs::remove_file(&temp_download);
        set_update_status(|s| {
            s.is_downloading = false;
            s.last_error = Some(e.clone());
        });
        return Err(e);
    }

    let verification = verify_release_download(
        &temp_download,
        target_asset,
        &release.assets,
        |sums_asset| {
            let sums_url = format!(
                "https://api.github.com/repos/{repo}/releases/assets/{}",
                sums_asset.id
            );
            http_get(&sums_url, token.as_deref(), "application/octet-stream")
        },
    );
    if let Err(error) = verification {
        let _ = fs::remove_file(&temp_download);
        return Err(error);
    }

    // Replace the running executable
    match self_replace_executable(&temp_download) {
        Ok(()) => {
            let _ = fs::remove_file(&temp_download);
            let msg = format!(
                "Successfully updated Duplex-KVM to {}! Please restart the application.",
                release.tag_name
            );
            set_update_status(|s| {
                s.is_downloading = false;
                s.update_available = false;
                s.restart_required = true;
                s.last_error = None;
            });
            Ok(msg)
        }
        Err(e) => {
            let _ = fs::remove_file(&temp_download);
            set_update_status(|s| {
                s.is_downloading = false;
                s.last_error = Some(e.clone());
            });
            Err(e)
        }
    }
}

/// Background periodic updater service (startup delay: 3s, cadence: 4 hours).
pub fn start_background_checker() {
    clean_old_backups();

    thread::spawn(|| {
        thread::sleep(Duration::from_secs(3));
        let config = AppConfig::load();
        if config.auto_check_updates {
            let _ = check_for_updates(false);
        }

        loop {
            thread::sleep(Duration::from_secs(4 * 3600));
            let config = AppConfig::load();
            if config.auto_check_updates {
                let _ = check_for_updates(false);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_semver() {
        assert_eq!(parse_semver("0.1.0"), (0, 1, 0));
        assert_eq!(parse_semver("v0.1.0"), (0, 1, 0));
        assert_eq!(parse_semver("V1.2.3"), (1, 2, 3));
        assert_eq!(parse_semver("v2.10.4-rc1"), (2, 10, 4));
    }

    #[test]
    fn test_is_newer_version() {
        assert!(is_newer_version("v0.2.0", "0.1.0"));
        assert!(is_newer_version("0.1.1", "0.1.0"));
        assert!(is_newer_version("v1.0.0", "0.9.9"));
        assert!(!is_newer_version("v0.1.0", "0.1.0"));
        assert!(!is_newer_version("0.0.9", "0.1.0"));
    }

    #[test]
    fn test_find_checksum_in_sums() {
        let hash = "ab".repeat(32);
        let sums =
            format!("{hash}  duplex-kvm-linux-x86_64\n{hash} *duplex-kvm-windows-x86_64.exe\n");
        assert_eq!(
            find_checksum_in_sums(&sums, "duplex-kvm-linux-x86_64"),
            Some(hash.clone())
        );
        assert_eq!(
            find_checksum_in_sums(&sums, "duplex-kvm-windows-x86_64.exe"),
            Some(hash)
        );
        assert_eq!(find_checksum_in_sums(&sums, "kvm-linux-x86_64"), None);
        assert_eq!(find_checksum_in_sums("abcd  file", "file"), None);
    }

    #[test]
    fn test_mask_token() {
        assert_eq!(mask_token("ghp_1234567890abcdef"), "ghp_••••••••cdef");
        assert_eq!(mask_token("short"), "••••••••");
    }

    #[test]
    fn test_config_serialization() {
        let cfg = AppConfig {
            repo: "rh45-one/Duplex-KVM".to_string(),
            github_token: Some("ghp_secret1234".to_string()),
            auto_check_updates: true,
            auto_install_updates: false,
            last_check_epoch: 123456789,
            latest_known_version: Some("v0.1.1".to_string()),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: AppConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.repo, "rh45-one/Duplex-KVM");
        assert_eq!(parsed.github_token, Some("ghp_secret1234".to_string()));
        assert!(parsed.auto_check_updates);
    }
    #[test]
    fn release_validation_rejects_missing_or_invalid_verification() {
        let dir = env::temp_dir().join(format!("duplex-update-test-{}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("binary");
        fs::write(&path, b"abc").unwrap();
        let asset = ReleaseAsset {
            id: 1,
            name: "binary".into(),
            size: 3,
            url: String::new(),
            browser_download_url: String::new(),
        };
        let sums_asset = ReleaseAsset {
            id: 2,
            name: "SHA256SUMS.txt".into(),
            ..asset.clone()
        };
        let assets = vec![asset.clone(), sums_asset];
        let sums = b"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  binary";
        assert!(verify_release_download(&path, &asset, &assets, |_| Ok(sums.to_vec())).is_ok());
        assert!(verify_release_download(&path, &asset, &[], |_| panic!(
            "missing asset must not fetch"
        ))
        .is_err());
        assert!(
            verify_release_download(&path, &asset, &assets, |_| Err("network failed".into()))
                .is_err()
        );
        assert!(
            verify_release_download(&path, &asset, &assets, |_| Ok(b"bad checksum".to_vec()))
                .is_err()
        );
        fs::write(&path, b"xyz").unwrap();
        assert!(verify_release_download(&path, &asset, &assets, |_| Ok(sums.to_vec())).is_err());
        fs::write(&path, b"a").unwrap();
        assert!(verify_release_download(&path, &asset, &assets, |_| panic!(
            "size mismatch must not fetch"
        ))
        .is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn repository_and_stable_version_validation() {
        assert!(validate_repo("rh45-one/Duplex-KVM").is_ok());
        for repo in [
            "../repo",
            "user/repo/extra",
            "user/repo?token=x",
            "https://example.com",
            "user/",
        ] {
            assert!(validate_repo(repo).is_err());
        }
        for tag in ["v1.0.0-rc1", "v2.0", "v2.0.0.1", "v2.x.0", ""] {
            assert!(!is_newer_version(tag, "0.1.0"));
        }
        assert!(is_newer_version("v1.0.0+build", "0.1.0"));
        assert!(!mask_token("éééééé").is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn saved_tokens_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let path = env::temp_dir().join(format!("duplex-config-test-{}", std::process::id()));
        fs::write(&path, b"old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        write_private_config(&path, b"secret").unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_replace_executable_at() {
        let dir = env::temp_dir().join(format!("duplex-replace-test-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let target_exe = dir.join("duplex-kvm");
        let new_binary = dir.join("duplex-kvm.tmp");
        fs::write(&target_exe, b"version-1.0").unwrap();
        fs::write(&new_binary, b"version-2.0").unwrap();

        assert!(replace_executable_at(&new_binary, &target_exe).is_ok());
        assert_eq!(fs::read(&target_exe).unwrap(), b"version-2.0");
        assert!(!new_binary.exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&target_exe).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111); // Executable bit set
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_live_public_release_fetch() {
        let cfg = AppConfig::default();
        let release = fetch_latest_release(&cfg)
            .expect("Must fetch public release from GitHub without token");
        assert_eq!(release.tag_name, "v1.0.1");
        assert!(release.assets.iter().any(|a| a.name == expected_asset_name()));
        assert!(release.assets.iter().any(|a| a.name == "SHA256SUMS.txt"));
    }

    #[test]
    fn test_live_download_and_verify() {
        let cfg = AppConfig::default();
        let release = fetch_latest_release(&cfg).unwrap();
        let expected = expected_asset_name();
        let asset = release.assets.iter().find(|a| a.name == expected).unwrap();
        let tmp_dir = env::temp_dir().join(format!("duplex-live-verify-{}", std::process::id()));
        let _ = fs::create_dir_all(&tmp_dir);
        let dest = tmp_dir.join(expected);
        download_file(
            &asset.browser_download_url,
            None,
            "application/octet-stream",
            &dest,
        )
        .unwrap();
        verify_release_download(&dest, asset, &release.assets, |sums_asset| {
            http_get(
                &sums_asset.browser_download_url,
                None,
                "application/octet-stream",
            )
        })
        .unwrap();
        let _ = fs::remove_dir_all(&tmp_dir);
    }
}
