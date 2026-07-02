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
  rdbg vars [--depth N]         locals with real Rust values
  rdbg eval <path>             evaluate a variable path (foo.bar[2].x)
  rdbg set <path> = <value>    change a variable's value
  rdbg watch-expr add|rm <path>
  rdbg list [--radius N] | bt | state

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
    let Some((f, l)) = spec.rsplit_once(':') else {
        eprintln!("bad breakpoint {spec:?} (want file.rs:line)");
        std::process::exit(2);
    };
    let line: i64 = l.parse().unwrap_or_else(|_| { eprintln!("bad line in {spec:?}"); std::process::exit(2); });
    let p = Path::new(f);
    let abs = if p.is_absolute() { p.to_path_buf() } else { base.join(f) };
    let abs = abs.canonicalize().unwrap_or(abs);
    (abs.to_string_lossy().to_string(), line)
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

fn print_stop(stop: &Value) {
    if stop.is_null() {
        println!("(no stop — not paused)");
        return;
    }
    if stop["exited"].as_bool().unwrap_or(false) {
        println!(">>> program exited (code {})", stop["exit_code"].as_i64().map(|c| c.to_string()).unwrap_or("?".into()));
        if let Some(o) = stop["output"].as_str() {
            if !o.is_empty() {
                println!("--- program output ---\n{}", o.trim_end());
            }
        }
        return;
    }
    println!(">>> STOP [{}] {}  (thread {})",
        stop["reason"].as_str().unwrap_or("?"), stop["frame"].as_str().unwrap_or("?"),
        stop["thread"].as_i64().unwrap_or(0));
    if let Some(src) = stop["source"].as_str() {
        if !src.is_empty() {
            println!("{src}");
        }
    }
    if let Some(w) = stop["watches"].as_str() {
        if !w.is_empty() {
            println!("watches:\n{w}");
        }
    }
}

fn print_result_stop(r: Option<Value>) {
    match r {
        Some(v) if v["ok"].as_bool().unwrap_or(false) => print_stop(&v["stop"]),
        Some(v) => println!("error: {}", v["error"].as_str().unwrap_or("unknown")),
        None => println!("error: daemon did not respond"),
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
    let r = |p: Value| request(&ws, &p, Duration::from_secs(300));

    match cmd {
        "status" => println!("{}", serde_json::to_string_pretty(&r(json!({"cmd": "status"})).unwrap_or(Value::Null)).unwrap()),
        "launch" => return do_launch(&ws, rest),
        "break" => {
            if rest.iter().any(|a| a == "--fn") {
                let resp = r(json!({"cmd": "bp_fn", "name": opt(rest, "--fn").unwrap_or("")}));
                println!("{}", resp.map(|v| format!("fn breakpoint [{}]", v["id"])).unwrap_or_else(|| "error".into()));
            } else if rest.iter().any(|a| a == "--panic") {
                r(json!({"cmd": "bp_panic"}));
                println!("panic breakpoint [panic] set (breaks where a Rust panic is raised)");
            } else {
                let (f, l) = parse_bp(&rest[0], &std::env::current_dir().unwrap());
                let hit = opt(rest, "--hit").and_then(|h| h.parse::<i64>().ok());
                let resp = r(json!({"cmd": "bp_add", "file": f, "line": l,
                    "condition": opt_multi(rest, "--if"), "hit": hit, "log": opt_multi(rest, "--log")}));
                match resp {
                    Some(v) if v["ok"].as_bool().unwrap_or(false) => {
                        let warn = if v["verified"].as_bool().unwrap_or(true) { "" } else { "  (UNVERIFIED — no code at that line?)" };
                        println!("breakpoint [{}] {}{}", v["id"], rest[0], warn);
                    }
                    _ => println!("error setting breakpoint"),
                }
            }
        }
        "watch" => {
            let resp = r(json!({"cmd": "bp_watch", "var": rest.first().cloned().unwrap_or_default()}));
            match resp {
                Some(v) if v["ok"].as_bool().unwrap_or(false) => println!("watchpoint [{}] on {} (breaks when it changes)", v["id"], rest[0]),
                Some(v) => println!("error: {}", v["error"].as_str().unwrap_or("unknown")),
                None => println!("error: daemon did not respond"),
            }
        }
        "breaks" => print_field(r(json!({"cmd": "bp_list"})), "breakpoints"),
        "break-rm" => { r(json!({"cmd": "bp_rm", "id": rest.first().cloned().unwrap_or_default()})); println!("ok"); }
        "break-on" | "break-off" => {
            r(json!({"cmd": "bp_enable", "id": rest.first().cloned().unwrap_or_default(), "enabled": cmd == "break-on"}));
            println!("ok");
        }
        "run" | "continue" => print_result_stop(r(json!({"cmd": "continue"}))),
        "step" => print_result_stop(r(json!({"cmd": "step", "kind": rest.first().map(|s| s.as_str()).unwrap_or("over")}))),
        "until" => {
            let (f, l) = parse_bp(&rest[0], &std::env::current_dir().unwrap());
            print_result_stop(r(json!({"cmd": "until", "file": f, "line": l})));
        }
        "pause" => print_result_stop(r(json!({"cmd": "pause"}))),
        "restart" => print_result_stop(r(json!({"cmd": "restart"}))),
        "threads" => print_field(r(json!({"cmd": "threads"})), "threads"),
        "thread" => print_result_stop(r(json!({"cmd": "thread", "id": rest.first().and_then(|s| s.parse::<i64>().ok()).unwrap_or(0)}))),
        "frame" | "up" | "down" => {
            let payload = if cmd == "up" || cmd == "down" {
                json!({"cmd": "frame", "dir": cmd})
            } else {
                json!({"cmd": "frame", "index": rest.first().and_then(|s| s.parse::<i64>().ok()).unwrap_or(0)})
            };
            match r(payload) {
                Some(v) if v["ok"].as_bool().unwrap_or(false) => {
                    if let Some(src) = v["source"].as_str() { println!("{src}"); }
                    println!("locals:");
                    if let Some(vars) = v["vars"].as_str() { println!("{vars}"); }
                }
                _ => println!("no such frame"),
            }
        }
        "vars" => {
            let depth = opt(rest, "--depth").and_then(|d| d.parse::<i64>().ok()).unwrap_or(3);
            print_field(r(json!({"cmd": "vars", "depth": depth})), "vars");
        }
        "eval" => print_field(r(json!({"cmd": "eval", "expr": rest.first().cloned().unwrap_or_default()})), "value"),
        "set" => {
            let joined = rest.join(" ");
            let (path, value) = match joined.split_once('=') {
                Some((p, v)) => (p.trim().to_string(), v.trim().to_string()),
                None => (rest.first().cloned().unwrap_or_default(), rest[1..].join(" ")),
            };
            print_field(r(json!({"cmd": "set", "path": path, "value": value})), "value");
        }
        "watch-expr" => {
            let action = rest.first().map(|s| s.as_str()).filter(|s| *s == "add" || *s == "rm").unwrap_or("list");
            let expr = if action != "list" { Some(rest[1..].join(" ")) } else { None };
            print_field(r(json!({"cmd": "watch_expr", "action": action, "expr": expr})), "watches");
        }
        "bt" => print_field(r(json!({"cmd": "bt"})), "bt"),
        "list" => {
            let radius = opt(rest, "--radius").and_then(|d| d.parse::<i64>().ok()).unwrap_or(6);
            print_field(r(json!({"cmd": "list", "radius": radius})), "source");
        }
        "state" => match r(json!({"cmd": "state"})) {
            Some(v) if v["ok"].as_bool().unwrap_or(false) => {
                print_stop(&v["stop"]);
                println!("locals:");
                if let Some(vars) = v["vars"].as_str() { println!("{vars}"); }
                if let Some(w) = v["watches"].as_str() { if !w.is_empty() { println!("watches:\n{w}"); } }
            }
            _ => println!("error"),
        },
        "stop" => { r(json!({"cmd": "stop"})); println!("debug session ended"); }
        "where" => match r(json!({"cmd": "where", "query": rest.first().cloned().unwrap_or_default()})) {
            Some(v) if v["ok"].as_bool().unwrap_or(false) => {
                for s in v["symbols"].as_array().cloned().unwrap_or_default() {
                    let c = s["container"].as_str().map(|c| format!(" ({c})")).unwrap_or_default();
                    println!("  {}{}  {}:{}", s["name"].as_str().unwrap_or("?"), c, s["file"].as_str().unwrap_or("?"), s["line"]);
                }
            }
            Some(v) => println!("error: {}", v["error"].as_str().unwrap_or("unknown")),
            None => println!("error"),
        },
        "def" | "refs" | "hover" => {
            let (f, l, c) = (rest[0].clone(), rest.get(1).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0),
                             rest.get(2).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0));
            match r(json!({"cmd": cmd, "file": f, "line": l, "col": c})) {
                Some(v) if v["ok"].as_bool().unwrap_or(false) => {
                    if cmd == "hover" {
                        println!("{}", v["hover"].as_str().filter(|s| !s.is_empty()).unwrap_or("(no hover)"));
                    } else {
                        let locs = v["locations"].as_array().cloned().unwrap_or_default();
                        if locs.is_empty() { println!("(none)"); }
                        for loc in locs {
                            println!("  {}:{}:{}", loc["file"].as_str().unwrap_or("?"), loc["line"], loc["col"]);
                        }
                    }
                }
                Some(v) => println!("error: {}", v["error"].as_str().unwrap_or("unknown")),
                None => println!("error"),
            }
        }
        _ => { println!("{USAGE}"); return 2; }
    }
    0
}

fn print_field(resp: Option<Value>, field: &str) {
    match resp {
        Some(v) if v["ok"].as_bool().unwrap_or(false) => println!("{}", v[field].as_str().unwrap_or("")),
        Some(v) => println!("error: {}", v["error"].as_str().unwrap_or("unknown")),
        None => println!("error: daemon did not respond"),
    }
}

fn do_launch(ws: &Path, rest: &[String]) -> i32 {
    let (mut cargo, mut bin_path, mut bin, mut test): (Option<String>, Option<String>, Option<String>, Option<String>) = (None, None, None, None);
    let mut breaks: Vec<String> = vec![];
    let mut fn_breaks: Vec<String> = vec![];
    let mut args: Vec<String> = vec![];
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
        Some(v) if v["ok"].as_bool().unwrap_or(false) => { print_stop(&v["stop"]); 0 }
        Some(v) => { eprintln!("launch failed: {}", v["error"].as_str().unwrap_or("unknown")); 1 }
        None => { eprintln!("launch failed: daemon did not respond"); 1 }
    }
}
