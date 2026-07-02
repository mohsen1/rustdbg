//! MCP server: exposes the debugger to MCP clients (Claude Code, Codex) over
//! newline-delimited JSON-RPC on stdio. Each tool call ensures the per-project
//! daemon is up and forwards to it — the same daemon the CLI drives.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

use crate::client::{cargo_build, ensure_daemon, request, ws_root};

const PROTOCOL: &str = "2024-11-05";

fn tools() -> Vec<Value> {
    let obj = |props: Value| json!({"type": "object", "properties": props});
    vec![
        json!({"name": "debug_launch", "description":
            "Build (or take) a Rust debug binary and run it to the first breakpoint. Provide `cargo` (project dir, with optional `bin`/`test` target) or `bin_path`. `breakpoints` are 'file.rs:line' strings; `panic:true` also breaks where any panic is raised.",
            "inputSchema": obj(json!({"cargo": {"type": "string"}, "bin": {"type": "string"}, "test": {"type": "string"},
                "bin_path": {"type": "string"}, "breakpoints": {"type": "array", "items": {"type": "string"}},
                "fn_breaks": {"type": "array", "items": {"type": "string"}}, "args": {"type": "array", "items": {"type": "string"}},
                "panic": {"type": "boolean"}}))}),
        json!({"name": "debug_add_breakpoint", "description":
            "Add a breakpoint while paused or before running. `file`+`line`, or `fn` for a function breakpoint, `panic:true` for a Rust panic breakpoint, or `watch` to break when a local changes. Line breakpoints accept `condition`, `hit`, `log`.",
            "inputSchema": obj(json!({"file": {"type": "string"}, "line": {"type": "integer"}, "fn": {"type": "string"},
                "panic": {"type": "boolean"}, "watch": {"type": "string"}, "condition": {"type": "string"},
                "hit": {"type": "integer"}, "log": {"type": "string"}}))}),
        json!({"name": "debug_trace", "description":
            "Run through breakpoint hits without stopping and return a compact table — one call instead of break/inspect/continue for every hit. Provide `cargo`(+`bin`/`test`) or `bin_path`, `breakpoints` ('file.rs:line'), and `capture` (variable paths evaluated at each hit; brief locals if omitted). `max` caps the hit count.",
            "inputSchema": obj(json!({"cargo": {"type": "string"}, "bin": {"type": "string"}, "test": {"type": "string"},
                "bin_path": {"type": "string"}, "breakpoints": {"type": "array", "items": {"type": "string"}},
                "capture": {"type": "array", "items": {"type": "string"}}, "max": {"type": "integer"},
                "args": {"type": "array", "items": {"type": "string"}}}))}),
        json!({"name": "debug_do", "description":
            "Run several rdbg subcommands in one call, separated by ';' (e.g. 'break src/main.rs:10; continue; vars; eval sum; bt'). Each is labeled with its command; stops at the first error or program exit. One tool call instead of a fixed break/inspect/continue recipe.",
            "inputSchema": obj(json!({"commands": {"type": "string"}}))}),
        json!({"name": "debug_breakpoints", "description": "List all breakpoints with ids.", "inputSchema": obj(json!({}))}),
        json!({"name": "debug_remove_breakpoint", "description": "Remove a breakpoint by id (or 'panic').",
            "inputSchema": obj(json!({"id": {"type": "string"}}))}),
        json!({"name": "debug_continue", "description": "Resume until the next stop.", "inputSchema": obj(json!({}))}),
        json!({"name": "debug_step", "description": "Step the current thread: over | in | out | insn.",
            "inputSchema": obj(json!({"kind": {"type": "string", "enum": ["over", "in", "out", "insn"]}}))}),
        json!({"name": "debug_run_to", "description": "Run to a line ('file.rs:line').",
            "inputSchema": obj(json!({"location": {"type": "string"}}))}),
        json!({"name": "debug_pause", "description": "Interrupt a running program.", "inputSchema": obj(json!({}))}),
        json!({"name": "debug_restart", "description": "Relaunch with the same line/function/panic breakpoints.", "inputSchema": obj(json!({}))}),
        json!({"name": "debug_locals", "description": "Local variables at the current frame, with real Rust values. `full:true` forces a deep dump (otherwise a stop already shows what changed).",
            "inputSchema": obj(json!({"depth": {"type": "integer"}, "full": {"type": "boolean"}}))}),
        json!({"name": "debug_eval", "description": "Evaluate a variable path (e.g. items[0].qty) at the current frame.",
            "inputSchema": obj(json!({"path": {"type": "string"}}))}),
        json!({"name": "debug_set", "description": "Change a variable's value.",
            "inputSchema": obj(json!({"path": {"type": "string"}, "value": {"type": "string"}}))}),
        json!({"name": "debug_backtrace", "description": "Backtrace of the current thread.", "inputSchema": obj(json!({}))}),
        json!({"name": "debug_source", "description": "Source lines around the current stop.",
            "inputSchema": obj(json!({"radius": {"type": "integer"}}))}),
        json!({"name": "debug_state", "description": "The current stop, locals, and watches in one call.", "inputSchema": obj(json!({}))}),
        json!({"name": "debug_threads", "description": "List threads.", "inputSchema": obj(json!({}))}),
        json!({"name": "debug_select_thread", "description": "Switch the current thread by id.",
            "inputSchema": obj(json!({"id": {"type": "integer"}}))}),
        json!({"name": "debug_select_frame", "description": "Select a stack frame by index (0 = innermost).",
            "inputSchema": obj(json!({"index": {"type": "integer"}}))}),
        json!({"name": "debug_watch_expr", "description": "Add or remove a watch expression (shown at every stop).",
            "inputSchema": obj(json!({"action": {"type": "string", "enum": ["add", "rm", "list"]}, "expr": {"type": "string"}}))}),
        json!({"name": "debug_where", "description": "Find a function/type/const across the workspace (rust-analyzer).",
            "inputSchema": obj(json!({"query": {"type": "string"}}))}),
        json!({"name": "debug_definition", "description": "Go to definition at file:line:col (1-based).",
            "inputSchema": obj(json!({"file": {"type": "string"}, "line": {"type": "integer"}, "col": {"type": "integer"}}))}),
        json!({"name": "debug_hover", "description": "Type/signature/docs at file:line:col.",
            "inputSchema": obj(json!({"file": {"type": "string"}, "line": {"type": "integer"}, "col": {"type": "integer"}}))}),
        json!({"name": "debug_references", "description": "Find references at file:line:col.",
            "inputSchema": obj(json!({"file": {"type": "string"}, "line": {"type": "integer"}, "col": {"type": "integer"}}))}),
        json!({"name": "debug_stop", "description": "End the debug session (keeps the daemon warm).", "inputSchema": obj(json!({}))}),
    ]
}

fn abs_file(f: &str) -> String {
    let p = Path::new(f);
    let ap = if p.is_absolute() { p.to_path_buf() } else { std::env::current_dir().unwrap_or_default().join(f) };
    ap.canonicalize().unwrap_or(ap).to_string_lossy().to_string()
}

fn parse_loc(spec: &str) -> (String, i64) {
    match spec.rsplit_once(':') {
        Some((f, l)) => (abs_file(f), l.parse().unwrap_or(0)),
        None => (abs_file(spec), 0),
    }
}

/// Launch + run through all hits + return the trace, in one tool call.
fn trace_call(ws: &Path, a: &Value) -> (String, bool) {
    let program = if let Some(c) = a["cargo"].as_str() {
        cargo_build(&PathBuf::from(c), a["bin"].as_str(), a["test"].as_str())
    } else if let Some(bp) = a["bin_path"].as_str() {
        PathBuf::from(bp).canonicalize().unwrap_or_else(|_| PathBuf::from(bp))
    } else {
        return ("provide either `cargo` (project dir) or `bin_path`".into(), true);
    };
    let bps: Vec<Value> = a["breakpoints"].as_array().map(|arr| arr.iter().filter_map(|b| b.as_str()).map(|b| {
        let (f, l) = parse_loc(b);
        json!({"file": f, "line": l})
    }).collect()).unwrap_or_default();
    ensure_daemon(ws);
    let launch = request(ws, &json!({"cmd": "launch", "program": program.to_string_lossy(),
        "cwd": program.parent().map(|p| p.to_string_lossy().to_string()),
        "args": a["args"].as_array().cloned().unwrap_or_default(),
        "breakpoints": bps, "fn_breaks": [], "panic": false}), Duration::from_secs(300));
    match launch {
        Some(v) if v["ok"].as_bool().unwrap_or(false) => {}
        Some(v) => return (format!("launch failed: {}", v["error"].as_str().unwrap_or("?")), true),
        None => return ("the rdbg daemon did not respond".into(), true),
    }
    let t = request(ws, &json!({"cmd": "trace", "captures": a["capture"].as_array().cloned().unwrap_or_default(),
        "max": a["max"].as_i64().unwrap_or(50)}), Duration::from_secs(300));
    match t {
        Some(tv) if tv["ok"].as_bool().unwrap_or(false) =>
            (format!("trace: {} hit(s)\n{}", tv["hits"].as_i64().unwrap_or(0), tv["trace"].as_str().unwrap_or("")), false),
        _ => ("trace failed".into(), true),
    }
}

/// Map a tool call to a daemon request; returns (text, is_error).
fn call(ws: &Path, name: &str, a: &Value) -> (String, bool) {
    if name == "debug_trace" {
        return trace_call(ws, a);
    }
    if name == "debug_do" {
        ensure_daemon(ws);
        return crate::client::run_batch(ws, a["commands"].as_str().unwrap_or(""));
    }
    let payload = match name {
        "debug_launch" => {
            let program = if let Some(c) = a["cargo"].as_str() {
                cargo_build(&PathBuf::from(c), a["bin"].as_str(), a["test"].as_str())
            } else if let Some(bp) = a["bin_path"].as_str() {
                PathBuf::from(bp).canonicalize().unwrap_or_else(|_| PathBuf::from(bp))
            } else {
                return ("provide either `cargo` (project dir) or `bin_path`".into(), true);
            };
            let bps: Vec<Value> = a["breakpoints"].as_array().map(|arr| arr.iter().filter_map(|b| b.as_str()).map(|b| {
                let (f, l) = parse_loc(b);
                json!({"file": f, "line": l})
            }).collect()).unwrap_or_default();
            json!({"cmd": "launch", "program": program.to_string_lossy(),
                "cwd": program.parent().map(|p| p.to_string_lossy().to_string()),
                "args": a["args"].as_array().cloned().unwrap_or_default(),
                "breakpoints": bps, "fn_breaks": a["fn_breaks"].as_array().cloned().unwrap_or_default(),
                "panic": a["panic"].as_bool().unwrap_or(false)})
        }
        "debug_add_breakpoint" => {
            if let Some(f) = a["fn"].as_str() {
                json!({"cmd": "bp_fn", "name": f})
            } else if a["panic"].as_bool().unwrap_or(false) {
                json!({"cmd": "bp_panic"})
            } else if let Some(w) = a["watch"].as_str() {
                json!({"cmd": "bp_watch", "var": w})
            } else {
                json!({"cmd": "bp_add", "file": abs_file(a["file"].as_str().unwrap_or("")),
                    "line": a["line"].as_i64().unwrap_or(0), "condition": a["condition"].clone(),
                    "hit": a["hit"].as_i64(), "log": a["log"].clone()})
            }
        }
        "debug_breakpoints" => json!({"cmd": "bp_list"}),
        "debug_remove_breakpoint" => json!({"cmd": "bp_rm", "id": a["id"].as_str().unwrap_or("")}),
        "debug_continue" => json!({"cmd": "continue"}),
        "debug_step" => json!({"cmd": "step", "kind": a["kind"].as_str().unwrap_or("over")}),
        "debug_run_to" => { let (f, l) = parse_loc(a["location"].as_str().unwrap_or("")); json!({"cmd": "until", "file": f, "line": l}) }
        "debug_pause" => json!({"cmd": "pause"}),
        "debug_restart" => json!({"cmd": "restart"}),
        "debug_locals" => {
            let full = a["full"].as_bool().unwrap_or(false);
            let depth = a["depth"].as_i64().unwrap_or(if full { 10 } else { 3 });
            json!({"cmd": "vars", "depth": depth, "full": full})
        }
        "debug_eval" => json!({"cmd": "eval", "expr": a["path"].as_str().unwrap_or("")}),
        "debug_set" => json!({"cmd": "set", "path": a["path"].as_str().unwrap_or(""), "value": a["value"].as_str().unwrap_or("")}),
        "debug_backtrace" => json!({"cmd": "bt"}),
        "debug_source" => json!({"cmd": "list", "radius": a["radius"].as_i64().unwrap_or(6)}),
        "debug_state" => json!({"cmd": "state"}),
        "debug_threads" => json!({"cmd": "threads"}),
        "debug_select_thread" => json!({"cmd": "thread", "id": a["id"].as_i64().unwrap_or(0)}),
        "debug_select_frame" => json!({"cmd": "frame", "index": a["index"].as_i64().unwrap_or(0)}),
        "debug_watch_expr" => json!({"cmd": "watch_expr", "action": a["action"].as_str().unwrap_or("list"), "expr": a["expr"].clone()}),
        "debug_where" => json!({"cmd": "where", "query": a["query"].as_str().unwrap_or("")}),
        "debug_definition" => json!({"cmd": "def", "file": a["file"].as_str().unwrap_or(""), "line": a["line"].as_i64().unwrap_or(0), "col": a["col"].as_i64().unwrap_or(0)}),
        "debug_hover" => json!({"cmd": "hover", "file": a["file"].as_str().unwrap_or(""), "line": a["line"].as_i64().unwrap_or(0), "col": a["col"].as_i64().unwrap_or(0)}),
        "debug_references" => json!({"cmd": "refs", "file": a["file"].as_str().unwrap_or(""), "line": a["line"].as_i64().unwrap_or(0), "col": a["col"].as_i64().unwrap_or(0)}),
        "debug_stop" => json!({"cmd": "stop"}),
        _ => return (format!("unknown tool {name:?}"), true),
    };
    ensure_daemon(ws);
    match request(ws, &payload, Duration::from_secs(300)) {
        None => ("the rdbg daemon did not respond (call debug_launch first?)".into(), true),
        Some(resp) => format_resp(&resp),
    }
}

fn format_resp(resp: &Value) -> (String, bool) {
    if !resp["ok"].as_bool().unwrap_or(true) {
        return (format!("error: {}", resp["error"].as_str().unwrap_or("unknown")), true);
    }
    for key in ["stop", "vars", "value", "bt", "source", "threads", "breakpoints", "hover", "watches"] {
        let v = &resp[key];
        if v.is_string() && !v.as_str().unwrap().is_empty() {
            return (v.as_str().unwrap().to_string(), false);
        }
        if v.is_object() {
            return (serde_json::to_string_pretty(v).unwrap_or_default(), false);
        }
    }
    if let Some(syms) = resp["symbols"].as_array() {
        let text = syms.iter().map(|s| format!("{}  {}:{}", s["name"].as_str().unwrap_or("?"), s["file"].as_str().unwrap_or("?"), s["line"])).collect::<Vec<_>>().join("\n");
        return (if text.is_empty() { "(no matches)".into() } else { text }, false);
    }
    if let Some(locs) = resp["locations"].as_array() {
        let text = locs.iter().map(|l| format!("{}:{}:{}", l["file"].as_str().unwrap_or("?"), l["line"], l["col"])).collect::<Vec<_>>().join("\n");
        return (if text.is_empty() { "(no results)".into() } else { text }, false);
    }
    ("ok".into(), false)
}

fn send(v: Value) {
    let mut out = std::io::stdout();
    let _ = out.write_all(serde_json::to_string(&v).unwrap().as_bytes());
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

fn reply(id: &Value, result: Value) {
    send(json!({"jsonrpc": "2.0", "id": id, "result": result}));
}

pub fn main() -> i32 {
    let ws = ws_root();
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(msg): Result<Value, _> = serde_json::from_str(line) else { continue };
        let method = msg["method"].as_str().unwrap_or("");
        let id = &msg["id"];
        match method {
            "initialize" => reply(id, json!({
                "protocolVersion": msg["params"]["protocolVersion"].as_str().unwrap_or(PROTOCOL),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "rust-debugger-skill", "version": env!("CARGO_PKG_VERSION")}})),
            "notifications/initialized" | "initialized" => {}
            "ping" => reply(id, json!({})),
            "tools/list" => reply(id, json!({"tools": tools()})),
            "tools/call" => {
                let name = msg["params"]["name"].as_str().unwrap_or("");
                let args = &msg["params"]["arguments"];
                let (text, is_error) = call(&ws, name, args);
                reply(id, json!({"content": [{"type": "text", "text": text}], "isError": is_error}));
            }
            "shutdown" => reply(id, json!({})),
            _ => {
                if !id.is_null() {
                    send(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("method {method:?} not found")}}));
                }
            }
        }
    }
    0
}
