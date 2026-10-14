//! mpv backend. mpv has no HTTP interface like VLC; instead we start it with
//! `--input-ipc-server` and talk JSON over that socket (a Unix domain socket
//! on Linux/macOS, a named pipe on Windows) to read `time-pos` / `duration`.

use crate::vlc::{PlayerSession, Status};
use anyhow::{anyhow, Result};
use rand::Rng;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::Command;

pub fn default_binary_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "mpv.exe"
    } else {
        "mpv"
    }
}

pub fn detect_mpv() -> Option<String> {
    #[cfg(not(target_os = "windows"))]
    {
        if let Ok(output) = Command::new("which").arg("mpv").output() {
            if output.status.success() {
                let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !path.is_empty() && Path::new(&path).is_file() {
                    return Some(path);
                }
            }
        }
        for candidate in [
            "/usr/bin/mpv",
            "/usr/local/bin/mpv",
            "/opt/homebrew/bin/mpv",
            "/snap/bin/mpv",
            "/var/lib/flatpak/exports/bin/io.mpv.Mpv",
            "/Applications/mpv.app/Contents/MacOS/mpv",
        ] {
            if Path::new(candidate).is_file() {
                return Some(candidate.to_string());
            }
        }
        None
    }
    #[cfg(target_os = "windows")]
    {
        if let Ok(output) = Command::new("where").arg("mpv.exe").output() {
            if output.status.success() {
                if let Some(line) = String::from_utf8_lossy(&output.stdout).lines().next() {
                    if Path::new(line.trim()).is_file() {
                        return Some(line.trim().to_string());
                    }
                }
            }
        }
        for candidate in [
            "C:\\Program Files\\mpv\\mpv.exe",
            "C:\\Program Files (x86)\\mpv\\mpv.exe",
            "C:\\mpv\\mpv.exe",
        ] {
            if Path::new(candidate).is_file() {
                return Some(candidate.to_string());
            }
        }
        None
    }
}

fn ipc_path() -> String {
    let id: u32 = rand::thread_rng().gen();
    if cfg!(target_os = "windows") {
        format!("\\\\.\\pipe\\aparatchi-mpv-{id}")
    } else {
        std::env::temp_dir()
            .join(format!("aparatchi-mpv-{id}.sock"))
            .display()
            .to_string()
    }
}

pub fn launch(mpv_path: &str, target: &str, start_seconds: i64, subtitle_lang: &str) -> Result<PlayerSession> {
    let binary = if mpv_path.trim().is_empty() { default_binary_name() } else { mpv_path };
    let ipc = ipc_path();

    let mut cmd = Command::new(binary);
    cmd.arg(format!("--input-ipc-server={ipc}"))
        .arg("--force-window=yes")
        .arg("--no-terminal");
    if start_seconds > 0 {
        cmd.arg(format!("--start={start_seconds}"));
    }
    if !subtitle_lang.trim().is_empty() {
        cmd.arg(format!("--slang={}", subtitle_lang.trim()));
    }
    // "--" so a target starting with '-' can't be read as an option.
    cmd.arg("--").arg(target);

    let child = cmd
        .spawn()
        .map_err(|e| anyhow!("failed to launch '{binary}': {e}. Set the correct mpv path in Settings."))?;

    Ok(PlayerSession { child, port: 0, password: String::new(), ipc: Some(ipc) })
}

#[cfg(not(target_os = "windows"))]
fn connect(path: &str) -> Result<std::os::unix::net::UnixStream> {
    let s = std::os::unix::net::UnixStream::connect(path)?;
    s.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;
    s.set_write_timeout(Some(std::time::Duration::from_secs(2)))?;
    Ok(s)
}

#[cfg(target_os = "windows")]
fn connect(path: &str) -> Result<std::fs::File> {
    Ok(std::fs::OpenOptions::new().read(true).write(true).open(path)?)
}

pub fn query_status(ipc: &str) -> Result<Status> {
    #[cfg(not(target_os = "windows"))]
    {
        let mut stream = connect(ipc)?;
        let mut reader = BufReader::new(stream.try_clone()?);
        let time = request(&mut stream, &mut reader, "time-pos", 1).unwrap_or(0.0);
        let length = request(&mut stream, &mut reader, "duration", 2).unwrap_or(0.0);
        Ok(Status { time_seconds: time as i64, length_seconds: length as i64 })
    }
    #[cfg(target_os = "windows")]
    {
        let mut file = connect(ipc)?;
        let mut reader = BufReader::new(file.try_clone()?);
        let time = request(&mut file, &mut reader, "time-pos", 1).unwrap_or(0.0);
        let length = request(&mut file, &mut reader, "duration", 2).unwrap_or(0.0);
        Ok(Status { time_seconds: time as i64, length_seconds: length as i64 })
    }
}

fn request<W: Write, R: std::io::Read>(w: &mut W, r: &mut BufReader<R>, name: &str, id: u64) -> Result<f64> {
    let msg = serde_json::json!({ "command": ["get_property", name], "request_id": id });
    writeln!(w, "{msg}")?;
    w.flush()?;
    // Skip event lines until the reply to our request shows up.
    for _ in 0..50 {
        let mut line = String::new();
        if r.read_line(&mut line)? == 0 {
            break;
        }
        let v: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("request_id").and_then(|x| x.as_u64()) == Some(id) {
            if v.get("error").and_then(|e| e.as_str()) == Some("success") {
                return v.get("data").and_then(|d| d.as_f64()).ok_or_else(|| anyhow!("no data"));
            }
            return Err(anyhow!("mpv: {}", v.get("error").and_then(|e| e.as_str()).unwrap_or("error")));
        }
    }
    Err(anyhow!("no reply from mpv"))
}

// Removes the leftover socket file once mpv has exited (no-op on Windows).
pub fn cleanup(ipc: &str) {
    if !cfg!(target_os = "windows") {
        let _ = std::fs::remove_file(ipc);
    }
}
