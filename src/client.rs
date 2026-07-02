//! The `rdbg` command-line client: dispatches subcommands to the daemon.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

pub const USAGE: &str = "\
rdbg — IDE-grade Rust debugging for agents (rust-analyzer + lldb-dap)

LAUNCH
  rdbg launch --cargo <dir> [--bin <t>|--test <t>] --break f.rs:L [...] [-- ARGS]
  rdbg launch --bin-path <path> --break f.rs:L [...]      (skip the build)
    at launch you may also pass  --break-fn <name>  --panic

TRACE (one call instead of break→inspect→continue×N)
  rdbg trace --cargo . --bin app --break f.rs:L --capture i,sum --max 30 -- ARGS
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

  rdbg status | stop | down | mcp";

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

pub fn cargo_build(dir: &Path, bin: Option<&str>, test: Option<&str>) -> PathBuf {
    let mut cmd = Command::new("cargo");
    cmd.current_dir(dir).arg(if test.is_some() { "test" } else { "build" }).arg("--message-format=json");
    if let Some(t) = test {
        cmd.arg("--no-run");
        if t == "lib" {
            cmd.arg("--lib");
        } else {
            cmd.args(["--test", t]);
        }
    }
    if let Some(b) = bin {
        cmd.args(["--bin", b]);
    }
    eprintln!("building …");
    let out = cmd.output().expect("run cargo");
    let mut exe: Option<PathBuf> = None;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Ok(m): Result<Value, _> = serde_json::from_str(line) else { continue };
        if m["reason"] == "compiler-artifact" {
            if let Some(e) = m["executable"].as_str() {
                let name = m["target"]["name"].as_str().unwrap_or("");
                if (bin == Some(name)) || (test == Some(name)) || (test == Some("lib")) {
                    exe = Some(PathBuf::from(e));
                } else if exe.is_none() {
                    exe = Some(PathBuf::from(e));
                }
            }
        }
    }
    exe.unwrap_or_else(|| {
        eprintln!("{}", String::from_utf8_lossy(&out.stderr));
        std::process::exit(2);
    })
}

fn parse_bp(spec: &str, base: &Path) -> (String, i64) {
    match parse_bp_soft(spec, base) {
        Ok(bp) => bp,
        Err(e) => { eprintln!("{e}"); std::process::exit(2); }
    }
}

/// Like `parse_bp`, but returns an error instead of exiting the process — used
/// by `run_command`, which the MCP server and `do` drive (never exit there).
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

/// Render a stop summary as text (returned, not printed — callers `println!` it,
/// or thread it through `do` / MCP).
fn fmt_stop(stop: &Value) -> String {
    if stop.is_null() {
        return "(no stop — not paused)".to_string();
    }
    if stop["exited"].as_bool().unwrap_or(false) {
        let mut out = format!(">>> program exited (code {})",
            stop["exit_code"].as_i64().map(|c| c.to_string()).unwrap_or("?".into()));
        if let Some(o) = stop["output"].as_str() {
            if !o.is_empty() {
                out.push_str(&format!("\n--- program output ---\n{}", o.trim_end()));
            }
        }
        return out;
    }
    let mut lines = vec![format!(">>> STOP [{}] {}  (thread {})",
        stop["reason"].as_str().unwrap_or("?"), stop["frame"].as_str().unwrap_or("?"),
        stop["thread"].as_i64().unwrap_or(0))];
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

fn fmt_result_stop(r: Option<Value>) -> String {
    match r {
        Some(v) if v["ok"].as_bool().unwrap_or(false) => fmt_stop(&v["stop"]),
        Some(v) => format!("error: {}", v["error"].as_str().unwrap_or("unknown")),
        None => "error: daemon did not respond".to_string(),
    }
}

pub fn main(args: &[String]) -> i32 {
    if args.is_empty() {
        println!("{USAGE}");
        return 0;
    }
    let cmd = args[0].as_str();
    let rest = &args[1..];
    match cmd {
        "--help" | "-h" | "help" => { println!("{USAGE}"); return 0; }
        "--version" | "-V" => { println!("rdbg {}", env!("CARGO_PKG_VERSION")); return 0; }
        _ => {}
    }
    let ws = ws_root();

    if cmd == "down" {
        request(&ws, &json!({"cmd": "shutdown"}), Duration::from_secs(5));
        println!("rdbg: daemon stopped");
        return 0;
    }
    ensure_daemon(&ws);

    match cmd {
        "launch" => do_launch(&ws, rest, false),
        "trace" => do_launch(&ws, rest, true),
        "do" => {
            // run several subcommands in one call: `rdbg do 'break f:L; continue; vars'`
            let (text, had_error) = run_batch(&ws, &rest.join(" "));
            print!("{text}");
            if had_error { 1 } else { 0 }
        }
        _ => {
            let out = run_command(&ws, cmd, rest);
            println!("{out}");
            if out == USAGE { 2 } else { 0 }
        }
    }
}

/// Run one subcommand against the daemon and return its rendered text (instead
/// of printing it inline). `main` prints this; `do` and the MCP `debug_do` tool
/// collect it. Never exits the process — safe to call from the MCP server.
fn run_command(ws: &Path, cmd: &str, rest: &[String]) -> String {
    let r = |p: Value| request(ws, &p, Duration::from_secs(300));
    let cwd = std::env::current_dir().unwrap_or_default();
    match cmd {
        "status" => serde_json::to_string_pretty(&r(json!({"cmd": "status"})).unwrap_or(Value::Null)).unwrap(),
        "break" => {
            if rest.iter().any(|a| a == "--fn") {
                let resp = r(json!({"cmd": "bp_fn", "name": opt(rest, "--fn").unwrap_or("")}));
                resp.map(|v| format!("fn breakpoint [{}]", v["id"])).unwrap_or_else(|| "error".into())
            } else if rest.iter().any(|a| a == "--panic") {
                r(json!({"cmd": "bp_panic"}));
                "panic breakpoint [panic] set (breaks where a Rust panic is raised)".to_string()
            } else {
                let Some(spec) = rest.first() else { return "error: break needs file.rs:line".to_string() };
                let (f, l) = match parse_bp_soft(spec, &cwd) { Ok(v) => v, Err(e) => return format!("error: {e}") };
                let hit = opt(rest, "--hit").and_then(|h| h.parse::<i64>().ok());
                let resp = r(json!({"cmd": "bp_add", "file": f, "line": l,
                    "condition": opt_multi(rest, "--if"), "hit": hit, "log": opt_multi(rest, "--log")}));
                match resp {
                    Some(v) if v["ok"].as_bool().unwrap_or(false) => {
                        let warn = if v["verified"].as_bool().unwrap_or(true) { "" } else { "  (UNVERIFIED — no code at that line?)" };
                        format!("breakpoint [{}] {}{}", v["id"], spec, warn)
                    }
                    _ => "error setting breakpoint".to_string(),
                }
            }
        }
        "watch" => {
            let Some(var) = rest.first().cloned() else { return "error: watch needs a variable".to_string() };
            match r(json!({"cmd": "bp_watch", "var": var})) {
                Some(v) if v["ok"].as_bool().unwrap_or(false) => format!("watchpoint [{}] on {} (breaks when it changes)", v["id"], var),
                Some(v) => format!("error: {}", v["error"].as_str().unwrap_or("unknown")),
                None => "error: daemon did not respond".to_string(),
            }
        }
        "breaks" => fmt_field(r(json!({"cmd": "bp_list"})), "breakpoints"),
        "break-rm" => { r(json!({"cmd": "bp_rm", "id": rest.first().cloned().unwrap_or_default()})); "ok".to_string() }
        "break-on" | "break-off" => {
            r(json!({"cmd": "bp_enable", "id": rest.first().cloned().unwrap_or_default(), "enabled": cmd == "break-on"}));
            "ok".to_string()
        }
        "run" | "continue" => fmt_result_stop(r(json!({"cmd": "continue"}))),
        "step" => fmt_result_stop(r(json!({"cmd": "step", "kind": rest.first().map(|s| s.as_str()).unwrap_or("over")}))),
        "until" => {
            let Some(spec) = rest.first() else { return "error: until needs file.rs:line".to_string() };
            let (f, l) = match parse_bp_soft(spec, &cwd) { Ok(v) => v, Err(e) => return format!("error: {e}") };
            fmt_result_stop(r(json!({"cmd": "until", "file": f, "line": l})))
        }
        "pause" => fmt_result_stop(r(json!({"cmd": "pause"}))),
        "restart" => fmt_result_stop(r(json!({"cmd": "restart"}))),
        "threads" => fmt_field(r(json!({"cmd": "threads"})), "threads"),
        "thread" => fmt_result_stop(r(json!({"cmd": "thread", "id": rest.first().and_then(|s| s.parse::<i64>().ok()).unwrap_or(0)}))),
        "frame" | "up" | "down" => {
            let payload = if cmd == "up" || cmd == "down" {
                json!({"cmd": "frame", "dir": cmd})
            } else {
                json!({"cmd": "frame", "index": rest.first().and_then(|s| s.parse::<i64>().ok()).unwrap_or(0)})
            };
            match r(payload) {
                Some(v) if v["ok"].as_bool().unwrap_or(false) => {
                    let mut lines = vec![];
                    if let Some(src) = v["source"].as_str() { lines.push(src.to_string()); }
                    lines.push("locals:".to_string());
                    if let Some(vars) = v["vars"].as_str() { lines.push(vars.to_string()); }
                    lines.join("\n")
                }
                _ => "no such frame".to_string(),
            }
        }
        "vars" => {
            let full = rest.iter().any(|a| a == "--full");
            let mut payload = json!({"cmd": "vars", "full": full});
            if let Some(d) = opt(rest, "--depth").and_then(|d| d.parse::<i64>().ok()) {
                payload["depth"] = json!(d);
            }
            fmt_field(r(payload), "vars")
        }
        "eval" => {
            // evaluate one or more variable paths in a single agent call
            let mut out = vec![];
            for path in rest.iter().filter(|p| !p.starts_with("--")) {
                let v = r(json!({"cmd": "eval", "expr": path}));
                let val = v.as_ref().filter(|v| v["ok"].as_bool().unwrap_or(false))
                    .map(|v| v["value"].as_str().unwrap_or("").to_string())
                    .unwrap_or_else(|| "error".into());
                out.push(format!("{path} = {val}"));
            }
            out.join("\n")
        }
        "set" => {
            // rdbg set <path> = <value> [--then continue|step]   (test a fix live)
            let then = rest.iter().position(|a| a == "--then");
            let body: Vec<String> = rest.iter().take(then.unwrap_or(rest.len())).cloned().collect();
            if body.is_empty() { return "error: set needs <path> = <value>".to_string(); }
            let joined = body.join(" ");
            let (path, value) = match joined.split_once('=') {
                Some((p, v)) => (p.trim().to_string(), v.trim().to_string()),
                None => (body[0].clone(), body[1..].join(" ")),
            };
            let mut out = vec![fmt_field(r(json!({"cmd": "set", "path": path, "value": value})), "value")];
            if let Some(i) = then {
                let after = rest.get(i + 1).map(|s| s.as_str()).unwrap_or("continue");
                out.push(fmt_result_stop(if after == "step" { r(json!({"cmd": "step", "kind": "over"})) } else { r(json!({"cmd": "continue"})) }));
            }
            out.join("\n")
        }
        "watch-expr" => {
            let action = rest.first().map(|s| s.as_str()).filter(|s| *s == "add" || *s == "rm").unwrap_or("list");
            let expr = if action != "list" { Some(rest[1..].join(" ")) } else { None };
            fmt_field(r(json!({"cmd": "watch_expr", "action": action, "expr": expr})), "watches")
        }
        "bt" => fmt_field(r(json!({"cmd": "bt"})), "bt"),
        "list" => {
            let radius = opt(rest, "--radius").and_then(|d| d.parse::<i64>().ok()).unwrap_or(6);
            fmt_field(r(json!({"cmd": "list", "radius": radius})), "source")
        }
        "state" => match r(json!({"cmd": "state"})) {
            Some(v) if v["ok"].as_bool().unwrap_or(false) => {
                let mut lines = vec![fmt_stop(&v["stop"]), "locals:".to_string()];
                if let Some(vars) = v["vars"].as_str() { lines.push(vars.to_string()); }
                if let Some(w) = v["watches"].as_str() { if !w.is_empty() { lines.push(format!("watches:\n{w}")); } }
                lines.join("\n")
            }
            _ => "error".to_string(),
        },
        "stop" => { r(json!({"cmd": "stop"})); "debug session ended".to_string() }
        "where" => match r(json!({"cmd": "where", "query": rest.first().cloned().unwrap_or_default()})) {
            Some(v) if v["ok"].as_bool().unwrap_or(false) => {
                v["symbols"].as_array().cloned().unwrap_or_default().iter().map(|s| {
                    let c = s["container"].as_str().map(|c| format!(" ({c})")).unwrap_or_default();
                    format!("  {}{}  {}:{}", s["name"].as_str().unwrap_or("?"), c, s["file"].as_str().unwrap_or("?"), s["line"])
                }).collect::<Vec<_>>().join("\n")
            }
            Some(v) => format!("error: {}", v["error"].as_str().unwrap_or("unknown")),
            None => "error".to_string(),
        },
        "def" | "refs" | "hover" => {
            let Some(f) = rest.first().cloned() else { return "error: needs <file> <line> <col>".to_string() };
            let (l, c) = (rest.get(1).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0),
                          rest.get(2).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0));
            match r(json!({"cmd": cmd, "file": f, "line": l, "col": c})) {
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
            }
        }
        _ => USAGE.to_string(),
    }
}

/// Run a `;`-separated batch of subcommands, labeling each with `$ <subcommand>`.
/// Stops at the first error or program exit. Returns the combined text and
/// whether any subcommand errored. Shared by the CLI `do` and the MCP `debug_do`.
pub fn run_batch(ws: &Path, script: &str) -> (String, bool) {
    let mut out = String::new();
    let mut had_error = false;
    for part in script.split(';') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let toks: Vec<String> = part.split_whitespace().map(String::from).collect();
        let text = run_command(ws, toks[0].as_str(), &toks[1..]);
        // an unknown subcommand falls through to the full USAGE — flag it, don't dump it
        if text == USAGE {
            out.push_str(&format!("$ {part}\nerror: unknown subcommand {:?} (not usable inside `do`)\n\n", toks[0]));
            had_error = true;
            break;
        }
        out.push_str(&format!("$ {part}\n{text}\n\n"));
        let errored = text.lines().any(|l| l.trim_start().starts_with("error:"));
        let exited = text.contains(">>> program exited");
        had_error |= errored;
        if errored || exited {
            break;
        }
    }
    (out, had_error)
}

fn fmt_field(resp: Option<Value>, field: &str) -> String {
    match resp {
        Some(v) if v["ok"].as_bool().unwrap_or(false) => v[field].as_str().unwrap_or("").to_string(),
        Some(v) => format!("error: {}", v["error"].as_str().unwrap_or("unknown")),
        None => "error: daemon did not respond".to_string(),
    }
}

fn do_launch(ws: &Path, rest: &[String], trace_mode: bool) -> i32 {
    let (mut cargo, mut bin_path, mut bin, mut test): (Option<String>, Option<String>, Option<String>, Option<String>) = (None, None, None, None);
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
            "--break" => { if let Some(b) = rest.get(i + 1) { breaks.push(b.clone()); } i += 2; }
            "--break-fn" => { if let Some(b) = rest.get(i + 1) { fn_breaks.push(b.clone()); } i += 2; }
            "--capture" => { if let Some(c) = rest.get(i + 1) { captures.extend(c.split(',').map(|s| s.trim().to_string())); } i += 2; }
            "--max" => { max = rest.get(i + 1).and_then(|n| n.parse().ok()).unwrap_or(50); i += 2; }
            "--panic" => { panic = true; i += 1; }
            "--" => { args = rest[i + 1..].to_vec(); break; }
            other => { eprintln!("unknown launch arg {other:?}"); return 2; }
        }
    }
    if breaks.is_empty() && fn_breaks.is_empty() && !panic {
        eprintln!("launch needs at least one --break / --break-fn / --panic");
        return 2;
    }
    let program: PathBuf = if let Some(c) = cargo {
        cargo_build(&PathBuf::from(&c), bin.as_deref(), test.as_deref())
    } else if let Some(bp) = bin_path {
        PathBuf::from(bp).canonicalize().unwrap_or_else(|_| PathBuf::from("missing"))
    } else {
        eprintln!("launch needs --cargo <dir> or --bin-path <path>");
        return 2;
    };
    let cwd = std::env::current_dir().unwrap();
    let bps: Vec<Value> = breaks.iter().map(|b| { let (f, l) = parse_bp(b, &cwd); json!({"file": f, "line": l}) }).collect();
    eprintln!("debugging {}", program.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default());
    let resp = request(ws, &json!({"cmd": "launch", "program": program.to_string_lossy(),
        "cwd": program.parent().map(|p| p.to_string_lossy().to_string()),
        "args": args, "breakpoints": bps, "fn_breaks": fn_breaks, "panic": panic}), Duration::from_secs(300));
    match resp {
        Some(v) if v["ok"].as_bool().unwrap_or(false) => {
            if !trace_mode {
                println!("{}", fmt_stop(&v["stop"]));
                return 0;
            }
            // run through all hits and return the compact trace in one call
            let t = request(ws, &json!({"cmd": "trace", "captures": captures, "max": max}), Duration::from_secs(300));
            match t {
                Some(tv) if tv["ok"].as_bool().unwrap_or(false) => {
                    println!("trace: {} hit(s)", tv["hits"].as_i64().unwrap_or(0));
                    println!("{}", tv["trace"].as_str().unwrap_or(""));
                    if let Some(o) = tv["output"].as_str() { if !o.is_empty() { println!("--- output ---\n{}", o.trim_end()); } }
                    0
                }
                _ => { eprintln!("trace failed"); 1 }
            }
        }
        Some(v) => { eprintln!("launch failed: {}", v["error"].as_str().unwrap_or("unknown")); 1 }
        None => { eprintln!("launch failed: daemon did not respond"); 1 }
    }
}
