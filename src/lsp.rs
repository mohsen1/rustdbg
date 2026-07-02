//! rust-analyzer client for navigation (definition / hover / references /
//! workspace symbols). checkOnSave off; kept warm by the daemon.

use std::collections::{HashMap, HashSet};
use std::io::BufReader;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::util::{find_rust_analyzer, own_process_group, read_framed, rel_from_uri, uri, write_framed};

pub struct Lsp {
    root: String,
    child: Mutex<Child>,
    stdin: Arc<Mutex<ChildStdin>>,
    id: AtomicI64,
    replies: Arc<Mutex<HashMap<i64, Sender<Value>>>>,
    opened: Mutex<HashSet<String>>,
    ready: Arc<AtomicBool>,
}

impl Lsp {
    pub fn spawn(root: &str) -> Result<Lsp, String> {
        let ra = find_rust_analyzer().ok_or("rust-analyzer not found (rustup component add rust-analyzer)")?;
        let mut cmd = Command::new(ra);
        cmd.current_dir(root).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        own_process_group(&mut cmd);
        let mut child = cmd.spawn().map_err(|e| format!("failed to launch rust-analyzer: {e}"))?;
        let stdin = Arc::new(Mutex::new(child.stdin.take().unwrap()));
        let stdout = child.stdout.take().unwrap();
        let replies: Arc<Mutex<HashMap<i64, Sender<Value>>>> = Arc::new(Mutex::new(HashMap::new()));
        let ready = Arc::new(AtomicBool::new(false));

        let replies_r = Arc::clone(&replies);
        let ready_r = Arc::clone(&ready);
        let stdin_r = Arc::clone(&stdin);
        std::thread::spawn(move || {
            let mut r = BufReader::new(stdout);
            while let Some(msg) = read_framed(&mut r) {
                let has_result = msg.get("result").is_some() || msg.get("error").is_some();
                if msg.get("id").is_some() && has_result {
                    if let Some(id) = msg.get("id").and_then(|i| i.as_i64()) {
                        if let Some(tx) = replies_r.lock().unwrap().remove(&id) {
                            let _ = tx.send(msg);
                        }
                    }
                } else if let Some(method) = msg.get("method").and_then(|m| m.as_str()) {
                    match method {
                        "$/progress" => {
                            if msg["params"]["value"]["kind"].as_str() == Some("end") {
                                ready_r.store(true, Ordering::SeqCst);
                            }
                        }
                        // server->client requests we must answer so RA proceeds
                        "workspace/configuration" | "client/registerCapability"
                        | "window/workDoneProgress/create" => {
                            if let Some(id) = msg.get("id") {
                                let result = match msg["params"]["items"].as_array() {
                                    Some(items) => Value::Array(vec![Value::Null; items.len()]),
                                    None => Value::Null,
                                };
                                let reply = json!({"jsonrpc": "2.0", "id": id, "result": result});
                                let mut w = stdin_r.lock().unwrap();
                                write_framed(&mut *w, &reply);
                            }
                        }
                        _ => {}
                    }
                }
            }
        });

        let lsp = Lsp {
            root: root.to_string(),
            child: Mutex::new(child),
            stdin,
            id: AtomicI64::new(0),
            replies,
            opened: Mutex::new(HashSet::new()),
            ready,
        };
        lsp.initialize();
        Ok(lsp)
    }

    fn write(&self, payload: &Value) {
        let mut w = self.stdin.lock().unwrap();
        write_framed(&mut *w, payload);
    }

    fn request(&self, method: &str, params: Value, timeout: Duration) -> Value {
        let id = self.id.fetch_add(1, Ordering::SeqCst) + 1;
        let (tx, rx) = channel();
        self.replies.lock().unwrap().insert(id, tx);
        self.write(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        rx.recv_timeout(timeout).unwrap_or(Value::Null)
    }

    fn notify(&self, method: &str, params: Value) {
        self.write(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    fn initialize(&self) {
        self.request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": uri(&self.root),
                "workspaceFolders": [{"uri": uri(&self.root), "name": "root"}],
                "capabilities": {"textDocument": {"hover": {"contentFormat": ["plaintext"]},
                                                  "definition": {}, "references": {}},
                                 "workspace": {"symbol": {}, "configuration": true, "workspaceFolders": true},
                                 "window": {"workDoneProgress": true}},
                "initializationOptions": {"checkOnSave": false,
                                          "cargo": {"buildScripts": {"enable": true}},
                                          "procMacro": {"enable": true}}
            }),
            Duration::from_secs(120),
        );
        self.notify("initialized", json!({}));
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }

    pub fn wait_ready(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.ready.load(Ordering::SeqCst) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(300));
        }
        self.ready.load(Ordering::SeqCst)
    }

    fn open(&self, rel: &str) -> Option<String> {
        let p = std::path::Path::new(rel);
        let abs = if p.is_absolute() { p.to_path_buf() } else { std::path::Path::new(&self.root).join(rel) };
        let abs = abs.canonicalize().ok()?;
        let key = abs.to_string_lossy().to_string();
        if !self.opened.lock().unwrap().contains(&key) {
            let text = std::fs::read_to_string(&abs).ok()?;
            self.notify(
                "textDocument/didOpen",
                json!({"textDocument": {"uri": uri(&key), "languageId": "rust", "version": 1, "text": text}}),
            );
            self.opened.lock().unwrap().insert(key.clone());
            std::thread::sleep(Duration::from_millis(300));
        }
        Some(key)
    }

    fn pos(&self, rel: &str, line: i64, col: i64) -> Option<Value> {
        let key = self.open(rel)?;
        Some(json!({"textDocument": {"uri": uri(&key)},
                    "position": {"line": (line - 1).max(0), "character": (col - 1).max(0)}}))
    }

    fn loc(&self, l: &Value) -> Value {
        let u = l.get("uri").or_else(|| l.get("targetUri")).and_then(|x| x.as_str()).unwrap_or("");
        let range = l.get("range").or_else(|| l.get("targetSelectionRange")).cloned().unwrap_or(json!({}));
        let s = &range["start"];
        json!({"file": rel_from_uri(u, &self.root),
               "line": s["line"].as_i64().unwrap_or(0) + 1,
               "col": s["character"].as_i64().unwrap_or(0) + 1})
    }

    pub fn definition(&self, rel: &str, line: i64, col: i64) -> Vec<Value> {
        let Some(params) = self.pos(rel, line, col) else { return vec![] };
        let res = self.request("textDocument/definition", params, Duration::from_secs(30));
        match &res["result"] {
            Value::Array(a) => a.iter().map(|l| self.loc(l)).collect(),
            r @ Value::Object(_) => vec![self.loc(r)],
            _ => vec![],
        }
    }

    pub fn references(&self, rel: &str, line: i64, col: i64) -> Vec<Value> {
        let Some(mut params) = self.pos(rel, line, col) else { return vec![] };
        params["context"] = json!({"includeDeclaration": false});
        let res = self.request("textDocument/references", params, Duration::from_secs(30));
        res["result"].as_array().map(|a| a.iter().take(30).map(|l| self.loc(l)).collect()).unwrap_or_default()
    }

    pub fn hover(&self, rel: &str, line: i64, col: i64) -> String {
        let Some(params) = self.pos(rel, line, col) else { return String::new() };
        let res = self.request("textDocument/hover", params, Duration::from_secs(30));
        match &res["result"]["contents"] {
            Value::Object(o) => o.get("value").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            Value::String(s) => s.clone(),
            Value::Array(a) => a.iter()
                .map(|c| c.get("value").and_then(|v| v.as_str()).unwrap_or("").to_string())
                .collect::<Vec<_>>().join("\n"),
            _ => String::new(),
        }
    }

    pub fn symbols(&self, query: &str) -> Vec<Value> {
        let res = self.request("workspace/symbol", json!({"query": query}), Duration::from_secs(30));
        res["result"].as_array().map(|a| {
            a.iter().take(30).map(|s| {
                let mut d = self.loc(&s["location"]);
                d["name"] = s["name"].clone();
                d["container"] = s.get("containerName").cloned().unwrap_or(Value::Null);
                d
            }).collect()
        }).unwrap_or_default()
    }

}

impl Drop for Lsp {
    fn drop(&mut self) {
        let _ = self.request("shutdown", json!({}), Duration::from_secs(3));
        self.notify("exit", json!({}));
        if let Ok(mut c) = self.child.lock() {
            crate::util::kill_group(c.id());
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}
