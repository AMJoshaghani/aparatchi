//! Launches VLC as a child process with its built-in HTTP interface enabled,
//! so we can poll playback position (for resume) and know when it exits.
//!
//! We deliberately do NOT embed VLC's video surface inside the Slint window:
//! Slint has no official video-embedding API, and shelling out to the real
//! `vlc` binary is far more robust across Linux/macOS/Windows than hand-rolling
//! libvlc window-handle plumbing.

use anyhow::{anyhow, Result};
use rand::Rng;
use std::path::Path;
use std::process::{Child, Command};
use std::time::Duration;

pub struct VlcSession {
    pub child: Child,
    pub port: u16,
    pub password: String,
}

const VLC_USER: &str = ""; // VLC's http interface uses an empty username

/// The bare binary name to try as an absolute last resort, relying on the
/// OS to resolve it via PATH — same as the user typing it themselves.
pub fn default_binary_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "vlc.exe"
    } else {
        "vlc"
    }
}

fn random_port() -> u16 {
    rand::thread_rng().gen_range(19000..20000)
}

fn random_password() -> String {
    let mut rng = rand::thread_rng();
    (0..12)
        .map(|_| {
            let chars = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
            chars[rng.gen_range(0..chars.len())] as char
        })
        .collect()
}

/// Best-effort search for an installed VLC binary — used to pre-fill the
/// Settings page, and as the fallback when no explicit path has been
/// configured there. Windows and Linux/macOS use different strategies
/// since there's no single convention shared across them.
pub fn detect_vlc() -> Option<String> {
    #[cfg(target_os = "windows")]
    {
        detect_vlc_windows()
    }
    #[cfg(not(target_os = "windows"))]
    {
        detect_vlc_unix()
    }
}

#[cfg(target_os = "windows")]
fn detect_vlc_windows() -> Option<String> {
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    use winreg::RegKey;

    // 1) Ask the registry where VLC's own installer said it put itself.
    for hive in [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER] {
        if let Ok(key) = RegKey::predef(hive).open_subkey("SOFTWARE\\VideoLAN\\VLC") {
            if let Ok(dir) = key.get_value::<String, _>("InstallDir") {
                let candidate = Path::new(&dir).join("vlc.exe");
                if candidate.is_file() {
                    return Some(candidate.display().to_string());
                }
            }
        }
        // 32-bit VLC registered on a 64-bit Windows shows up under Wow6432Node.
        if let Ok(key) = RegKey::predef(hive).open_subkey("SOFTWARE\\WOW6432Node\\VideoLAN\\VLC") {
            if let Ok(dir) = key.get_value::<String, _>("InstallDir") {
                let candidate = Path::new(&dir).join("vlc.exe");
                if candidate.is_file() {
                    return Some(candidate.display().to_string());
                }
            }
        }
    }

    // 2) Fall back to the well-known default install locations.
    for candidate in [
        "C:\\Program Files\\VideoLAN\\VLC\\vlc.exe",
        "C:\\Program Files (x86)\\VideoLAN\\VLC\\vlc.exe",
    ] {
        if Path::new(candidate).is_file() {
            return Some(candidate.to_string());
        }
    }
    None
}

#[cfg(not(target_os = "windows"))]
fn detect_vlc_unix() -> Option<String> {
    // 1) Ask the shell to resolve it via PATH, same as typing `vlc`.
    if let Ok(output) = Command::new("which").arg("vlc").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() && Path::new(&path).is_file() {
                return Some(path);
            }
        }
    }
    // 2) Fall back to well-known install locations (Linux + macOS + Flatpak/Snap).
    for candidate in [
        "/usr/bin/vlc",
        "/usr/local/bin/vlc",
        "/snap/bin/vlc",
        "/var/lib/flatpak/exports/bin/org.videolan.VLC",
        "/Applications/VLC.app/Contents/MacOS/VLC",
    ] {
        if Path::new(candidate).is_file() {
            return Some(candidate.to_string());
        }
    }
    None
}

/// Launch VLC playing `target` (a local file path or a URL), starting at
/// `start_seconds` if > 0. `vlc_path` is the configured/detected binary to
/// run; an empty string falls back to [`default_binary_name`] and lets the
/// OS resolve it via PATH. Returns a handle you can poll / wait on.
pub fn launch(vlc_path: &str, target: &str, start_seconds: i64) -> Result<VlcSession> {
    let port = random_port();
    let password = random_password();
    let binary = if vlc_path.trim().is_empty() { default_binary_name() } else { vlc_path };

    let mut cmd = Command::new(binary);
    cmd.arg(target)
        .arg("--extraintf=http")
        .arg(format!("--http-port={port}"))
        .arg(format!("--http-password={password}"))
        .arg("--no-video-title-show");

    if start_seconds > 0 {
        cmd.arg(format!("--start-time={start_seconds}"));
    }

    let child = cmd
        .spawn()
        .map_err(|e| anyhow!("failed to launch '{binary}': {e}. Set the correct VLC path in Settings."))?;

    Ok(VlcSession { child, port, password })
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Status {
    pub time_seconds: i64,
    pub length_seconds: i64,
}

/// Query VLC's status.json over its HTTP interface. Returns Err while VLC is
/// still starting up (interface not bound yet) or after it has exited.
pub fn query_status(port: u16, password: &str) -> Result<Status> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()?;
    let url = format!("http://127.0.0.1:{port}/requests/status.json");
    let resp = client
        .get(url)
        .basic_auth(VLC_USER, Some(password))
        .send()?
        .error_for_status()?;
    let v: serde_json::Value = resp.json()?;
    Ok(Status {
        time_seconds: v.get("time").and_then(|x| x.as_i64()).unwrap_or(0),
        length_seconds: v.get("length").and_then(|x| x.as_i64()).unwrap_or(0),
    })
}
