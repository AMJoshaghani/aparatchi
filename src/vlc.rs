//! This module launches VLC as a separate process, with its built-in HTTP
//! interface turned on so we can poll it for playback position (that's how
//! resume works) and notice when it exits.
//!
//! We're deliberately not trying to embed VLC's video surface inside the
//! Slint window itself. Slint doesn't have an official way to do that, and
//! shelling out to the real `vlc` binary is just a lot more robust across
//! Linux, macOS, and Windows than hand-rolling our own libvlc window-handle
//! plumbing would be.

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

// The bare binary name we fall back to as a last resort, trusting the OS
// to find it on PATH - basically the same as if the user typed it
// themselves.
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

// Takes a best-effort guess at where VLC is installed. This pre-fills the
// Settings page and also acts as the fallback when nobody's configured a
// path there yet. Windows and Linux/macOS need different approaches here
// since they don't share any one convention for where this stuff lives.
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

    // First, just ask the registry where VLC's own installer said it went.
    for hive in [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER] {
        if let Ok(key) = RegKey::predef(hive).open_subkey("SOFTWARE\\VideoLAN\\VLC") {
            if let Ok(dir) = key.get_value::<String, _>("InstallDir") {
                let candidate = Path::new(&dir).join("vlc.exe");
                if candidate.is_file() {
                    return Some(candidate.display().to_string());
                }
            }
        }
        // A 32-bit VLC on a 64-bit Windows registers itself under Wow6432Node
        // instead, so we check there too.
        if let Ok(key) = RegKey::predef(hive).open_subkey("SOFTWARE\\WOW6432Node\\VideoLAN\\VLC") {
            if let Ok(dir) = key.get_value::<String, _>("InstallDir") {
                let candidate = Path::new(&dir).join("vlc.exe");
                if candidate.is_file() {
                    return Some(candidate.display().to_string());
                }
            }
        }
    }

    // No luck in the registry - fall back to the usual install locations.
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
    // First, just let the shell resolve it via PATH, same as if we'd typed
    // `vlc` ourselves.
    if let Ok(output) = Command::new("which").arg("vlc").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() && Path::new(&path).is_file() {
                return Some(path);
            }
        }
    }
    // Otherwise, check the usual spots - covers Linux, macOS, Flatpak, and Snap.
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

// Plays `target` (a local file or a URL) by launching VLC, starting at
// `start_seconds` if it's greater than 0. `vlc_path` is whatever binary
// we've settled on running - an empty string just falls back to
// `default_binary_name` and lets the OS find it on PATH. `subtitle_lang` is
// an optional language code like "eng"; leave it empty to let VLC decide on
// its own. Returns a handle you can poll or wait on.
pub fn launch(vlc_path: &str, target: &str, start_seconds: i64, subtitle_lang: &str) -> Result<VlcSession> {
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
    if !subtitle_lang.trim().is_empty() {
        cmd.arg(format!("--sub-language={}", subtitle_lang.trim()));
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

// Asks VLC for its status.json over the HTTP interface. This comes back
// as an Err both while VLC is still starting up (the interface isn't bound
// yet) and after it's already exited - both are fine, just mean there's
// nothing to report right now.
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
