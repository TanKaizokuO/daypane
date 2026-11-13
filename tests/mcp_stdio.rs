//! End-to-end: spawn `<bin> mcp` against a temp XDG data dir and speak JSON-RPC over stdio.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use serde_json::{json, Value};

/// Cargo's path to the built binary. The variable name embeds the crate name, which must not
/// be written literally (scripts/check-name.sh), so it is assembled at compile time.
fn bin() -> PathBuf {
    PathBuf::from(env!(concat!("CARGO_BIN_EXE_", env!("CARGO_PKG_NAME"))))
}

struct Client {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    next_id: i64,
}

impl Client {
    fn spawn(data_home: &std::path::Path) -> Self {
        let mut child = Command::new(bin())
            .arg("mcp")
            .env("XDG_DATA_HOME", data_home)
            .env("XDG_CONFIG_HOME", data_home.join("config"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            lines,
            next_id: 0,
        }
    }

    fn send(&mut self, msg: &Value) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{msg}").unwrap();
        stdin.flush().unwrap();
    }

    /// Send a request and return the matching response (every stdout line must be valid JSON-RPC).
    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let line = self
                .lines
                .recv_timeout(Duration::from_secs(20))
                .unwrap_or_else(|e| panic!("no response to {method}: {e}"));
            let v: Value = serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("non-JSON on stdout {line:?}: {e}"));
            if v.get("id") == Some(&json!(id)) {
                assert!(v.get("error").is_none(), "{method} failed: {v}");
                return v["result"].clone();
            }
        }
    }

    /// Call a tool; returns (is_error, parsed JSON of the first text content or raw text).
    fn call(&mut self, name: &str, args: Value) -> (bool, Value) {
        let r = self.request("tools/call", json!({"name": name, "arguments": args}));
        let is_error = r["isError"].as_bool().unwrap_or(false);
        let text = r["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("no text content: {r}"));
        let value = serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_owned()));
        if !is_error {
            assert_eq!(
                r["structuredContent"], value,
                "structured content mirrors text"
            );
        }
        (is_error, value)
    }

    fn ok(&mut self, name: &str, args: Value) -> Value {
        let (is_error, v) = self.call(name, args);
        assert!(!is_error, "{name} returned an error: {v}");
        v
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        drop(self.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn full_task_lifecycle_over_stdio() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = Client::spawn(dir.path());

    let init = c.request(
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "0"}
        }),
    );
    assert_eq!(init["serverInfo"]["name"], env!("CARGO_PKG_NAME"));
    assert_eq!(init["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
    assert!(init["instructions"].as_str().unwrap().contains("task list"));
    c.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));

    let tools = c.request("tools/list", json!({}));
    let mut names: Vec<&str> = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "complete_task",
            "create_task",
            "delete_task",
            "get_task",
            "list_tasks",
            "reopen_task",
            "restore_task",
            "update_task"
        ]
    );

    // create
    let t = c.ok(
        "create_task",
        json!({"title": "pay rent", "due_date": "2030-01-15", "due_time": "17:00", "priority": "high", "tags": ["#Home"]}),
    );
    let id = t["id"].as_i64().unwrap();
    assert_eq!(t["title"], "pay rent");
    assert_eq!(t["due_date"], "2030-01-15");
    assert_eq!(t["due_time"], "17:00");
    assert_eq!(t["priority"], "high");
    assert_eq!(t["tags"], json!(["home"]));

    // an absent due_date leaves it alone; explicit null clears it (and the time)
    let u = c.ok("update_task", json!({"id": id, "notes": "landlord"}));
    assert_eq!(
        (u["due_date"].as_str(), u["notes"].as_str()),
        (Some("2030-01-15"), Some("landlord"))
    );
    let u = c.ok("update_task", json!({"id": id, "due_date": null}));
    assert!(u["due_date"].is_null() && u["due_time"].is_null(), "{u}");
    assert_eq!(u["title"], "pay rent");

    // delete -> trash
    let d = c.ok("delete_task", json!({"id": id}));
    assert!(d["deleted_at"].is_string());
    assert_eq!(c.ok("list_tasks", json!({}))["count"], 0);
    let trash = c.ok("list_tasks", json!({"status": "deleted"}));
    assert_eq!(trash["count"], 1);
    assert_eq!(trash["tasks"][0]["id"], id);

    // restore
    let r = c.ok("restore_task", json!({"id": id}));
    assert!(r["deleted_at"].is_null());
    assert_eq!(c.ok("list_tasks", json!({"status": "deleted"}))["count"], 0);
    assert_eq!(
        c.ok("list_tasks", json!({"query": "RENT"}))["tasks"][0]["id"],
        id
    );

    // complete / reopen / get
    assert!(c.ok("complete_task", json!({"id": id}))["completed_at"].is_string());
    assert_eq!(
        c.ok("list_tasks", json!({"status": "completed"}))["count"],
        1
    );
    assert!(c.ok("reopen_task", json!({"id": id}))["completed_at"].is_null());
    assert_eq!(c.ok("get_task", json!({"id": id}))["id"], id);

    // errors are tool results with isError, and the server keeps running
    let (is_error, msg) = c.call("get_task", json!({"id": 9999}));
    assert!(is_error && msg.as_str().unwrap().contains("9999"), "{msg}");
    let (is_error, msg) = c.call("create_task", json!({"title": "x", "due_date": "soon"}));
    assert!(
        is_error && msg.as_str().unwrap().contains("YYYY-MM-DD"),
        "{msg}"
    );
    let (is_error, _) = c.call("create_task", json!({"title": "x", "due_time": "09:00"}));
    assert!(is_error);
    assert_eq!(c.ok("get_task", json!({"id": id}))["id"], id);
}
