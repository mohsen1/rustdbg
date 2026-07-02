//! Debug Adapter Protocol client over stdio (framed JSON-RPC).
//!
//! Messages are framed `Content-Length: N\r\n\r\n<json>`. A reader thread
//! decodes them and routes responses to the caller that made the request (by
//! `request_seq`) and events to a queue.

use std::collections::HashMap;
use std::io::BufReader;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::util::{kill_group, own_process_group, read_framed, write_framed};

pub struct Dap {
    child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    seq: AtomicI64,
    replies: Arc<Mutex<HashMap<i64, Sender<Value>>>>,
    pending: Mutex<HashMap<i64, Receiver<Value>>>,
    events: Mutex<Receiver<Value>>,
}

impl Dap {
    pub fn spawn(program: &str) -> std::io::Result<Dap> {
        let mut cmd = Command::new(program);
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        own_process_group(&mut cmd);
        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let replies: Arc<Mutex<HashMap<i64, Sender<Value>>>> = Arc::new(Mutex::new(HashMap::new()));
        let (ev_tx, ev_rx) = channel::<Value>();
        let replies_r = Arc::clone(&replies);
        std::thread::spawn(move || {
            let mut r = BufReader::new(stdout);
            while let Some(msg) = read_framed(&mut r) {
                match msg.get("type").and_then(|t| t.as_str()) {
                    Some("response") => {
                        if let Some(rs) = msg.get("request_seq").and_then(|s| s.as_i64()) {
                            if let Some(tx) = replies_r.lock().unwrap().remove(&rs) {
                                let _ = tx.send(msg);
                            }
                        }
                    }
                    Some("event") => {
                        let _ = ev_tx.send(msg);
                    }
                    Some("request") => { /* reverse request (runInTerminal): ignore */ }
                    _ => {}
                }
            }
        });
        Ok(Dap {
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            seq: AtomicI64::new(0),
            replies,
            pending: Mutex::new(HashMap::new()),
            events: Mutex::new(ev_rx),
        })
    }

    fn write(&self, payload: &Value) {
        let mut w = self.stdin.lock().unwrap();
        write_framed(&mut *w, payload);
    }

    /// Send a request; return its seq (retrieve the reply with `reply`).
    pub fn send(&self, command: &str, args: Value) -> i64 {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let (tx, rx) = channel::<Value>();
        self.replies.lock().unwrap().insert(seq, tx);
        // stash the receiver so reply() can find it
        self.pending.lock().unwrap().insert(seq, rx);
        let mut payload = json!({"seq": seq, "type": "request", "command": command});
        if !args.is_null() {
            payload["arguments"] = args;
        }
        self.write(&payload);
        seq
    }

    pub fn reply(&self, seq: i64, timeout: Duration) -> Option<Value> {
        let rx = self.pending.lock().unwrap().remove(&seq)?;
        rx.recv_timeout(timeout).ok()
    }

    /// Send + block for the response. Returns Ok(response) on success, Err(msg) on failure.
    pub fn request(&self, command: &str, args: Value, timeout: Duration) -> Result<Value, String> {
        let seq = self.send(command, args);
        match self.reply(seq, timeout) {
            None => Err(format!("timed out waiting for {command}")),
            Some(resp) => {
                if resp.get("success").and_then(|s| s.as_bool()).unwrap_or(false) {
                    Ok(resp)
                } else {
                    Err(resp
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("request failed")
                        .to_string())
                }
            }
        }
    }

    /// Send + block; return the (possibly failed) response.
    pub fn request_soft(&self, command: &str, args: Value, timeout: Duration) -> Value {
        let seq = self.send(command, args);
        self.reply(seq, timeout).unwrap_or_else(|| json!({"success": false, "message": "no response"}))
    }

    pub fn wait_event(&self, name: &str, timeout: Duration) -> Option<Value> {
        let deadline = Instant::now() + timeout;
        let rx = self.events.lock().unwrap();
        loop {
            let remaining = deadline.checked_duration_since(Instant::now())?;
            match rx.recv_timeout(remaining) {
                Ok(ev) => {
                    if ev.get("event").and_then(|e| e.as_str()) == Some(name) {
                        return Some(ev);
                    }
                }
                Err(_) => return None,
            }
        }
    }

    pub fn poll_event(&self, timeout: Duration) -> Option<Value> {
        self.events.lock().unwrap().recv_timeout(timeout).ok()
    }

    pub fn close(&self) {
        // terminate the adapter and its debugserver child via the process group
        let mut child = self.child.lock().unwrap();
        kill_group(child.id());
        let _ = child.kill();
        let _ = child.wait();
    }
}

impl Drop for Dap {
    fn drop(&mut self) {
        if let Ok(mut c) = self.child.lock() {
            kill_group(c.id());
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}
