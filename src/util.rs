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

/// Map an error message to the outcome taxonomy carried in every response's
/// `status` field. The full set of values is:
/// `ok | user_error | target_error | build_error | debug_adapter_error |
/// timeout | no_session | no_new_information`.
/// (`ok` is produced for successes, not by this function; `no_new_information`
/// is reserved for future use — nothing emits it yet.)
pub fn classify_error(e: &str) -> &'static str {
    if e.contains("no debug session") {
        return "no_session";
    }
    if e.contains("timed out") || e.contains("did not respond") || e.contains("no stop/exit event")
        || e.contains("still warming up") {
        return "timeout";
    }
    if e.contains("lldb") || e.contains("adapter") || e.contains("failed to launch")
        || e.contains("launch did not complete") || e.contains("relaunch failed") {
        return "debug_adapter_error";
    }
    if is_target_error(e) {
        return "target_error";
    }
    if e.contains("could not compile") || e.contains("error[E") || e.contains("could not run cargo") {
        return "build_error";
    }
    "user_error"
}

/// Cargo told us the requested target does not exist (as opposed to failing
/// to compile it): `--bin`/`--test`/`--lib` naming nothing buildable.
fn is_target_error(e: &str) -> bool {
    e.contains("no test target") || e.contains("no bin target") || e.contains("no example target")
        || e.contains("no bench target") || e.contains("no integration test target")
        || e.contains("no library targets") || e.contains("built nothing debuggable")
}

/// Classify a `cargo_build`/`build_target` failure: a missing/unknown target
/// is `target_error`; anything else (a compile failure, cargo itself failing
/// to run) is `build_error`.
pub fn classify_build_error(e: &str) -> &'static str {
    if is_target_error(e) { "target_error" } else { "build_error" }
}

/// Pull the panic report out of captured program output: the `panicked at ...`
/// line plus the message lines after it (assert_eq! reports span several),
/// stopping at the `note: run with RUST_BACKTRACE` hint, a backtrace, or a
/// blank line. `None` until the panic hook has actually printed.
pub fn extract_panic_message(output: &str) -> Option<String> {
    let lines: Vec<&str> = output.lines().collect();
    let start = lines.iter().position(|l| l.contains("panicked at"))?;
    let mut msg = vec![lines[start].trim_end().to_string()];
    for l in lines[start + 1..].iter().take(12) {
        let t = l.trim_end();
        if t.is_empty() || t.starts_with("note: run with") || t.starts_with("stack backtrace:") {
            break;
        }
        msg.push(t.to_string());
    }
    Some(msg.join("\n"))
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
    // an explicit override, then a codelldb installed by install.sh into its own
    // dir (so it finds its bundled liblldb) — both preferred over PATH lldb-dap
    // because codelldb evaluates comparisons (`a == b`), arithmetic, and field
    // access (`p.0`), which lldb-dap's C++ evaluator rejects.
    if let Some(p) = std::env::var_os("RDBG_CODELLDB") {
        if std::path::Path::new(&p).is_file() {
            return Some(p.to_string_lossy().to_string());
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let bundled = std::path::Path::new(&home)
            .join(".local/share/rdbg/codelldb/extension/adapter/codelldb");
        if bundled.is_file() {
            return Some(bundled.to_string_lossy().to_string());
        }
    }
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
    // On Linux, also ask the kernel to SIGKILL the adapter when the daemon dies —
    // even on the daemon's own SIGKILL/OOM/crash, where no cleanup code runs. The
    // adapter is in its own process group (above) so it would otherwise orphan;
    // codelldb can hold ~20 GB of symbols on a large repo, so an orphan is a real
    // memory leak that can OOM the next run. (macOS has no PDEATHSIG; there the
    // daemon reaps on relaunch/`rdbg down`, and callers should reap on hard-kill.)
    #[cfg(target_os = "linux")]
    {
        extern "C" {
            fn prctl(option: i32, a2: u64, a3: u64, a4: u64, a5: u64) -> i32;
        }
        unsafe {
            cmd.pre_exec(|| {
                prctl(1, 9, 0, 0, 0); // PR_SET_PDEATHSIG = 1, SIGKILL = 9
                Ok(())
            });
        }
    }
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

#[cfg(test)]
mod tests {
    use super::{classify_build_error, classify_error, extract_panic_message};

    #[test]
    fn extracts_the_panic_report_from_program_output() {
        // modern (1.72+) two-line format, with the note trimmed off
        let out = "running 1 test\nthread 'tests::boom' panicked at src/lib.rs:6:23:\n\
                   index out of bounds: the len is 3 but the index is 7\n\
                   note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n";
        assert_eq!(extract_panic_message(out).unwrap(),
            "thread 'tests::boom' panicked at src/lib.rs:6:23:\nindex out of bounds: the len is 3 but the index is 7");
        // multi-line assert_eq! report is kept whole
        let out = "thread 'main' panicked at src/main.rs:2:5:\nassertion `left == right` failed\n  left: 1\n right: 2\n\ntail";
        assert_eq!(extract_panic_message(out).unwrap(),
            "thread 'main' panicked at src/main.rs:2:5:\nassertion `left == right` failed\n  left: 1\n right: 2");
        // pre-1.72 single-line format still comes through
        let out = "thread 'main' panicked at 'boom', src/main.rs:2:5\nnote: run with `RUST_BACKTRACE=1`...\n";
        assert_eq!(extract_panic_message(out).unwrap(), "thread 'main' panicked at 'boom', src/main.rs:2:5");
        // no panic yet
        assert_eq!(extract_panic_message("running 1 test\n"), None);
    }

    #[test]
    fn taxonomy_covers_the_known_error_shapes() {
        assert_eq!(classify_error("no debug session (run `rdbg launch` first)"), "no_session");
        assert_eq!(classify_error("timed out waiting for continue"), "timeout");
        assert_eq!(classify_error("no stop/exit event (program may still be running — try `pause`)"), "timeout");
        assert_eq!(classify_error("rust-analyzer is still warming up — retry in a moment"), "timeout");
        assert_eq!(classify_error("no lldb-dap adapter found (install LLVM/lldb, or Xcode command line tools)"), "debug_adapter_error");
        assert_eq!(classify_error("failed to launch /usr/bin/lldb-dap: No such file"), "debug_adapter_error");
        assert_eq!(classify_error("adapter never sent `initialized`"), "debug_adapter_error");
        assert_eq!(classify_error("relaunch failed: spawn error"), "debug_adapter_error");
        assert_eq!(classify_error("unknown command \"bogus\""), "user_error");
        assert_eq!(classify_error("breakpoint not found"), "user_error");
        assert_eq!(classify_error(""), "user_error");
    }

    #[test]
    fn build_failures_split_into_target_and_build_errors() {
        assert_eq!(classify_build_error("error: no test target named `nope`."), "target_error");
        assert_eq!(classify_build_error("no integration test target 'nope' — tests/nope.rs does not exist"), "target_error");
        assert_eq!(classify_build_error("error: no bin target named `nope`"), "target_error");
        assert_eq!(classify_build_error("no library targets found in package `app`"), "target_error");
        assert_eq!(classify_build_error("cargo built nothing debuggable in /x — pick a target"), "target_error");
        assert_eq!(classify_build_error("error[E0308]: mismatched types\nerror: could not compile `app`"), "build_error");
        assert_eq!(classify_build_error("could not run cargo in /x: No such file"), "build_error");
    }
}
