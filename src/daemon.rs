//! Per-project background server. Holds one paused debug session and a warm
//! rust-analyzer, and serves one JSON request per Unix-socket connection.

use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::lsp::Lsp;
use crate::session::{first_user_frame, parse_predicate, Session, Stop, UntilOutcome};
use crate::util::{abs, classify_error, extract_panic_message};

const IDLE_SHUTDOWN: Duration = Duration::from_secs(30 * 60);

pub fn socket_path(ws: &str) -> PathBuf {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    ws.hash(&mut h);
    // short path — macOS AF_UNIX sun_path is ~104 bytes
    PathBuf::from(format!("/tmp/rdbg-{:016x}.sock", h.finish()))
}

fn state_dir(ws: &str) -> PathBuf {
    PathBuf::from(ws).join(".rdbg")
}

struct Daemon {
    session: Option<Session>,
    lsp: Option<Lsp>,
    ws: String,
    last: Instant,
}

pub fn serve(ws: &str) {
    let ws = abs(ws);
    let sock = socket_path(&ws);
    let _ = std::fs::remove_file(&sock);
    let listener = match UnixListener::bind(&sock) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("rdbg daemon: cannot bind {}: {e}", sock.display());
            return;
        }
    };
    let dir = state_dir(&ws);
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(
        dir.join("daemon.json"),
        json!({"socket": sock.to_string_lossy(), "pid": std::process::id()}).to_string(),
    );

    let daemon = Arc::new(Mutex::new(Daemon { session: None, lsp: None, ws: ws.to_string(), last: Instant::now() }));

    // rust-analyzer is started lazily on the first navigation command (where/def/
    // hover/refs). Debug-only sessions never pay its indexing cost — which on a large
    // repo (e.g. 1.7M lines) is minutes of CPU/RAM competing with the build and lldb.
    // idle shutdown
    {
        let d = Arc::clone(&daemon);
        let sock = sock.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(60));
            if d.lock().unwrap().last.elapsed() > IDLE_SHUTDOWN {
                let _ = std::fs::remove_file(&sock);
                std::process::exit(0);
            }
        });
    }

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let done = handle(&daemon, stream);
        if done {
            break;
        }
    }
    let _ = std::fs::remove_file(&sock);
}

fn handle(daemon: &Arc<Mutex<Daemon>>, stream: UnixStream) -> bool {
    let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
        return false;
    }
    let req: Value = serde_json::from_str(line.trim()).unwrap_or(Value::Null);
    let mut d = daemon.lock().unwrap();
    d.last = Instant::now();
    let (resp, shutdown) = d.dispatch(&req);
    let mut w = stream;
    let _ = w.write_all(serde_json::to_string(&resp).unwrap().as_bytes());
    let _ = w.write_all(b"\n");
    let _ = w.flush();
    shutdown
}

/// Compact tabular render of a trace run — one line per breakpoint hit.
fn format_trace(hits: &[crate::session::TraceHit]) -> String {
    if hits.is_empty() {
        return "(no breakpoint hits)".to_string();
    }
    hits.iter().enumerate().map(|(i, h)| {
        let vals = h.values.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("  ");
        format!(" #{:<3} {}  {}   {}", i + 1, h.func, h.loc, vals)
    }).collect::<Vec<_>>().join("\n")
}

/// The last `n` bytes of program output, snapped to a char boundary.
fn tail(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut i = s.len() - n;
    while !s.is_char_boundary(i) {
        i += 1;
    }
    s[i..].to_string()
}

/// Read-only stop summary (frame + source + watches).
fn summarize(s: &Session, stop: &Stop) -> Value {
    if stop.exited {
        return json!({"exited": true, "exit_code": stop.exit_code});
    }
    let frame = stop.top().map(|f| format!("{}  {}:{}", f.name, f.file, f.line)).unwrap_or_else(|| "?".into());
    json!({
        "exited": false, "reason": stop.reason, "frame": frame, "thread": stop.thread_id,
        "source": s.source_around(6),
        "watches": if s.watches.is_empty() { String::new() } else { s.watches_text() },
    })
}

impl Daemon {
    /// Summary of a fresh stop; drops the session and captures output on exit.
    fn stop_summary(&mut self, stop: Stop) -> Value {
        if stop.exited {
            let (out, bps): (String, Value) = self.session.as_ref()
                .map(|s| (s.output.concat(), json!(s.bp_summary())))
                .unwrap_or_default();
            self.session = None;
            let tail = if out.len() > 2000 { out[out.len() - 2000..].to_string() } else { out };
            return json!({"exited": true, "exit_code": stop.exit_code, "output": tail, "breakpoints": bps});
        }
        // snapshot + diff the top-frame locals before borrowing immutably to summarize
        let delta = self.session.as_mut().map(|s| s.locals_delta()).unwrap_or_default();
        let mut summary = summarize(self.session.as_ref().unwrap(), &stop);
        summary["delta"] = json!(delta);
        summary
    }

    /// Run a session action that yields a stop, then summarize it — without
    /// holding a session borrow across `stop_summary`.
    fn run_stop<F: FnOnce(&mut Session) -> Result<Stop, String>>(&mut self, f: F) -> Value {
        let result = { f(self.session.as_mut().unwrap()) };
        match result {
            Ok(stop) => json!({"ok": true, "stop": self.stop_summary(stop)}),
            Err(e) => json!({"ok": false, "error": e}),
        }
    }

    /// Dispatch one request and stamp the response with a `status` outcome
    /// (`ok | user_error | target_error | build_error | debug_adapter_error |
    /// timeout | no_session | no_new_information`) so every result is
    /// machine-scorable. `ok:bool` stays alongside it for back-compat.
    fn dispatch(&mut self, req: &Value) -> (Value, bool) {
        let (mut resp, shutdown) = self.dispatch_inner(req);
        if resp.get("status").is_none() {
            resp["status"] = json!(if resp["ok"].as_bool().unwrap_or(false) {
                "ok"
            } else {
                classify_error(resp["error"].as_str().unwrap_or(""))
            });
        }
        (resp, shutdown)
    }

    fn dispatch_inner(&mut self, req: &Value) -> (Value, bool) {
        let cmd = req["cmd"].as_str().unwrap_or("");
        match cmd {
            "ping" => return (json!({"ok": true}), false),
            "shutdown" => return (json!({"ok": true}), true),
            "status" => {
                let s = self.session.as_ref();
                return (json!({
                    "ok": true,
                    "session": s.is_some(),
                    "stopped": s.map(|s| s.last_stop.as_ref().map(|x| !x.exited).unwrap_or(false)).unwrap_or(false),
                    "lsp_ready": self.lsp.as_ref().map(|l| l.is_ready()).unwrap_or(false),
                    "cur_thread": s.and_then(|s| s.cur_thread),
                    "threads": s.map(|s| s.threads.len()).unwrap_or(0),
                    "breakpoints": s.map(|s| s.breakpoint_count()).unwrap_or(0),
                }), false);
            }
            "launch" => return (self.cmd_launch(req), false),
            "panic_triage" => return (self.cmd_panic_triage(req), false),
            _ => {}
        }

        // navigation works without a debug session
        if matches!(cmd, "where" | "def" | "refs" | "hover") {
            return (self.cmd_nav(cmd, req), false);
        }

        if self.session.is_none() {
            return (json!({"ok": false, "error": "no debug session (run `rdbg launch` first)"}), false);
        }

        let resp = self.cmd_session(cmd, req);
        (resp, false)
    }

    /// `continue --until '<path> <op> <value>'`: keep resuming past breakpoint
    /// stops until the condition holds, evaluated by rdbg itself at each stop
    /// (not by lldb — works where lldb conditional breakpoints don't bind).
    /// The stop summary carries an `until` object saying how the loop ended.
    fn cmd_continue_until(&mut self, req: &Value) -> Value {
        let cond = req["until"].as_str().unwrap_or("").to_string();
        let pred = match parse_predicate(&cond) {
            Ok(p) => p,
            Err(e) => return json!({"ok": false, "error": e}),
        };
        let max = req["max"].as_i64().unwrap_or(10_000).clamp(1, 1_000_000) as usize;
        if self.session.as_ref().unwrap().active_breakpoint_count() == 0 {
            return json!({"ok": false, "error":
                "continue --until needs at least one active breakpoint — rdbg re-checks the \
                 condition at each breakpoint stop (set one with `rdbg break <file.rs:line>`)"});
        }
        match self.session.as_mut().unwrap().cont_until(&pred, max) {
            Ok((stop, outcome, stops)) => {
                let (name, observed) = match outcome {
                    UntilOutcome::Held(obs) => ("held", Some(obs)),
                    UntilOutcome::Exited => ("exited", None),
                    UntilOutcome::Cap => ("cap", None),
                };
                let mut summary = self.stop_summary(stop);
                summary["until"] = json!({"outcome": name, "cond": pred.to_string(),
                                          "stops": stops, "observed": observed});
                json!({"ok": true, "stop": summary})
            }
            Err(e) => json!({"ok": false, "error": e}),
        }
    }

    fn cmd_launch(&mut self, req: &Value) -> Value {
        if let Some(mut old) = self.session.take() {
            old.disconnect();
        }
        let program = req["program"].as_str().unwrap_or("");
        let cwd = req["cwd"].as_str();
        let args: Vec<String> = req["args"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
        let mut session = match Session::new(program, cwd, args) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e}),
        };
        if let Some(bps) = req["breakpoints"].as_array() {
            for b in bps {
                session.add_line_bp(
                    b["file"].as_str().unwrap_or(""),
                    b["line"].as_i64().unwrap_or(0),
                    b["condition"].as_str().map(String::from),
                    b["hit"].as_i64(),
                    b["log"].as_str().map(String::from),
                );
            }
        }
        if let Some(fns) = req["fn_breaks"].as_array() {
            for n in fns {
                if let Some(n) = n.as_str() {
                    session.add_fn_bp(n);
                }
            }
        }
        if req["panic"].as_bool().unwrap_or(false) {
            session.add_panic_bp();
        }
        if let Err(e) = session.launch(false) {
            return json!({"ok": false, "error": e});
        }
        let stop = match session.run() {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e}),
        };
        self.session = Some(session);
        let summary = self.stop_summary(stop);
        json!({"ok": true, "stop": summary})
    }

    /// One-shot panic triage: launch with a panic breakpoint, run to the
    /// panic, and bundle the panic message, the first USER frame (std/core
    /// internals skipped) with its arguments and locals, and a short backtrace
    /// into a single response. If the program exits without panicking, says so
    /// (`panicked: false`). The session stays paused inside the panic (frames
    /// intact) unless the program ran to exit.
    fn cmd_panic_triage(&mut self, req: &Value) -> Value {
        if let Some(mut old) = self.session.take() {
            old.disconnect();
        }
        let program = req["program"].as_str().unwrap_or("");
        let cwd = req["cwd"].as_str();
        let args: Vec<String> = req["args"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
        let mut session = match Session::new(program, cwd, args) {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e}),
        };
        session.add_panic_bp();
        if let Err(e) = session.launch(false) {
            return json!({"ok": false, "error": e});
        }
        let stop = match session.run() {
            Ok(s) => s,
            Err(e) => return json!({"ok": false, "error": e}),
        };
        if stop.exited {
            // no panic — the only breakpoints were the panic symbols
            let out = session.output.concat();
            return json!({"ok": true, "panicked": false, "exit_code": stop.exit_code,
                          "output": tail(&out, 2000)});
        }
        // paused where the panic is raised — the first user frame, from the
        // frame list only (no variable reads yet, see below)
        let idx = first_user_frame(&stop.frames).unwrap_or(0);
        session.select_frame(idx);
        let frame = stop.frames.get(idx)
            .map(|f| format!("{}  {}:{}", f.name, f.file, f.line))
            .unwrap_or_else(|| "?".into());
        let source = session.source_around(4);
        let bt = session.backtrace_text();
        // Hunt the panic message next: the panic breakpoint fires *before* the
        // hook prints `panicked at ...`, so walk forward through the remaining
        // panic symbols (the user stack below stays intact) until the message
        // shows up — but never past rust_panic: continuing there lets the
        // program die. Deliberately before any locals read — lldb's Rust data
        // formatters can wedge the adapter on a moved-out String, and reading
        // locals last means that can only degrade the locals field.
        let at_last = |s: &Session| s.last_stop.as_ref().and_then(|st| st.top())
            .map(|f| f.name.ends_with("rust_panic")).unwrap_or(false);
        let mut exited = false;
        let mut exit_code = None;
        let mut message = extract_panic_message(&session.output.concat());
        for _ in 0..3 {
            if message.is_some() {
                break;
            }
            let last = at_last(&session);
            if session.wait_output_contains("panicked at",
                Duration::from_millis(if last { 2000 } else { 700 })) {
                message = extract_panic_message(&session.output.concat());
                break;
            }
            if last {
                break; // the hook already ran and printed nothing visible
            }
            match session.cont() {
                Ok(s) if s.exited => {
                    // panic=abort (no rust_panic symbol): the message flushes at exit
                    exited = true;
                    exit_code = s.exit_code;
                    session.wait_output_contains("panicked at", Duration::from_millis(700));
                    message = extract_panic_message(&session.output.concat());
                    break;
                }
                Ok(_) => message = extract_panic_message(&session.output.concat()),
                Err(_) => break,
            }
        }
        // locals (arguments included) at the user frame of the current stop —
        // the same physical frame, read last
        let vars = if exited {
            "  (program ran to exit during triage — locals no longer available)".to_string()
        } else {
            let i = session.last_stop.as_ref().map(|s| first_user_frame(&s.frames).unwrap_or(0)).unwrap_or(0);
            session.select_frame(i);
            session.locals_text(3)
        };
        let out = session.output.concat();
        if exited {
            session.disconnect();
        } else {
            self.session = Some(session); // paused inside the panic, user frame selected
        }
        json!({
            "ok": true, "panicked": true,
            "message": message,
            "frame": frame, "frame_index": idx,
            "source": source, "vars": vars, "bt": bt,
            "exited": exited, "exit_code": exit_code,
            "output": tail(&out, 1500),
        })
    }

    fn cmd_nav(&mut self, cmd: &str, req: &Value) -> Value {
        // lazily start rust-analyzer on first navigation — debug-only sessions skip it
        if self.lsp.is_none() {
            match Lsp::spawn(&self.ws) {
                Ok(lsp) => {
                    lsp.wait_ready(Duration::from_secs(180));
                    self.lsp = Some(lsp);
                }
                Err(e) => return json!({"ok": false, "error": format!("rust-analyzer unavailable: {e}")}),
            }
        }
        let lsp = self.lsp.as_ref().unwrap();
        match cmd {
            "where" => json!({"ok": true, "symbols": lsp.symbols(req["query"].as_str().unwrap_or(""))}),
            "def" | "refs" => {
                let (f, l, c) = (req["file"].as_str().unwrap_or(""), req["line"].as_i64().unwrap_or(0), req["col"].as_i64().unwrap_or(0));
                let locs = if cmd == "def" { lsp.definition(f, l, c) } else { lsp.references(f, l, c) };
                json!({"ok": true, "locations": locs})
            }
            "hover" => {
                let (f, l, c) = (req["file"].as_str().unwrap_or(""), req["line"].as_i64().unwrap_or(0), req["col"].as_i64().unwrap_or(0));
                json!({"ok": true, "hover": lsp.hover(f, l, c)})
            }
            _ => json!({"ok": false, "error": "unknown"}),
        }
    }

    fn cmd_session(&mut self, cmd: &str, req: &Value) -> Value {
        // commands that resume execution and yield a fresh stop
        match cmd {
            "continue" => return self.run_stop(|s| s.cont()),
            "continue_until" => return self.cmd_continue_until(req),
            "step" => {
                let kind = req["kind"].as_str().unwrap_or("over").to_string();
                return self.run_stop(move |s| match kind.as_str() {
                    "in" => s.step_in(),
                    "out" => s.step_out(),
                    "insn" => s.step_over(true),
                    _ => s.step_over(false),
                });
            }
            "until" => {
                let (f, l) = (req["file"].as_str().unwrap_or("").to_string(), req["line"].as_i64().unwrap_or(0));
                return self.run_stop(move |s| s.until(&f, l));
            }
            "pause" => return self.run_stop(|s| s.pause()),
            "restart" => return self.run_stop(|s| s.restart()),
            "trace" => {
                let captures: Vec<String> = req["captures"].as_array()
                    .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                let max = req["max"].as_i64().unwrap_or(50).max(1) as usize;
                let hits = self.session.as_mut().unwrap().trace(&captures, max);
                let out: String = self.session.as_ref().map(|s| s.output.concat()).unwrap_or_default();
                let text = format_trace(&hits);
                let n = hits.len();
                return json!({"ok": true, "trace": text, "hits": n,
                              "output": if out.is_empty() { Value::Null } else { json!(out[out.len().saturating_sub(1000)..]) }});
            }
            "thread" => {
                let id = req["id"].as_i64().unwrap_or(0);
                let ok = self.session.as_mut().unwrap().select_thread(id);
                let s = self.session.as_ref().unwrap();
                let stop = if ok { s.last_stop.as_ref().map(|st| summarize(s, st)) } else { None };
                return json!({"ok": ok, "stop": stop});
            }
            "state" => {
                let s = self.session.as_ref().unwrap();
                let stop = s.last_stop.as_ref().map(|st| summarize(s, st));
                return json!({"ok": true, "stop": stop, "vars": s.locals_text(3),
                    "watches": if s.watches.is_empty() { String::new() } else { s.watches_text() }});
            }
            "stop" => {
                if let Some(mut sess) = self.session.take() {
                    sess.disconnect();
                }
                return json!({"ok": true, "stopped_session": true});
            }
            "frame" => {
                let ok = {
                    let s = self.session.as_mut().unwrap();
                    match req["dir"].as_str() {
                        Some("up") => s.frame_shift(true),
                        Some("down") => s.frame_shift(false),
                        _ => s.select_frame(req["index"].as_i64().unwrap_or(0).max(0) as usize),
                    }
                };
                let s = self.session.as_ref().unwrap();
                return json!({"ok": ok, "source": if ok { s.source_around(6) } else { String::new() },
                              "vars": if ok { s.locals_text(3) } else { String::new() }});
            }
            _ => {}
        }
        // stateful, non-resuming commands
        let s = self.session.as_mut().unwrap();
        match cmd {
            "bp_add" => {
                let (id, verified) = s.add_line_bp(
                    req["file"].as_str().unwrap_or(""), req["line"].as_i64().unwrap_or(0),
                    req["condition"].as_str().map(String::from), req["hit"].as_i64(), req["log"].as_str().map(String::from));
                json!({"ok": true, "id": id, "verified": verified})
            }
            "bp_fn" => json!({"ok": true, "id": s.add_fn_bp(req["name"].as_str().unwrap_or(""))}),
            "bp_panic" => { s.add_panic_bp(); json!({"ok": true, "id": "panic"}) }
            "bp_watch" => match s.add_watchpoint(req["var"].as_str().unwrap_or("")) {
                Ok(id) => json!({"ok": true, "id": id}),
                Err(e) => json!({"ok": false, "error": e}),
            },
            "bp_list" => json!({"ok": true, "breakpoints": s.breakpoints_text()}),
            "bp_rm" => {
                let ok = s.remove_bp(req["id"].as_str().unwrap_or(""));
                json!({"ok": ok, "error": if ok { Value::Null } else { json!("breakpoint not found") }})
            }
            "bp_enable" => json!({"ok": s.set_enabled(req["id"].as_str().unwrap_or(""), req["enabled"].as_bool().unwrap_or(true))}),
            "threads" => json!({"ok": true, "threads": s.threads_text()}),
            "vars" => {
                let full = req["full"].as_bool().unwrap_or(false);
                let depth = req["depth"].as_i64().unwrap_or(if full { 10 } else { 3 }) as i32;
                let cap = if full { 64 } else { 12 };
                json!({"ok": true, "vars": s.locals_text_capped(depth, cap)})
            }
            "eval" => json!({"ok": true, "value": s.evaluate(req["expr"].as_str().unwrap_or(""))}),
            "set" => json!({"ok": true, "value": s.set_variable(req["path"].as_str().unwrap_or(""), req["value"].as_str().unwrap_or(""))}),
            "watch_expr" => {
                s.watch_expr(req["action"].as_str().unwrap_or("list"), req["expr"].as_str().map(String::from));
                json!({"ok": true, "watches": s.watches_text()})
            }
            "bt" => json!({"ok": true, "bt": s.backtrace_text()}),
            "list" => json!({"ok": true, "source": s.source_around(req["radius"].as_i64().unwrap_or(6))}),
            _ => json!({"ok": false, "error": format!("unknown command {cmd:?}")}),
        }
    }
}
