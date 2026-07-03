//! The `rdbg` command-line client: dispatches subcommands to the daemon.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::util::{classify_build_error, classify_error};

pub const USAGE: &str = "\
rdbg — IDE-grade Rust debugging for agents (rust-analyzer + lldb-dap)

LAUNCH
  rdbg launch --cargo <dir> [--bin <t>|--test <t>|--lib] --break f.rs:L [...] [-- ARGS]
    --bin <t>   a binary target        --test <t>  an integration test (tests/<t>.rs)
    --lib       the library's own unit tests (#[cfg(test)] mod tests) — pass the
                test name after --,  e.g.  --lib --break src/lib.rs:42 -- my_test
  rdbg launch --bin-path <path> --break f.rs:L [...]      (skip the build)
    at launch you may also pass  --break-fn <name>  --panic

TRACE (one call instead of break→inspect→continue×N)
  rdbg trace --cargo . --bin app --break f.rs:L --capture i,sum --max 30 -- ARGS
  rdbg trace --cargo . --lib --break src/lib.rs:42 --capture a,b -- my_test
    runs through every hit, captures the paths at each, returns a table

BREAKPOINTS (set or change any time, even while paused)
  rdbg break f.rs:L [--if <expr>] [--hit <N>] [--log <msg>]
  rdbg break --fn <name>        break entering a function
  rdbg break --panic            break where a Rust panic is raised
  rdbg watch <var>              break when a local changes (data breakpoint)
  rdbg breaks                   list breakpoints with ids
  rdbg break-rm <id|panic> | break-off <id> | break-on <id>

RUN CONTROL
  rdbg run | continue           resume to the next stop
  rdbg continue --until '<path> <op> <value>'   keep resuming past breakpoint
      stops until the condition holds (op: == != < <= > >=), checked by rdbg
      itself at each stop — needs at least one active breakpoint
  rdbg step [over|in|out|insn]  step a source line, or one instruction
  rdbg until f.rs:L             run to a line
  rdbg pause                    interrupt a running program
  rdbg restart                  relaunch with the same breakpoints

THREADS / FRAMES
  rdbg threads | thread <id>
  rdbg frame <n> | up | down

INSPECT / MUTATE
  rdbg vars [--depth N] [--full]   locals with real Rust values (--full: deep dump)
  rdbg eval <path> [<path>...]  evaluate one or more variable paths
  rdbg set <path> = <value> [--then continue|step]   change a value (test a fix)
  rdbg watch-expr add|rm <path>
  rdbg list [--radius N] | bt | state

BATCH (one call instead of several)
  rdbg do 'break f.rs:L; continue; vars; eval sum; bt'
    run several subcommands in order, labeled; stops on the first error/exit

NAVIGATE (rust-analyzer)
  rdbg where <Name>
  rdbg def|hover|refs <file> <line> <col>

  rdbg status | stop | down | mcp

OUTPUT
  --json (anywhere in the args)  print the result as one compact JSON line with
    a `status` outcome field: ok | user_error | target_error | build_error |
    debug_adapter_error | timeout | no_session | no_new_information.
    Works with every command — see docs/json-schema.md";

const STATE: &str = ".rdbg";

pub fn ws_root() -> PathBuf {
    let out = Command::new("git").args(["rev-parse", "--show-toplevel"]).output();
    match out {
        Ok(o) if o.status.success() => {
            let p = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !p.is_empty() {
                return PathBuf::from(p);
            }
        }
        _ => {}
    }
    std::env::current_dir().unwrap_or_default()
}

pub fn request(ws: &Path, payload: &Value, timeout: Duration) -> Option<Value> {
    let addr = ws.join(STATE).join("daemon.json");
    let meta: Value = serde_json::from_str(&std::fs::read_to_string(addr).ok()?).ok()?;
    let sock = meta["socket"].as_str()?;
    let mut stream = UnixStream::connect(sock).ok()?;
    stream.set_read_timeout(Some(timeout)).ok()?;
    stream.write_all(serde_json::to_string(payload).ok()?.as_bytes()).ok()?;
    stream.write_all(b"\n").ok()?;
    stream.flush().ok()?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    serde_json::from_str(line.trim()).ok()
}

pub fn ensure_daemon(ws: &Path) {
    if request(ws, &json!({"cmd": "ping"}), Duration::from_secs(3)).is_some() {
        return;
    }
    let exe = std::env::current_exe().expect("current exe");
    let _ = Command::new(exe)
        .arg("__daemon")
        .arg(ws)
        .current_dir(ws)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let deadline = Instant::now() + Duration::from_secs(12);
    while Instant::now() < deadline {
        if request(ws, &json!({"cmd": "ping"}), Duration::from_secs(1)).is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Build one cargo target and return its executable. `bin` → a `--bin` target,
/// `test` → a `tests/<name>.rs` integration test, `lib` → the library's own
/// unit-test binary (inline `#[cfg(test)] mod tests`). Returns an error string
/// instead of exiting — the MCP server calls this too.
pub fn cargo_build(dir: &Path, bin: Option<&str>, test: Option<&str>, lib: bool) -> Result<PathBuf, String> {
    let mut cmd = Command::new("cargo");
    cmd.current_dir(dir);
    if lib || test.is_some() {
        cmd.args(["test", "--no-run"]);
    } else {
        cmd.arg("build");
    }
    cmd.arg("--message-format=json");
    if lib {
        cmd.arg("--lib");
    } else if let Some(t) = test {
        cmd.args(["--test", t]);
    }
    if let Some(b) = bin {
        cmd.args(["--bin", b]);
    }
    eprintln!("building …");
    let out = cmd.output().map_err(|e| format!("could not run cargo in {}: {e}", dir.display()))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() {
        // surface the rendered compiler diagnostics (JSON message format sends
        // them to stdout), then cargo's own stderr (target-selection errors etc.)
        let mut msg = String::new();
        for line in stdout.lines() {
            let Ok(m): Result<Value, _> = serde_json::from_str(line) else { continue };
            if m["reason"] == "compiler-message" {
                if let Some(r) = m["message"]["rendered"].as_str() {
                    msg.push_str(r);
                }
            }
        }
        msg.push_str(String::from_utf8_lossy(&out.stderr).trim_end());
        return Err(msg);
    }
    pick_artifact(&stdout, bin, test, lib).ok_or_else(|| format!(
        "cargo built nothing debuggable in {} — pick a target: --bin <name>, --test <name> (tests/<name>.rs), or --lib (the library's #[cfg(test)] unit tests)",
        dir.display()))
}

/// Pick the executable out of cargo's `--message-format=json` output: the named
/// `--bin`/`--test` target, the library's unit-test binary in lib mode, or the
/// last executable produced. Build scripts are never candidates.
fn pick_artifact(json_lines: &str, bin: Option<&str>, test: Option<&str>, lib: bool) -> Option<PathBuf> {
    let mut exe: Option<PathBuf> = None;
    for line in json_lines.lines() {
        let Ok(m): Result<Value, _> = serde_json::from_str(line) else { continue };
        if m["reason"] != "compiler-artifact" {
            continue;
        }
        let Some(e) = m["executable"].as_str() else { continue };
        let kinds = m["target"]["kind"].as_array().cloned().unwrap_or_default();
        if kinds.iter().any(|k| k == "custom-build") {
            continue;
        }
        let name = m["target"]["name"].as_str().unwrap_or("");
        let is_lib_test = m["profile"]["test"].as_bool().unwrap_or(false)
            && kinds.iter().any(|k| k.as_str().is_some_and(|s| s.ends_with("lib")));
        let matched = if lib { is_lib_test } else { bin == Some(name) || test == Some(name) };
        if matched {
            exe = Some(PathBuf::from(e));
        } else if exe.is_none() {
            exe = Some(PathBuf::from(e));
        }
    }
    exe
}

/// Resolve `--bin`/`--test`/`--lib` to a built executable, forgivingly: when
/// `--test <X>` names no integration-test target (no `tests/<X>.rs`) but the
/// library's unit-test binary has a `#[test]` matching `X` — the common inline
/// `#[cfg(test)] mod tests` case — fall back to that binary with `X` as the
/// test filter. `args` are the program's args (the test-name filter); the
/// fallback inserts `X` if it is missing. Errors say the correct invocation.
pub fn build_target(dir: &Path, bin: Option<&str>, test: Option<&str>, lib: bool,
                    args: &mut Vec<String>) -> Result<PathBuf, String> {
    let lib = lib || test == Some("lib"); // `--test lib` has always meant the lib unit tests
    let test = if lib { None } else { test };
    let Some(t) = test else { return cargo_build(dir, bin, None, lib) };
    match cargo_build(dir, bin, Some(t), false) {
        Err(e) if e.contains("no test target named") => {
            let fallback = cargo_build(dir, None, None, true);
            if let Ok(exe) = &fallback {
                if lists_test(exe, t) {
                    eprintln!("note: '{t}' is not an integration test target (no tests/{t}.rs); \
                               treating it as a --lib unit-test filter");
                    if !args.iter().any(|a| a == t) {
                        args.insert(0, t.to_string());
                    }
                    return Ok(exe.clone());
                }
            }
            let detail = match &fallback {
                Ok(_) => format!("and the library's unit tests have no #[test] matching '{t}'"),
                Err(_) => "and the package has no library unit-test binary".to_string(),
            };
            Err(format!(
                "no integration test target '{t}' — tests/{t}.rs does not exist, {detail}.\n\
                 --test <name> targets integration tests (tests/<name>.rs). For a #[test] function\n\
                 inside the library, use --lib with the test name after --:\n  \
                 rdbg launch --cargo <dir> --lib --break <file>:<line> -- {t}\n\
                 (MCP debug_launch/debug_trace: pass lib:true and the test name in args)"))
        }
        other => other,
    }
}

/// Does this libtest binary have a test matching `filter`? (`<bin> <filter> --list`
/// prints `path::to::test: test` lines without running anything.)
fn lists_test(exe: &Path, filter: &str) -> bool {
    Command::new(exe).args([filter, "--list"]).output()
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().any(|l| l.trim_end().ends_with(": test")))
        .unwrap_or(false)
}

/// A client-side failure in the same shape the daemon uses, so `--json` output
/// is uniform no matter where the failure happened.
fn err_value(status: &str, msg: &str) -> Value {
    json!({"ok": false, "status": status, "error": msg})
}

/// A definite JSON response: no reply within the timeout becomes a `timeout`
/// error object, and a reply from an older daemon without `status` gets one
/// derived from `ok`/`error`.
fn jresp(resp: Option<Value>) -> Value {
    match resp {
        Some(mut v) => {
            if v.is_object() && v.get("status").is_none() {
                let s = if v["ok"].as_bool().unwrap_or(false) {
                    "ok"
                } else {
                    classify_error(v["error"].as_str().unwrap_or(""))
                };
                v["status"] = json!(s);
            }
            v
        }
        None => err_value("timeout", "the rdbg daemon did not respond"),
    }
}

/// Parse a `file.rs:line` breakpoint spec; returns an error instead of exiting
/// the process — `run_command_full` is driven by the MCP server and `do` too
/// (never exit there).
fn parse_bp_soft(spec: &str, base: &Path) -> Result<(String, i64), String> {
    let (f, l) = spec.rsplit_once(':').ok_or_else(|| format!("bad breakpoint {spec:?} (want file.rs:line)"))?;
    let line: i64 = l.parse().map_err(|_| format!("bad line in {spec:?}"))?;
    let p = Path::new(f);
    let abs = if p.is_absolute() { p.to_path_buf() } else { base.join(f) };
    let abs = abs.canonicalize().unwrap_or(abs);
    Ok((abs.to_string_lossy().to_string(), line))
}

fn opt<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).map(|s| s.as_str())
}

/// Value that may span several tokens (an unquoted expression/message): all
/// tokens after `flag` up to the next `--option` or end.
fn opt_multi(args: &[String], flag: &str) -> Option<String> {
    let i = args.iter().position(|a| a == flag)? + 1;
    let mut toks = vec![];
    for a in &args[i..] {
        if a.starts_with("--") {
            break;
        }
        toks.push(a.clone());
    }
    if toks.is_empty() { None } else { Some(toks.join(" ")) }
}

/// The `continue --until` outcome line, when the stop carries one: says whether
/// the condition held (and the observed value), or why the loop gave up.
fn until_note(stop: &Value) -> Option<String> {
    let u = stop.get("until")?;
    let cond = u["cond"].as_str().unwrap_or("?");
    let stops = u["stops"].as_i64().unwrap_or(0);
    Some(match u["outcome"].as_str()? {
        "held" => format!(">>> UNTIL: condition `{cond}` held after {stops} stop(s) — observed {}",
                          u["observed"].as_str().unwrap_or("?")),
        "exited" => format!(">>> UNTIL: program exited after {stops} stop(s) without `{cond}` holding"),
        "cap" => format!(">>> UNTIL: gave up after {stops} stops (safety cap) without `{cond}` holding — still paused at the last stop"),
        _ => return None,
    })
}

/// Render a stop summary as text (returned, not printed — callers `println!` it,
/// or thread it through `do` / MCP).
fn fmt_stop(stop: &Value) -> String {
    if stop.is_null() {
        return "(no stop — not paused)".to_string();
    }
    if stop["exited"].as_bool().unwrap_or(false) {
        let mut out = format!(">>> program exited (code {})",
            stop["exit_code"].as_i64().map(|c| c.to_string()).unwrap_or("?".into()));
        // flag breakpoints that never fired — the #1 confusion when a run exits unexpectedly
        if let Some(bps) = stop["breakpoints"].as_array() {
            let notes: Vec<String> = bps.iter().filter_map(|b| {
                let loc = b["loc"].as_str().unwrap_or("?");
                let verified = b["verified"].as_bool().unwrap_or(true);
                let hits = b["hits"].as_u64().unwrap_or(0);
                if !verified {
                    Some(format!("  {loc}  — NOT BOUND (name/path unresolved; check spelling, or the symbol may not exist / be inlined)"))
                } else if hits == 0 {
                    Some(format!("  {loc}  — bound, 0 hits (never reached on this input)"))
                } else {
                    None
                }
            }).collect();
            if !notes.is_empty() {
                out.push_str("\nbreakpoints that did not fire:\n");
                out.push_str(&notes.join("\n"));
                out.push_str("\n(bound-but-0-hits = the code didn't run that line/fn on this input; the value may be produced by a different path — try another break location.)");
            }
        }
        if let Some(n) = until_note(stop) {
            out = format!("{n}\n{out}");
        }
        if let Some(o) = stop["output"].as_str() {
            if !o.is_empty() {
                out.push_str(&format!("\n--- program output ---\n{}", o.trim_end()));
            }
        }
        return out;
    }
    let mut lines = vec![];
    if let Some(n) = until_note(stop) {
        lines.push(n);
    }
    lines.push(format!(">>> STOP [{}] {}  (thread {})",
        stop["reason"].as_str().unwrap_or("?"), stop["frame"].as_str().unwrap_or("?"),
        stop["thread"].as_i64().unwrap_or(0)));
    if let Some(src) = stop["source"].as_str() {
        if !src.is_empty() {
            lines.push(src.to_string());
        }
    }
    if let Some(d) = stop["delta"].as_str() {
        if !d.is_empty() {
            lines.push(format!("changed:\n{d}"));
        }
    }
    if let Some(w) = stop["watches"].as_str() {
        if !w.is_empty() {
            lines.push(format!("watches:\n{w}"));
        }
    }
    lines.join("\n")
}

fn fmt_result_stop(r: &Option<Value>) -> String {
    match r {
        Some(v) if v["ok"].as_bool().unwrap_or(false) => fmt_stop(&v["stop"]),
        Some(v) => format!("error: {}", v["error"].as_str().unwrap_or("unknown")),
        None => "error: daemon did not respond".to_string(),
    }
}

pub fn main(args: &[String]) -> i32 {
    // global --json (anywhere in the args): print each result as one compact
    // JSON line with a `status` outcome field instead of the human rendering
    let json_mode = args.iter().any(|a| a == "--json");
    let args: Vec<String> = args.iter().filter(|a| *a != "--json").cloned().collect();
    if args.is_empty() {
        if json_mode {
            println!("{}", json!({"ok": true, "status": "ok", "usage": USAGE}));
        } else {
            println!("{USAGE}");
        }
        return 0;
    }
    let cmd = args[0].as_str();
    let rest = &args[1..];
    match cmd {
        "--help" | "-h" | "help" => {
            if json_mode {
                println!("{}", json!({"ok": true, "status": "ok", "usage": USAGE}));
            } else {
                println!("{USAGE}");
            }
            return 0;
        }
        "--version" | "-V" => {
            if json_mode {
                println!("{}", json!({"ok": true, "status": "ok", "version": env!("CARGO_PKG_VERSION")}));
            } else {
                println!("rdbg {}", env!("CARGO_PKG_VERSION"));
            }
            return 0;
        }
        _ => {}
    }
    let ws = ws_root();

    if cmd == "down" {
        let resp = request(&ws, &json!({"cmd": "shutdown"}), Duration::from_secs(5));
        if json_mode {
            // no daemon running is still a stopped daemon
            println!("{}", resp.map(|v| jresp(Some(v))).unwrap_or_else(|| json!({"ok": true, "status": "ok"})));
        } else {
            println!("rdbg: daemon stopped");
        }
        return 0;
    }
    ensure_daemon(&ws);

    match cmd {
        "launch" => do_launch(&ws, rest, false, json_mode),
        "trace" => do_launch(&ws, rest, true, json_mode),
        "do" => {
            // run several subcommands in one call: `rdbg do 'break f:L; continue; vars'`
            let (text, had_error, value) = run_batch_full(&ws, &rest.join(" "));
            if json_mode {
                println!("{value}");
            } else {
                print!("{text}");
            }
            if had_error { 1 } else { 0 }
        }
        _ => {
            let (text, value) = run_command_full(&ws, cmd, rest);
            if json_mode {
                println!("{value}");
                return match value["status"].as_str() {
                    Some("ok") => 0,
                    Some("user_error") => 2,
                    _ => 1,
                };
            }
            println!("{text}");
            if text == USAGE { 2 } else { 0 }
        }
    }
}

/// Run one subcommand against the daemon and return its rendered text plus the
/// raw JSON response (with `status`). `main` prints one of the two (`--json`
/// picks the JSON line); `do` and the MCP `debug_do` tool collect the text.
/// Never exits the process — safe to call from the MCP server.
fn run_command_full(ws: &Path, cmd: &str, rest: &[String]) -> (String, Value) {
    let r = |p: Value| request(ws, &p, Duration::from_secs(300));
    let cwd = std::env::current_dir().unwrap_or_default();
    // a client-side argument error, in both renderings
    let usage_err = |msg: &str| (format!("error: {msg}"), err_value("user_error", msg));
    match cmd {
        "status" => {
            let resp = r(json!({"cmd": "status"}));
            (serde_json::to_string_pretty(resp.as_ref().unwrap_or(&Value::Null)).unwrap(), jresp(resp))
        }
        "break" => {
            if rest.iter().any(|a| a == "--fn") {
                let resp = r(json!({"cmd": "bp_fn", "name": opt(rest, "--fn").unwrap_or("")}));
                let text = resp.as_ref().map(|v| format!("fn breakpoint [{}]", v["id"])).unwrap_or_else(|| "error".into());
                (text, jresp(resp))
            } else if rest.iter().any(|a| a == "--panic") {
                let resp = r(json!({"cmd": "bp_panic"}));
                ("panic breakpoint [panic] set (breaks where a Rust panic is raised)".to_string(), jresp(resp))
            } else {
                let Some(spec) = rest.first() else { return usage_err("break needs file.rs:line") };
                let (f, l) = match parse_bp_soft(spec, &cwd) { Ok(v) => v, Err(e) => return usage_err(&e) };
                let hit = opt(rest, "--hit").and_then(|h| h.parse::<i64>().ok());
                let resp = r(json!({"cmd": "bp_add", "file": f, "line": l,
                    "condition": opt_multi(rest, "--if"), "hit": hit, "log": opt_multi(rest, "--log")}));
                let text = match &resp {
                    Some(v) if v["ok"].as_bool().unwrap_or(false) => {
                        let warn = if v["verified"].as_bool().unwrap_or(true) { "" } else { "  (UNVERIFIED — no code at that line?)" };
                        format!("breakpoint [{}] {}{}", v["id"], spec, warn)
                    }
                    _ => "error setting breakpoint".to_string(),
                };
                (text, jresp(resp))
            }
        }
        "watch" => {
            let Some(var) = rest.first().cloned() else { return usage_err("watch needs a variable") };
            let resp = r(json!({"cmd": "bp_watch", "var": var}));
            let text = match &resp {
                Some(v) if v["ok"].as_bool().unwrap_or(false) => format!("watchpoint [{}] on {} (breaks when it changes)", v["id"], var),
                Some(v) => format!("error: {}", v["error"].as_str().unwrap_or("unknown")),
                None => "error: daemon did not respond".to_string(),
            };
            (text, jresp(resp))
        }
        "breaks" => {
            let resp = r(json!({"cmd": "bp_list"}));
            (fmt_field(&resp, "breakpoints"), jresp(resp))
        }
        "break-rm" => {
            let resp = r(json!({"cmd": "bp_rm", "id": rest.first().cloned().unwrap_or_default()}));
            ("ok".to_string(), jresp(resp))
        }
        "break-on" | "break-off" => {
            let resp = r(json!({"cmd": "bp_enable", "id": rest.first().cloned().unwrap_or_default(), "enabled": cmd == "break-on"}));
            ("ok".to_string(), jresp(resp))
        }
        "run" | "continue" => {
            // `--until '<path> <op> <value>'`: the daemon keeps resuming and
            // re-checks the condition itself at each breakpoint stop
            let resp = match opt_multi(rest, "--until") {
                Some(cond) => r(json!({"cmd": "continue_until", "until": cond})),
                None => r(json!({"cmd": "continue"})),
            };
            (fmt_result_stop(&resp), jresp(resp))
        }
        "step" => {
            let resp = r(json!({"cmd": "step", "kind": rest.first().map(|s| s.as_str()).unwrap_or("over")}));
            (fmt_result_stop(&resp), jresp(resp))
        }
        "until" => {
            let Some(spec) = rest.first() else { return usage_err("until needs file.rs:line") };
            let (f, l) = match parse_bp_soft(spec, &cwd) { Ok(v) => v, Err(e) => return usage_err(&e) };
            let resp = r(json!({"cmd": "until", "file": f, "line": l}));
            (fmt_result_stop(&resp), jresp(resp))
        }
        "pause" => {
            let resp = r(json!({"cmd": "pause"}));
            (fmt_result_stop(&resp), jresp(resp))
        }
        "restart" => {
            let resp = r(json!({"cmd": "restart"}));
            (fmt_result_stop(&resp), jresp(resp))
        }
        "threads" => {
            let resp = r(json!({"cmd": "threads"}));
            (fmt_field(&resp, "threads"), jresp(resp))
        }
        "thread" => {
            let resp = r(json!({"cmd": "thread", "id": rest.first().and_then(|s| s.parse::<i64>().ok()).unwrap_or(0)}));
            (fmt_result_stop(&resp), jresp(resp))
        }
        "frame" | "up" | "down" => {
            let payload = if cmd == "up" || cmd == "down" {
                json!({"cmd": "frame", "dir": cmd})
            } else {
                json!({"cmd": "frame", "index": rest.first().and_then(|s| s.parse::<i64>().ok()).unwrap_or(0)})
            };
            let resp = r(payload);
            let text = match &resp {
                Some(v) if v["ok"].as_bool().unwrap_or(false) => {
                    let mut lines = vec![];
                    if let Some(src) = v["source"].as_str() { lines.push(src.to_string()); }
                    lines.push("locals:".to_string());
                    if let Some(vars) = v["vars"].as_str() { lines.push(vars.to_string()); }
                    lines.join("\n")
                }
                _ => "no such frame".to_string(),
            };
            (text, jresp(resp))
        }
        "vars" => {
            let full = rest.iter().any(|a| a == "--full");
            let mut payload = json!({"cmd": "vars", "full": full});
            if let Some(d) = opt(rest, "--depth").and_then(|d| d.parse::<i64>().ok()) {
                payload["depth"] = json!(d);
            }
            let resp = r(payload);
            (fmt_field(&resp, "vars"), jresp(resp))
        }
        "eval" => {
            // evaluate one or more variable paths in a single agent call
            let paths: Vec<&String> = rest.iter().filter(|p| !p.starts_with("--")).collect();
            if paths.is_empty() {
                return usage_err("eval needs at least one variable path");
            }
            let mut out = vec![];
            let mut results = vec![];
            let (mut all_ok, mut status) = (true, "ok".to_string());
            for path in paths {
                let v = r(json!({"cmd": "eval", "expr": path}));
                let val = v.as_ref().filter(|v| v["ok"].as_bool().unwrap_or(false))
                    .map(|v| v["value"].as_str().unwrap_or("").to_string())
                    .unwrap_or_else(|| "error".into());
                out.push(format!("{path} = {val}"));
                let mut jv = jresp(v);
                if all_ok && !jv["ok"].as_bool().unwrap_or(false) {
                    all_ok = false;
                    status = jv["status"].as_str().unwrap_or("user_error").to_string();
                }
                jv["expr"] = json!(path);
                results.push(jv);
            }
            (out.join("\n"), json!({"ok": all_ok, "status": status, "results": results}))
        }
        "set" => {
            // rdbg set <path> = <value> [--then continue|step]   (test a fix live)
            let then = rest.iter().position(|a| a == "--then");
            let body: Vec<String> = rest.iter().take(then.unwrap_or(rest.len())).cloned().collect();
            if body.is_empty() { return usage_err("set needs <path> = <value>") }
            let joined = body.join(" ");
            let (path, value) = match joined.split_once('=') {
                Some((p, v)) => (p.trim().to_string(), v.trim().to_string()),
                None => (body[0].clone(), body[1..].join(" ")),
            };
            let resp = r(json!({"cmd": "set", "path": path, "value": value}));
            let mut out = vec![fmt_field(&resp, "value")];
            let mut jv = jresp(resp);
            if let Some(i) = then {
                let after = rest.get(i + 1).map(|s| s.as_str()).unwrap_or("continue");
                let resume = if after == "step" { r(json!({"cmd": "step", "kind": "over"})) } else { r(json!({"cmd": "continue"})) };
                out.push(fmt_result_stop(&resume));
                let tv = jresp(resume);
                if jv["ok"].as_bool().unwrap_or(false) && !tv["ok"].as_bool().unwrap_or(false) {
                    jv["ok"] = json!(false);
                    jv["status"] = tv["status"].clone();
                }
                jv["then"] = tv;
            }
            (out.join("\n"), jv)
        }
        "watch-expr" => {
            let action = rest.first().map(|s| s.as_str()).filter(|s| *s == "add" || *s == "rm").unwrap_or("list");
            let expr = if action != "list" { Some(rest[1..].join(" ")) } else { None };
            let resp = r(json!({"cmd": "watch_expr", "action": action, "expr": expr}));
            (fmt_field(&resp, "watches"), jresp(resp))
        }
        "bt" => {
            let resp = r(json!({"cmd": "bt"}));
            (fmt_field(&resp, "bt"), jresp(resp))
        }
        "list" => {
            let radius = opt(rest, "--radius").and_then(|d| d.parse::<i64>().ok()).unwrap_or(6);
            let resp = r(json!({"cmd": "list", "radius": radius}));
            (fmt_field(&resp, "source"), jresp(resp))
        }
        "state" => {
            let resp = r(json!({"cmd": "state"}));
            let text = match &resp {
                Some(v) if v["ok"].as_bool().unwrap_or(false) => {
                    let mut lines = vec![fmt_stop(&v["stop"]), "locals:".to_string()];
                    if let Some(vars) = v["vars"].as_str() { lines.push(vars.to_string()); }
                    if let Some(w) = v["watches"].as_str() { if !w.is_empty() { lines.push(format!("watches:\n{w}")); } }
                    lines.join("\n")
                }
                _ => "error".to_string(),
            };
            (text, jresp(resp))
        }
        "stop" => {
            let resp = r(json!({"cmd": "stop"}));
            ("debug session ended".to_string(), jresp(resp))
        }
        "where" => {
            let resp = r(json!({"cmd": "where", "query": rest.first().cloned().unwrap_or_default()}));
            let text = match &resp {
                Some(v) if v["ok"].as_bool().unwrap_or(false) => {
                    v["symbols"].as_array().cloned().unwrap_or_default().iter().map(|s| {
                        let c = s["container"].as_str().map(|c| format!(" ({c})")).unwrap_or_default();
                        format!("  {}{}  {}:{}", s["name"].as_str().unwrap_or("?"), c, s["file"].as_str().unwrap_or("?"), s["line"])
                    }).collect::<Vec<_>>().join("\n")
                }
                Some(v) => format!("error: {}", v["error"].as_str().unwrap_or("unknown")),
                None => "error".to_string(),
            };
            (text, jresp(resp))
        }
        "def" | "refs" | "hover" => {
            let Some(f) = rest.first().cloned() else { return usage_err("needs <file> <line> <col>") };
            let (l, c) = (rest.get(1).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0),
                          rest.get(2).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0));
            let resp = r(json!({"cmd": cmd, "file": f, "line": l, "col": c}));
            let text = match &resp {
                Some(v) if v["ok"].as_bool().unwrap_or(false) => {
                    if cmd == "hover" {
                        v["hover"].as_str().filter(|s| !s.is_empty()).unwrap_or("(no hover)").to_string()
                    } else {
                        let locs = v["locations"].as_array().cloned().unwrap_or_default();
                        if locs.is_empty() {
                            "(none)".to_string()
                        } else {
                            locs.iter().map(|loc| format!("  {}:{}:{}", loc["file"].as_str().unwrap_or("?"), loc["line"], loc["col"])).collect::<Vec<_>>().join("\n")
                        }
                    }
                }
                Some(v) => format!("error: {}", v["error"].as_str().unwrap_or("unknown")),
                None => "error".to_string(),
            };
            (text, jresp(resp))
        }
        _ => (USAGE.to_string(), err_value("user_error", &format!("unknown subcommand {cmd:?}"))),
    }
}

/// Run a `;`-separated batch of subcommands, labeling each with `$ <subcommand>`.
/// Stops at the first error or program exit. Returns the combined text, whether
/// any subcommand errored, and a JSON value `{ok, status, steps}` where `steps`
/// is `[{cmd, response}, ...]` (the raw response of each subcommand run) and
/// `status` is the first non-`ok` step status, or `ok`. Shared by the CLI `do`
/// and the MCP `debug_do`.
pub(crate) fn run_batch_full(ws: &Path, script: &str) -> (String, bool, Value) {
    let mut out = String::new();
    let mut had_error = false;
    let mut steps = vec![];
    for part in script.split(';') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let toks: Vec<String> = part.split_whitespace().map(String::from).collect();
        let (text, jv) = run_command_full(ws, toks[0].as_str(), &toks[1..]);
        // an unknown subcommand falls through to the full USAGE — flag it, don't dump it
        if text == USAGE {
            out.push_str(&format!("$ {part}\nerror: unknown subcommand {:?} (not usable inside `do`)\n\n", toks[0]));
            steps.push(json!({"cmd": part, "response": jv}));
            had_error = true;
            break;
        }
        out.push_str(&format!("$ {part}\n{text}\n\n"));
        let errored = text.lines().any(|l| l.trim_start().starts_with("error:"));
        let exited = text.contains(">>> program exited");
        steps.push(json!({"cmd": part, "response": jv}));
        had_error |= errored;
        if errored || exited {
            break;
        }
    }
    let status = steps.iter()
        .filter_map(|s| s["response"]["status"].as_str())
        .find(|s| *s != "ok")
        .unwrap_or(if had_error { "user_error" } else { "ok" })
        .to_string();
    let ok = !had_error && status == "ok";
    let value = json!({"ok": ok, "status": status, "steps": steps});
    (out, had_error, value)
}

fn fmt_field(resp: &Option<Value>, field: &str) -> String {
    match resp {
        Some(v) if v["ok"].as_bool().unwrap_or(false) => v[field].as_str().unwrap_or("").to_string(),
        Some(v) => format!("error: {}", v["error"].as_str().unwrap_or("unknown")),
        None => "error: daemon did not respond".to_string(),
    }
}

fn do_launch(ws: &Path, rest: &[String], trace_mode: bool, json_mode: bool) -> i32 {
    let verb = if trace_mode { "trace" } else { "launch" };
    // emit a failure in the active mode: one JSON line on stdout, or text on stderr
    let fail = |status: &str, msg: &str, code: i32| -> i32 {
        if json_mode {
            println!("{}", err_value(status, msg));
        } else {
            eprintln!("error: {msg}");
        }
        code
    };
    let usage = format!(
        "usage: rdbg {verb} --cargo <dir> [--bin <name> | --test <name> | --lib] --break <file.rs:line> \
         [--break-fn <fn>] [--panic]{} [-- <program args / test-name filter>]\n       \
         rdbg {verb} --bin-path <path> --break <file.rs:line>\n  \
         --bin <name>   a binary target        --test <name>  an integration test (tests/<name>.rs)\n  \
         --lib          the library's own unit tests (#[cfg(test)] mod tests); pass the test name after --",
        if trace_mode { " [--capture a,b] [--max N]" } else { "" });
    let (mut cargo, mut bin_path, mut bin, mut test): (Option<String>, Option<String>, Option<String>, Option<String>) = (None, None, None, None);
    let mut lib = false;
    let mut breaks: Vec<String> = vec![];
    let mut fn_breaks: Vec<String> = vec![];
    let mut args: Vec<String> = vec![];
    let mut captures: Vec<String> = vec![];
    let mut max: i64 = 50;
    let mut panic = false;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--cargo" => { cargo = rest.get(i + 1).cloned(); i += 2; }
            "--bin-path" => { bin_path = rest.get(i + 1).cloned(); i += 2; }
            "--bin" => { bin = rest.get(i + 1).cloned(); i += 2; }
            "--test" => { test = rest.get(i + 1).cloned(); i += 2; }
            "--lib" => { lib = true; i += 1; }
            "--break" => { if let Some(b) = rest.get(i + 1) { breaks.push(b.clone()); } i += 2; }
            "--break-fn" => { if let Some(b) = rest.get(i + 1) { fn_breaks.push(b.clone()); } i += 2; }
            "--capture" => { if let Some(c) = rest.get(i + 1) { captures.extend(c.split(',').map(|s| s.trim().to_string())); } i += 2; }
            "--max" => { max = rest.get(i + 1).and_then(|n| n.parse().ok()).unwrap_or(50); i += 2; }
            "--panic" => { panic = true; i += 1; }
            "--" => { args = rest[i + 1..].to_vec(); break; }
            other => return fail("user_error", &format!("unknown {verb} arg {other:?}\n{usage}"), 2),
        }
    }
    if [bin.is_some(), test.is_some(), lib].iter().filter(|b| **b).count() > 1 {
        return fail("user_error", "pick one target — --bin <name>, --test <name> (tests/<name>.rs), or --lib (the library's #[cfg(test)] unit tests)", 2);
    }
    if breaks.is_empty() && fn_breaks.is_empty() && !panic {
        return fail("user_error", &format!(
            "{verb} needs at least one --break <file.rs:line>, --break-fn <name>, or --panic\n  \
             e.g.  rdbg {verb} --cargo . --lib --break src/lib.rs:42 -- my_test"), 2);
    }
    let program: PathBuf = if let Some(c) = cargo {
        match build_target(&PathBuf::from(&c), bin.as_deref(), test.as_deref(), lib, &mut args) {
            Ok(p) => p,
            // a missing/unknown cargo target vs. a compile failure
            Err(e) => return fail(classify_build_error(&e), &e, 2),
        }
    } else if let Some(bp) = bin_path {
        match PathBuf::from(&bp).canonicalize() {
            Ok(p) => p,
            Err(_) => return fail("user_error", &format!(
                "--bin-path {bp:?} does not exist — build it first, or use --cargo <dir> to build and debug in one step"), 2),
        }
    } else {
        return fail("user_error", &format!(
            "{verb} needs --cargo <dir> (build then debug) or --bin-path <path> (a prebuilt binary)\n{usage}"), 2);
    };
    let cwd = std::env::current_dir().unwrap();
    let mut bps: Vec<Value> = vec![];
    for b in &breaks {
        match parse_bp_soft(b, &cwd) {
            Ok((f, l)) => bps.push(json!({"file": f, "line": l})),
            Err(e) => return fail("user_error", &e, 2),
        }
    }
    eprintln!("debugging {}", program.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default());
    let v = jresp(request(ws, &json!({"cmd": "launch", "program": program.to_string_lossy(),
        "cwd": program.parent().map(|p| p.to_string_lossy().to_string()),
        "args": args, "breakpoints": bps, "fn_breaks": fn_breaks, "panic": panic}), Duration::from_secs(300)));
    if !v["ok"].as_bool().unwrap_or(false) {
        if json_mode {
            println!("{v}");
        } else {
            eprintln!("launch failed: {}", v["error"].as_str().unwrap_or("unknown"));
        }
        return 1;
    }
    if !trace_mode {
        if json_mode {
            println!("{v}");
        } else {
            println!("{}", fmt_stop(&v["stop"]));
        }
        return 0;
    }
    // run through all hits and return the compact trace in one call
    let tv = jresp(request(ws, &json!({"cmd": "trace", "captures": captures, "max": max}), Duration::from_secs(300)));
    let trace_ok = tv["ok"].as_bool().unwrap_or(false);
    if json_mode {
        println!("{tv}");
        return if trace_ok { 0 } else { 1 };
    }
    if trace_ok {
        println!("trace: {} hit(s)", tv["hits"].as_i64().unwrap_or(0));
        println!("{}", tv["trace"].as_str().unwrap_or(""));
        if let Some(o) = tv["output"].as_str() { if !o.is_empty() { println!("--- output ---\n{}", o.trim_end()); } }
        0
    } else {
        eprintln!("trace failed: {}", tv["error"].as_str().unwrap_or("unknown"));
        1
    }
}

#[cfg(test)]
mod tests {
    use super::pick_artifact;

    fn artifact(name: &str, kind: &str, test: bool, exe: &str) -> String {
        format!(r#"{{"reason":"compiler-artifact","target":{{"name":"{name}","kind":["{kind}"]}},"profile":{{"test":{test}}},"executable":"{exe}"}}"#)
    }

    #[test]
    fn lib_mode_picks_the_lib_unittest_binary() {
        let lines = [
            artifact("build-script-build", "custom-build", false, "/t/build/bs"),
            r#"{"reason":"compiler-artifact","target":{"name":"serde","kind":["lib"]},"profile":{"test":false},"executable":null}"#.to_string(),
            artifact("rpncalc", "lib", true, "/t/debug/deps/rpncalc-abc"),
        ].join("\n");
        assert_eq!(pick_artifact(&lines, None, None, true).unwrap().to_string_lossy(), "/t/debug/deps/rpncalc-abc");
    }

    #[test]
    fn named_bin_and_test_targets_win_over_others() {
        let lines = [
            artifact("other", "bin", false, "/t/debug/other"),
            artifact("app", "bin", false, "/t/debug/app"),
        ].join("\n");
        assert_eq!(pick_artifact(&lines, Some("app"), None, false).unwrap().to_string_lossy(), "/t/debug/app");
        let lines = [
            artifact("app", "bin", true, "/t/debug/deps/app-1"),
            artifact("mytest", "test", true, "/t/debug/deps/mytest-1"),
        ].join("\n");
        assert_eq!(pick_artifact(&lines, None, Some("mytest"), false).unwrap().to_string_lossy(), "/t/debug/deps/mytest-1");
    }

    #[test]
    fn build_scripts_are_never_picked() {
        let lines = artifact("build-script-build", "custom-build", false, "/t/build/bs");
        assert!(pick_artifact(&lines, None, None, false).is_none());
    }
}
