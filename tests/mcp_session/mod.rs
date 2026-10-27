use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdout, Stdio};

use serde_json::{Value, json};

use crate::common::{parse_response, symora};

/// One in-process `symora mcp serve`, whose language servers stay warm
/// across the calls made on it.
pub struct McpSession {
    child: Child,
    reader: BufReader<ChildStdout>,
    next_id: u64,
}

impl McpSession {
    pub fn new(root: Option<&Path>) -> Self {
        let mut command = symora();
        command
            .args(["mcp", "serve"])
            .env("SYMORA_NO_DAEMON", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(root) = root {
            command.current_dir(root);
        }
        let mut child = command.spawn().expect("spawn symora mcp serve");
        let reader = BufReader::new(child.stdout.take().expect("MCP stdout"));
        let mut session = Self {
            child,
            reader,
            next_id: 1,
        };
        session.request(
            "initialize",
            json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "symora-tests", "version": "1"},
            }),
        );
        session.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        session
    }

    fn send(&mut self, message: Value) {
        let stdin = self.child.stdin.as_mut().expect("MCP stdin");
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();
    }

    pub fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let mut line = String::new();
            assert!(
                self.reader.read_line(&mut line).expect("read MCP response") > 0,
                "MCP closed before answering {method}",
            );
            let response: Value = serde_json::from_str(&line).expect("valid JSON-RPC response");
            if response.get("id").is_none() {
                assert!(
                    response["method"].is_string(),
                    "invalid MCP notification: {response}"
                );
                continue;
            }
            assert_eq!(response["id"], id, "MCP response ID for {method}");
            assert!(
                response.get("error").is_none(),
                "MCP {method} failed: {response}"
            );
            return response.get("result").expect("MCP result").clone();
        }
    }

    pub fn call(&mut self, tool: &str, arguments: Value) -> Vec<Value> {
        let result = self.request("tools/call", json!({"name": tool, "arguments": arguments}));
        result["content"][0]["text"]
            .as_str()
            .expect("text content")
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| parse_response(l.as_bytes()).expect("tool emits valid JSON"))
            .collect()
    }
}

impl Drop for McpSession {
    fn drop(&mut self) {
        drop(self.child.stdin.take());
        let _ = self.child.wait();
    }
}
