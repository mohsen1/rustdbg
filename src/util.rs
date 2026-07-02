//! Cross-platform helpers: framed JSON-RPC IO, adapter discovery, URIs, cleanup.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

/// Read one `Content-Length`-framed JSON message (DAP and LSP share this).
pub fn read_framed<R: Read>(r: &mut R) -> Option<Value> {
    let mut header = Vec::new();
    let mut one = [0u8; 1];
    loop {
        r.read_exact(&mut one).ok()?;
        header.push(one[0]);
        if header.ends_with(b"\r\n\r\n") {
            break;
        }
        if header.len() > 1 << 16 {
            return None;
        }
    }
    let text = String::from_utf8_lossy(&header);
    let len: usize = text
        .lines()
        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().to_string()))?
        .parse()
        .ok()?;
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

pub fn write_framed<W: Write>(w: &mut W, payload: &Value) {
    let raw = serde_json::to_vec(payload).unwrap();
    let _ = write!(w, "Content-Length: {}\r\n\r\n", raw.len());
    let _ = w.write_all(&raw);
    let _ = w.flush();
}

fn on_path(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(name);
        if cand.is_file() {
            return Some(cand.to_string_lossy().to_string());
        }
    }
    None
}

/// Prefer `codelldb` (full Rust expression eval), then `lldb-dap`, then the
/// older `lldb-vscode`, including version-suffixed names on Linux
/// (`lldb-dap-18`, `lldb-vscode-14`, ...). On macOS, fall back to `xcrun`.
pub fn find_lldb_dap() -> Option<String> {
    for exact in ["codelldb", "lldb-dap", "lldb-vscode"] {
        if let Some(p) = on_path(exact) {
            return Some(p);
        }
    }
    // version-suffixed variants anywhere on PATH
    if let Some(path) = std::env::var_os("PATH") {
        let mut best: Option<(u32, String)> = None;
        for dir in std::env::split_paths(&path) {
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                for prefix in ["lldb-dap-", "lldb-vscode-"] {
                    if let Some(ver) = name.strip_prefix(prefix) {
                        let n: u32 = ver.split('.').next().unwrap_or("0").parse().unwrap_or(0);
                        if best.as_ref().map(|(b, _)| n > *b).unwrap_or(true) {
                            best = Some((n, e.path().to_string_lossy().to_string()));
                        }
                    }
                }
            }
        }
        if let Some((_, p)) = best {
            return Some(p);
        }
    }
    if let Ok(out) = Command::new("xcrun").args(["-f", "lldb-dap"]).output() {
        let cand = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if out.status.success() && !cand.is_empty() && Path::new(&cand).exists() {
            return Some(cand);
        }
    }
    None
}

pub fn find_rust_analyzer() -> Option<String> {
    if let Ok(out) = Command::new("rustup").args(["which", "rust-analyzer"]).output() {
        let cand = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if out.status.success() && !cand.is_empty() && Path::new(&cand).exists() {
            return Some(cand);
        }
    }
    on_path("rust-analyzer")
}

/// lldb commands that load the Rust value formatters (Vec/String/enum/... render
/// readably). The commands file references the python module but does not import
/// it, so import first, then source.
pub fn rust_formatter_commands() -> Vec<String> {
    let Ok(out) = Command::new("rustc").args(["--print", "sysroot"]).output() else { return vec![] };
    let sysroot = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if sysroot.is_empty() {
        return vec![];
    }
    let etc = PathBuf::from(&sysroot).join("lib/rustlib/etc");
    let lookup = etc.join("lldb_lookup.py");
    let commands = etc.join("lldb_commands");
    if lookup.exists() && commands.exists() {
        vec![
            format!("command script import {}", lookup.display()),
            format!("command source -s 0 {}", commands.display()),
        ]
    } else {
        vec![]
    }
}

pub fn uri(path: &str) -> String {
    let p = Path::new(path);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(p)
    };
    format!("file://{}", abs.to_string_lossy())
}

pub fn rel_from_uri(u: &str, root: &str) -> String {
    let path = u.strip_prefix("file://").unwrap_or(u);
    Path::new(path)
        .strip_prefix(root)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| {
            // fall back to the file name + parent for readability
            Path::new(path).file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_else(|| path.to_string())
        })
}

pub fn abs(path: &str) -> String {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_string_lossy().to_string()
    } else {
        std::env::current_dir().unwrap_or_default().join(p).to_string_lossy().to_string()
    }
}

/// Put the child in its own process group so `kill_group` reaps its whole tree
/// (debugserver, proc-macro-srv, ...).
#[cfg(unix)]
pub fn own_process_group(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
}

#[cfg(not(unix))]
pub fn own_process_group(_cmd: &mut Command) {}

#[cfg(unix)]
pub fn kill_group(pid: u32) {
    extern "C" {
        fn killpg(pgrp: i32, sig: i32) -> i32;
        fn getpgid(pid: i32) -> i32;
    }
    unsafe {
        let pg = getpgid(pid as i32);
        if pg > 0 {
            killpg(pg, 15);
        }
    }
}

#[cfg(not(unix))]
pub fn kill_group(_pid: u32) {}
