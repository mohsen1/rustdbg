//! A stateful, Rust-aware debug session over one launched `lldb-dap` process.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::time::Duration;

use serde_json::{json, Value};

use crate::dap::Dap;
use crate::render::{is_leaf_type, is_leaf_value, short_fn, short_type};
use crate::util::{abs, find_lldb_dap, rust_formatter_commands};

/// Function symbols where a Rust panic is raised (lldb-dap has no Rust
/// exception filter, so a function breakpoint on these breaks on panic).
const PANIC_SYMBOLS: &[&str] = &["rust_panic", "core::panicking::panic_fmt", "core::panicking::panic"];

const T: Duration = Duration::from_secs(120);

#[derive(Clone)]
pub struct LineBp {
    pub id: i64,
    pub line: i64,
    pub condition: Option<String>,
    pub hit: Option<i64>,
    pub log: Option<String>,
    pub enabled: bool,
    pub verified: bool,
}

#[derive(Clone)]
pub struct FnBp {
    pub id: i64,
    pub name: String,
    pub enabled: bool,
}

#[derive(Clone)]
pub struct DataBp {
    pub id: i64,
    pub name: String,
    pub data_id: String,
    pub enabled: bool,
}

#[derive(Clone)]
pub struct Frame {
    pub id: i64,
    pub name: String,
    pub file: String,
    pub line: i64,
    pub path: String,
}

#[derive(Clone)]
pub struct Stop {
    pub reason: String,
    pub thread_id: i64,
    pub frames: Vec<Frame>,
    pub exited: bool,
    pub exit_code: Option<i64>,
}

impl Stop {
    fn exited(code: Option<i64>) -> Stop {
        Stop { reason: "exited".into(), thread_id: 0, frames: vec![], exited: true, exit_code: code }
    }
    pub fn top(&self) -> Option<&Frame> {
        self.frames.first()
    }
}

/// One breakpoint hit captured during a `trace` run.
pub struct TraceHit {
    pub func: String,
    pub loc: String,
    pub values: Vec<(String, String)>,
}

pub struct Session {
    pub program: String,
    pub cwd: String,
    pub args: Vec<String>,
    adapter: String,
    dap: Dap,
    bp_id: i64,
    pub line_bps: HashMap<String, Vec<LineBp>>,
    pub fn_bps: Vec<FnBp>,
    pub data_bps: Vec<DataBp>,
    pub panic: bool,
    temp: HashMap<String, BTreeSet<i64>>,
    pub threads: Vec<Value>,
    pub cur_thread: Option<i64>,
    pub cur_frame: usize,
    pub watches: Vec<String>,
    pub output: Vec<String>,
    pub last_stop: Option<Stop>,
    /// Top-frame locals (name -> "type = value") snapshot from the previous stop,
    /// used to render only what changed at the next stop.
    prev_locals: HashMap<String, String>,
    configured: bool,
}

impl Session {
    pub fn new(program: &str, cwd: Option<&str>, args: Vec<String>) -> Result<Session, String> {
        let adapter = find_lldb_dap()
            .ok_or("no lldb-dap adapter found (install LLVM/lldb, or Xcode command line tools)")?;
        let dap = Dap::spawn(&adapter).map_err(|e| format!("failed to launch {adapter}: {e}"))?;
        let program = abs(program);
        let cwd = cwd.map(abs).unwrap_or_else(|| {
            Path::new(&program).parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default()
        });
        Ok(Session {
            program,
            cwd,
            args,
            adapter,
            dap,
            bp_id: 0,
            line_bps: HashMap::new(),
            fn_bps: vec![],
            data_bps: vec![],
            panic: false,
            temp: HashMap::new(),
            threads: vec![],
            cur_thread: None,
            cur_frame: 0,
            watches: vec![],
            output: vec![],
            last_stop: None,
            prev_locals: HashMap::new(),
            configured: false,
        })
    }

    // -- launch ---------------------------------------------------------------

    pub fn launch(&mut self, stop_on_entry: bool) -> Result<(), String> {
        self.dap.request("initialize", json!({
            "adapterID": "lldb-dap", "clientID": "rdbg", "linesStartAt1": true,
            "columnsStartAt1": true, "pathFormat": "path", "supportsVariableType": true,
            "supportsRunInTerminalRequest": false,
        }), T)?;
        let seq = self.dap.send("launch", json!({
            "program": self.program, "cwd": self.cwd, "args": self.args,
            "stopOnEntry": stop_on_entry, "initCommands": rust_formatter_commands(),
        }));
        self.dap.wait_event("initialized", T).ok_or("adapter never sent `initialized`")?;
        let paths: Vec<String> = self.line_bps.keys().cloned().collect();
        for p in paths {
            self.sync_line(&p);
        }
        self.sync_fn();
        self.dap.request("configurationDone", Value::Null, T)?;
        self.configured = true;
        self.dap.reply(seq, T).ok_or("launch did not complete")?;
        Ok(())
    }

    pub fn restart(&mut self) -> Result<Stop, String> {
        let _ = self.dap.request_soft("disconnect", json!({"terminateDebuggee": true}), Duration::from_secs(5));
        self.dap.close();
        self.dap = Dap::spawn(&self.adapter).map_err(|e| format!("relaunch failed: {e}"))?;
        self.configured = false;
        self.data_bps.clear(); // watchpoint dataIds are bound to the old session
        self.threads.clear();
        self.cur_thread = None;
        self.cur_frame = 0;
        self.prev_locals.clear(); // fresh program — nothing to diff against yet
        self.launch(false)?;
        self.run()
    }

    // -- breakpoint model -----------------------------------------------------

    fn next_id(&mut self) -> i64 {
        self.bp_id += 1;
        self.bp_id
    }

    fn sync_line(&self, path: &str) -> Vec<bool> {
        let empty = vec![];
        let bps = self.line_bps.get(path).unwrap_or(&empty);
        let mut wire: Vec<Value> = bps
            .iter()
            .filter(|b| b.enabled)
            .map(|b| {
                let mut m = json!({"line": b.line});
                if let Some(c) = &b.condition {
                    m["condition"] = json!(c);
                }
                if let Some(h) = b.hit {
                    m["hitCondition"] = json!(h.to_string());
                }
                if let Some(l) = &b.log {
                    m["logMessage"] = json!(l);
                }
                m
            })
            .collect();
        if let Some(temps) = self.temp.get(path) {
            for line in temps {
                wire.push(json!({"line": line}));
            }
        }
        let resp = self.dap.request_soft("setBreakpoints", json!({"source": {"path": path}, "breakpoints": wire}), T);
        resp["body"]["breakpoints"].as_array().map(|a| {
            a.iter().map(|b| b["verified"].as_bool().unwrap_or(false)).collect()
        }).unwrap_or_default()
    }

    fn sync_fn(&self) {
        let mut names: Vec<String> = self.fn_bps.iter().filter(|b| b.enabled).map(|b| b.name.clone()).collect();
        if self.panic {
            names.extend(PANIC_SYMBOLS.iter().map(|s| s.to_string()));
        }
        let wire: Vec<Value> = names.iter().map(|n| json!({"name": n})).collect();
        let _ = self.dap.request_soft("setFunctionBreakpoints", json!({"breakpoints": wire}), T);
    }

    fn sync_data(&self) {
        let wire: Vec<Value> = self.data_bps.iter().filter(|b| b.enabled).map(|b| json!({"dataId": b.data_id})).collect();
        let _ = self.dap.request_soft("setDataBreakpoints", json!({"breakpoints": wire}), T);
    }

    pub fn add_line_bp(&mut self, path: &str, line: i64, condition: Option<String>, hit: Option<i64>, log: Option<String>) -> (i64, bool) {
        let ap = abs(path);
        let id = self.next_id();
        self.line_bps.entry(ap.clone()).or_default().push(LineBp {
            id, line, condition, hit, log, enabled: true, verified: true,
        });
        let mut verified = true;
        if self.configured {
            let flags = self.sync_line(&ap);
            let active: Vec<usize> = self.line_bps[&ap].iter().enumerate().filter(|(_, b)| b.enabled).map(|(i, _)| i).collect();
            if let Some(pos) = active.iter().position(|&i| self.line_bps[&ap][i].id == id) {
                verified = flags.get(pos).copied().unwrap_or(true);
            }
            if let Some(b) = self.line_bps.get_mut(&ap).and_then(|v| v.iter_mut().find(|b| b.id == id)) {
                b.verified = verified;
            }
        }
        (id, verified)
    }

    pub fn add_fn_bp(&mut self, name: &str) -> i64 {
        let id = self.next_id();
        self.fn_bps.push(FnBp { id, name: name.to_string(), enabled: true });
        if self.configured {
            self.sync_fn();
        }
        id
    }

    pub fn add_panic_bp(&mut self) {
        self.panic = true;
        if self.configured {
            self.sync_fn();
        }
    }

    pub fn add_watchpoint(&mut self, var: &str) -> Result<i64, String> {
        let (rref, name) = self.resolve_var_ref(var).ok_or_else(|| format!("variable {var:?} not found in the current frame"))?;
        let info = self.dap.request_soft("dataBreakpointInfo", json!({"variablesReference": rref, "name": name}), T);
        let data_id = info["body"]["dataId"].as_str().map(|s| s.to_string());
        let Some(data_id) = data_id else {
            let why = info["body"]["description"].as_str().unwrap_or("unsupported");
            return Err(format!("cannot watch {var:?}: {why}"));
        };
        let id = self.next_id();
        self.data_bps.push(DataBp { id, name: var.to_string(), data_id, enabled: true });
        self.sync_data();
        Ok(id)
    }

    pub fn remove_bp(&mut self, id: &str) -> bool {
        if id == "panic" {
            self.panic = false;
            self.sync_fn();
            return true;
        }
        let Ok(num) = id.parse::<i64>() else { return false };
        for (path, v) in self.line_bps.iter_mut() {
            let before = v.len();
            v.retain(|b| b.id != num);
            if v.len() != before {
                let p = path.clone();
                self.sync_line(&p);
                return true;
            }
        }
        let before = self.fn_bps.len();
        self.fn_bps.retain(|b| b.id != num);
        if self.fn_bps.len() != before {
            self.sync_fn();
            return true;
        }
        let before = self.data_bps.len();
        self.data_bps.retain(|b| b.id != num);
        if self.data_bps.len() != before {
            self.sync_data();
            return true;
        }
        false
    }

    pub fn set_enabled(&mut self, id: &str, enabled: bool) -> bool {
        if id == "panic" {
            self.panic = enabled;
            self.sync_fn();
            return true;
        }
        let Ok(num) = id.parse::<i64>() else { return false };
        let mut which: Option<String> = None;
        for (path, v) in self.line_bps.iter_mut() {
            if let Some(b) = v.iter_mut().find(|b| b.id == num) {
                b.enabled = enabled;
                which = Some(path.clone());
            }
        }
        if let Some(p) = which {
            self.sync_line(&p);
            return true;
        }
        if let Some(b) = self.fn_bps.iter_mut().find(|b| b.id == num) {
            b.enabled = enabled;
            self.sync_fn();
            return true;
        }
        if let Some(b) = self.data_bps.iter_mut().find(|b| b.id == num) {
            b.enabled = enabled;
            self.sync_data();
            return true;
        }
        false
    }

    pub fn breakpoints_text(&self) -> String {
        let mut lines = vec![];
        let mut all: Vec<(i64, String)> = vec![];
        for (path, v) in self.line_bps.iter() {
            let file = Path::new(path).file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_else(|| path.clone());
            for b in v {
                let extra = [
                    b.condition.as_ref().map(|c| format!(" if {c}")),
                    b.hit.map(|h| format!(" hit={h}")),
                    b.log.as_ref().map(|l| format!(" log={l}")),
                ]
                .into_iter()
                .flatten()
                .collect::<String>();
                let dis = if b.enabled { "" } else { " [disabled]" };
                all.push((b.id, format!("  [{}] {}:{}{}{}", b.id, file, b.line, extra, dis)));
            }
        }
        for b in &self.fn_bps {
            let dis = if b.enabled { "" } else { " [disabled]" };
            all.push((b.id, format!("  [{}] fn {}{}", b.id, b.name, dis)));
        }
        for b in &self.data_bps {
            let dis = if b.enabled { "" } else { " [disabled]" };
            all.push((b.id, format!("  [{}] watch {}{}", b.id, b.name, dis)));
        }
        all.sort_by_key(|(id, _)| *id);
        lines.extend(all.into_iter().map(|(_, s)| s));
        if self.panic {
            lines.push("  [panic] rust panic".to_string());
        }
        if lines.is_empty() {
            "  (no breakpoints)".to_string()
        } else {
            lines.join("\n")
        }
    }

    pub fn breakpoint_count(&self) -> usize {
        self.line_bps.values().map(|v| v.len()).sum::<usize>() + self.fn_bps.len() + self.data_bps.len() + self.panic as usize
    }

    // -- run control ----------------------------------------------------------

    fn flush(&mut self) {
        while let Some(ev) = self.dap.poll_event(Duration::from_millis(0)) {
            if ev["event"] == "output" {
                if let Some(o) = ev["body"]["output"].as_str() {
                    self.output.push(o.to_string());
                }
            }
        }
    }

    fn await_stop(&mut self, timeout: Duration) -> Result<Stop, String> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.checked_duration_since(std::time::Instant::now())
                .ok_or("no stop/exit event (program may still be running — try `pause`)")?;
            let Some(ev) = self.dap.poll_event(remaining) else { continue };
            match ev["event"].as_str() {
                Some("output") => {
                    if let Some(o) = ev["body"]["output"].as_str() {
                        self.output.push(o.to_string());
                    }
                }
                Some("exited") => {
                    let stop = Stop::exited(ev["body"]["exitCode"].as_i64());
                    self.last_stop = Some(stop.clone());
                    return Ok(stop);
                }
                Some("terminated") => {
                    let stop = Stop::exited(None);
                    self.last_stop = Some(stop.clone());
                    return Ok(stop);
                }
                Some("stopped") => {
                    if let Some(tid) = ev["body"]["threadId"].as_i64() {
                        self.cur_thread = Some(tid);
                    }
                    self.cur_frame = 0;
                    self.refresh_threads();
                    let reason = ev["body"]["reason"].as_str().unwrap_or("stopped").to_string();
                    let stop = self.build_stop(&reason);
                    self.last_stop = Some(stop.clone());
                    return Ok(stop);
                }
                _ => {}
            }
        }
    }

    fn refresh_threads(&mut self) {
        self.threads = self.dap.request_soft("threads", Value::Null, Duration::from_secs(10))["body"]["threads"]
            .as_array().cloned().unwrap_or_default();
    }

    fn frames(&self, thread_id: i64) -> Vec<Frame> {
        let resp = self.dap.request_soft("stackTrace", json!({"threadId": thread_id, "levels": 40}), Duration::from_secs(15));
        resp["body"]["stackFrames"].as_array().map(|a| {
            a.iter().map(|f| Frame {
                id: f["id"].as_i64().unwrap_or(0),
                name: short_fn(f["name"].as_str().unwrap_or("")),
                file: f["source"]["name"].as_str().unwrap_or("?").to_string(),
                line: f["line"].as_i64().unwrap_or(0),
                path: f["source"]["path"].as_str().unwrap_or("").to_string(),
            }).collect()
        }).unwrap_or_default()
    }

    fn build_stop(&self, reason: &str) -> Stop {
        let tid = self.cur_thread.unwrap_or(0);
        Stop { reason: reason.into(), thread_id: tid, frames: self.frames(tid), exited: false, exit_code: None }
    }

    pub fn run(&mut self) -> Result<Stop, String> {
        self.await_stop(T)
    }

    fn resume(&mut self, command: &str, extra: Value) -> Result<Stop, String> {
        let tid = self.cur_thread.ok_or("not stopped")?;
        self.flush();
        let mut args = json!({"threadId": tid});
        if let Value::Object(m) = extra {
            for (k, v) in m {
                args[k] = v;
            }
        }
        self.dap.request(command, args, T)?;
        self.await_stop(T)
    }

    pub fn cont(&mut self) -> Result<Stop, String> {
        self.resume("continue", Value::Null)
    }
    pub fn step_over(&mut self, insn: bool) -> Result<Stop, String> {
        self.resume("next", if insn { json!({"granularity": "instruction"}) } else { Value::Null })
    }
    pub fn step_in(&mut self) -> Result<Stop, String> {
        self.resume("stepIn", Value::Null)
    }
    pub fn step_out(&mut self) -> Result<Stop, String> {
        self.resume("stepOut", Value::Null)
    }

    pub fn until(&mut self, path: &str, line: i64) -> Result<Stop, String> {
        let ap = abs(path);
        self.temp.entry(ap.clone()).or_default().insert(line);
        self.sync_line(&ap);
        let result = self.cont();
        if let Some(t) = self.temp.get_mut(&ap) {
            t.remove(&line);
        }
        self.sync_line(&ap);
        result
    }

    /// Run through breakpoint hits without yielding: at each stop capture the
    /// given variable paths (or brief locals), auto-continue, and collect a
    /// compact trace. Starts from the current stop. One call replaces N
    /// break/inspect/continue round-trips.
    pub fn trace(&mut self, captures: &[String], max_hits: usize) -> Vec<TraceHit> {
        let mut hits = vec![];
        while hits.len() < max_hits {
            let Some(stop) = self.last_stop.clone() else { break };
            if stop.exited {
                break;
            }
            let (func, loc) = match stop.top() {
                Some(f) => (f.name.clone(), format!("{}:{}", f.file, f.line)),
                None => (String::new(), String::new()),
            };
            let values = if captures.is_empty() {
                vec![("locals".to_string(), self.locals_text(1).trim().replace('\n', "; "))]
            } else {
                captures.iter().map(|c| (c.clone(), self.evaluate(c))).collect()
            };
            hits.push(TraceHit { func, loc, values });
            match self.cont() {
                Ok(s) if s.exited => break,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        hits
    }

    pub fn pause(&mut self) -> Result<Stop, String> {
        self.flush();
        let tid = self.cur_thread.or_else(|| self.threads.first().and_then(|t| t["id"].as_i64())).unwrap_or(1);
        let _ = self.dap.request_soft("pause", json!({"threadId": tid}), Duration::from_secs(5));
        self.await_stop(Duration::from_secs(30))
    }

    // -- threads / frames -----------------------------------------------------

    pub fn select_thread(&mut self, thread_id: i64) -> bool {
        if self.threads.iter().any(|t| t["id"].as_i64() == Some(thread_id)) {
            self.cur_thread = Some(thread_id);
            self.cur_frame = 0;
            let stop = self.build_stop("switch");
            self.last_stop = Some(stop);
            true
        } else {
            false
        }
    }

    pub fn threads_text(&self) -> String {
        if self.threads.is_empty() {
            return "(no threads)".to_string();
        }
        self.threads.iter().map(|t| {
            let id = t["id"].as_i64().unwrap_or(0);
            let name = t["name"].as_str().unwrap_or("");
            let frames = self.frames(id);
            let where_ = frames.first().map(|f| format!("{} {}:{}", f.name, f.file, f.line)).unwrap_or_else(|| "?".into());
            let mark = if Some(id) == self.cur_thread { "*" } else { " " };
            format!(" {mark} thread {id} [{name}]  {where_}")
        }).collect::<Vec<_>>().join("\n")
    }

    pub fn select_frame(&mut self, index: usize) -> bool {
        if let Some(s) = &self.last_stop {
            if index < s.frames.len() {
                self.cur_frame = index;
                return true;
            }
        }
        false
    }

    pub fn frame_shift(&mut self, up: bool) -> bool {
        let idx = if up { self.cur_frame + 1 } else { self.cur_frame.saturating_sub(1) };
        self.select_frame(idx)
    }

    fn frame(&self) -> Option<Frame> {
        let s = self.last_stop.as_ref()?;
        let i = self.cur_frame.min(s.frames.len().saturating_sub(1));
        s.frames.get(i).cloned()
    }

    // -- inspection / mutation ------------------------------------------------

    fn locals_ref(&self) -> i64 {
        let Some(f) = self.frame() else { return 0 };
        let scopes = self.dap.request_soft("scopes", json!({"frameId": f.id}), Duration::from_secs(10));
        scopes["body"]["scopes"].as_array().and_then(|a| {
            a.iter().find(|s| s["name"].as_str().unwrap_or("").to_lowercase().starts_with("local"))
                .and_then(|s| s["variablesReference"].as_i64())
        }).unwrap_or(0)
    }

    fn resolve_var_ref(&self, path: &str) -> Option<(i64, String)> {
        let parts = tokenize_path(path);
        if parts.is_empty() {
            return None;
        }
        let leaf = parts.last().unwrap().clone();
        let mut rref = self.locals_ref();
        for seg in &parts[..parts.len() - 1] {
            let vars = self.dap.request_soft("variables", json!({"variablesReference": rref}), Duration::from_secs(10));
            let found = vars["body"]["variables"].as_array().and_then(|a| {
                a.iter().find(|v| v["name"].as_str() == Some(seg.as_str()))
                    .and_then(|v| v["variablesReference"].as_i64()).filter(|r| *r != 0)
            })?;
            rref = found;
        }
        Some((rref, leaf))
    }

    pub fn locals_text(&self, depth: i32) -> String {
        self.locals_text_capped(depth, 12)
    }

    /// Like `locals_text`, but with an explicit per-level child cap (raised by
    /// `vars --full` for a complete dump).
    pub fn locals_text_capped(&self, depth: i32, cap: usize) -> String {
        let rref = self.locals_ref();
        let mut out = vec![];
        self.render(rref, depth, cap, "  ", &mut out);
        if out.is_empty() {
            "  (no locals)".to_string()
        } else {
            out.join("\n")
        }
    }

    /// Snapshot the top frame's depth-1 locals (name -> "type = value"), diff
    /// against the previous stop's snapshot, and return only what changed:
    /// `~ name` for a changed value (with the old one), `+ name` for a new local,
    /// and a `(+N unchanged)` tail. Updates the snapshot for the next stop.
    pub fn locals_delta(&mut self) -> String {
        let rref = self.locals_ref();
        let mut cur: Vec<(String, String)> = vec![];
        if rref != 0 {
            let resp = self.dap.request_soft("variables", json!({"variablesReference": rref}), Duration::from_secs(10));
            if let Some(vars) = resp["body"]["variables"].as_array() {
                for v in vars.iter().take(24) {
                    let name = v["name"].as_str().unwrap_or("?").to_string();
                    let val = v["value"].as_str().unwrap_or("");
                    let typ = short_type(v["type"].as_str().unwrap_or(""));
                    let child = v["variablesReference"].as_i64().unwrap_or(0);
                    let leaf = is_leaf_value(val) || is_leaf_type(&typ) || child == 0;
                    let shown = if leaf || is_leaf_value(val) { val } else { "" };
                    let repr = if shown.is_empty() { typ } else { format!("{typ} = {shown}") };
                    cur.push((name, repr));
                }
            }
        }
        let mut lines = vec![];
        let mut unchanged = 0;
        for (name, repr) in &cur {
            match self.prev_locals.get(name) {
                Some(old) if old == repr => unchanged += 1,
                Some(old) => lines.push(format!("  ~ {name}: {repr} (was {})", value_part(old))),
                None => lines.push(format!("  + {name}: {repr}")),
            }
        }
        self.prev_locals = cur.into_iter().collect();
        if lines.is_empty() {
            if unchanged == 0 {
                "  (no locals)".to_string()
            } else {
                format!("  (no change; {unchanged} unchanged)")
            }
        } else {
            if unchanged > 0 {
                lines.push(format!("  (+{unchanged} unchanged)"));
            }
            lines.join("\n")
        }
    }

    fn render(&self, rref: i64, depth: i32, cap: usize, indent: &str, out: &mut Vec<String>) {
        if rref == 0 || depth <= 0 {
            return;
        }
        let resp = self.dap.request_soft("variables", json!({"variablesReference": rref}), Duration::from_secs(10));
        let Some(vars) = resp["body"]["variables"].as_array() else { return };
        for v in vars.iter().take(cap) {
            let name = v["name"].as_str().unwrap_or("?");
            let val = v["value"].as_str().unwrap_or("");
            let typ = short_type(v["type"].as_str().unwrap_or(""));
            let child = v["variablesReference"].as_i64().unwrap_or(0);
            let leaf = is_leaf_value(val) || is_leaf_type(&typ) || child == 0;
            let shown = if leaf || is_leaf_value(val) { val } else { "" };
            if shown.is_empty() {
                out.push(format!("{indent}{name}: {typ}"));
            } else {
                out.push(format!("{indent}{name}: {typ} = {shown}"));
            }
            if !leaf && depth > 1 {
                self.render(child, depth - 1, cap, &format!("{indent}  "), out);
            }
        }
        if vars.len() > cap {
            out.push(format!("{indent}... ({} more)", vars.len() - cap));
        }
    }

    pub fn evaluate(&self, expr: &str) -> String {
        // Prefer the variables tree: it auto-derefs `&references` and renders Rust
        // aggregates, where lldb's expression evaluator rejects `.` on a pointer
        // (`it.qty` on a `&Item`). Fall back to the evaluator for anything the
        // tree walk can't resolve.
        if let Some((rref, leaf)) = self.resolve_var_ref(expr) {
            let resp = self.dap.request_soft("variables", json!({"variablesReference": rref}), Duration::from_secs(10));
            if let Some(vars) = resp["body"]["variables"].as_array() {
                if let Some(v) = vars.iter().find(|v| v["name"].as_str() == Some(leaf.as_str())) {
                    let typ = short_type(v["type"].as_str().unwrap_or(""));
                    let val = v["value"].as_str().unwrap_or("");
                    if !val.is_empty() {
                        return format!("{typ} = {val}").trim_matches(|c| c == ' ' || c == '=').to_string();
                    }
                    if !typ.is_empty() {
                        return typ;
                    }
                }
            }
        }
        let Some(f) = self.frame() else { return "(not stopped)".into() };
        let resp = self.dap.request_soft("evaluate", json!({"expression": expr, "frameId": f.id, "context": "hover"}), Duration::from_secs(15));
        if !resp["success"].as_bool().unwrap_or(false) {
            let m = resp["message"].as_str().unwrap_or("error");
            return format!("(cannot evaluate {expr:?}: {m})");
        }
        let typ = short_type(resp["body"]["type"].as_str().unwrap_or(""));
        let result = resp["body"]["result"].as_str().unwrap_or("");
        format!("{typ} = {result}").trim_matches(|c| c == ' ' || c == '=').to_string()
    }

    pub fn set_variable(&self, path: &str, value: &str) -> String {
        let Some((rref, name)) = self.resolve_var_ref(path) else {
            return format!("(variable {path:?} not found)");
        };
        let resp = self.dap.request_soft("setVariable", json!({"variablesReference": rref, "name": name, "value": value}), Duration::from_secs(10));
        if !resp["success"].as_bool().unwrap_or(false) {
            let m = resp["message"].as_str().unwrap_or("error");
            return format!("(cannot set {path:?}: {m})");
        }
        format!("{path} = {}", resp["body"]["value"].as_str().unwrap_or(""))
    }

    pub fn watches_text(&self) -> String {
        if self.watches.is_empty() {
            return "  (no watch expressions)".to_string();
        }
        self.watches.iter().map(|e| format!("  {e}: {}", self.evaluate(e))).collect::<Vec<_>>().join("\n")
    }

    pub fn backtrace_text(&self) -> String {
        let Some(s) = &self.last_stop else { return "(not stopped)".into() };
        let mut out = vec![];
        for (i, f) in s.frames.iter().take(20).enumerate() {
            let mark = if i == self.cur_frame { ">" } else { " " };
            out.push(format!(" {mark}#{i} {}  {}:{}", f.name, f.file, f.line));
            if f.name.ends_with("::main") || f.name == "main" {
                break;
            }
        }
        out.join("\n")
    }

    pub fn source_around(&self, radius: i64) -> String {
        let Some(f) = self.frame() else { return "(not stopped)".into() };
        if f.path.is_empty() || !Path::new(&f.path).exists() {
            return format!("  ({}:{} — source not available)", f.file, f.line);
        }
        let Ok(text) = std::fs::read_to_string(&f.path) else {
            return format!("  ({}:{})", f.file, f.line);
        };
        let lines: Vec<&str> = text.lines().collect();
        let lo = (f.line - radius - 1).max(0) as usize;
        let hi = ((f.line + radius) as usize).min(lines.len());
        let mut out = vec![format!("  {}:{}  (frame #{} {})", f.file, f.line, self.cur_frame, f.name)];
        for i in lo..hi {
            let mark = if (i as i64) + 1 == f.line { "->" } else { "  " };
            out.push(format!("  {mark} {:>5} | {}", i + 1, lines[i]));
        }
        out.join("\n")
    }

    pub fn watch_expr(&mut self, action: &str, expr: Option<String>) {
        match (action, expr) {
            ("add", Some(e)) => self.watches.push(e),
            ("rm", Some(e)) => self.watches.retain(|w| *w != e),
            _ => {}
        }
    }

    pub fn disconnect(&mut self) {
        let _ = self.dap.request_soft("disconnect", json!({"terminateDebuggee": true}), Duration::from_secs(5));
        self.dap.close();
    }
}

/// The value side of a `type = value` render (for the `(was …)` note); returns
/// the whole string if there is no `=`.
fn value_part(repr: &str) -> &str {
    repr.rsplit_once(" = ").map(|(_, v)| v).unwrap_or(repr)
}

/// Split `items[0].qty` into `["items", "[0]", "qty"]` — Vec/array children are
/// named `[0]` in the DAP variables tree, so set/watch work on indexed paths.
fn tokenize_path(path: &str) -> Vec<String> {
    let mut out = vec![];
    let b = path.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'.' => i += 1,
            b'[' => {
                let start = i;
                while i < b.len() && b[i] != b']' {
                    i += 1;
                }
                if i < b.len() {
                    i += 1; // include ']'
                }
                out.push(path[start..i].to_string());
            }
            _ => {
                let start = i;
                while i < b.len() && b[i] != b'.' && b[i] != b'[' {
                    i += 1;
                }
                out.push(path[start..i].to_string());
            }
        }
    }
    out
}
