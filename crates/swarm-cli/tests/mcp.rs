//! `swarm mcp` over real pipes, speaking what an MCP client speaks: newline
//! delimited JSON-RPC. The mock provider keeps it offline.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Value, json};

struct Client {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next: u64,
    _dir: tempfile::TempDir,
}

impl Client {
    fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_swarm"))
            .arg("mcp")
            .arg("--jobs")
            .arg(dir.path().join("jobs"))
            .env("SWARM_MOCK_SCRIPT", "coding")
            .env_remove("SWARM_PROVIDER")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start swarm mcp");
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self { child, stdin, stdout, next: 1, _dir: dir }
    }

    fn send_raw(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{line}").unwrap();
        stdin.flush().unwrap();
    }

    fn read(&mut self) -> Value {
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("not JSON ({e}): {line:?}"))
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        self.send_raw(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string());
        let reply = self.read();
        assert_eq!(reply["id"], id, "{reply}");
        reply
    }

    /// Calls a tool; returns the JSON the tool put in its text content, and isError.
    fn call(&mut self, name: &str, arguments: Value) -> (Value, bool) {
        let reply = self.request("tools/call", json!({"name": name, "arguments": arguments}));
        let result = &reply["result"];
        let text = result["content"][0]["text"].as_str().unwrap_or_else(|| panic!("no text: {reply}"));
        (serde_json::from_str(text).unwrap(), result["isError"] == true)
    }
}

#[test]
fn an_mcp_client_can_launch_a_worker_wait_for_it_and_read_the_result() {
    let mut client = Client::start();

    let init = client.request("initialize", json!({"protocolVersion": "2024-11-05", "capabilities": {},
        "clientInfo": {"name": "test", "version": "0"}}));
    assert_eq!(init["result"]["serverInfo"]["name"], "swarm");
    client.send_raw(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string());

    let tools = client.request("tools/list", json!({}));
    let names: Vec<&str> = tools["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"swarm_submit") && names.contains(&"swarm_result"), "{names:?}");

    // A malformed line gets a JSON-RPC parse error and the server keeps going.
    client.send_raw("this is not json");
    let parse = client.read();
    assert_eq!(parse["error"]["code"], -32700, "{parse}");

    let (submitted, is_error) =
        client.call("swarm_submit", json!({"goal": "create hello.txt and say done", "provider": "mock", "label": "mcp"}));
    assert!(!is_error, "{submitted}");
    let id = submitted["job_id"].as_str().unwrap().to_string();

    let (waited, is_error) = client.call("swarm_wait", json!({"job_ids": [id], "timeout_secs": 40}));
    assert!(!is_error && waited["done"] == true, "{waited}");
    assert_eq!(waited["jobs"][0]["state"], "completed");

    let (result, is_error) = client.call("swarm_result", json!({"job_id": id}));
    assert!(!is_error, "{result}");
    assert_eq!(result["state"], "completed");
    assert!(result["files_changed"].as_array().unwrap().iter().any(|f| f == "hello.txt"), "{result}");

    let (listed, _) = client.call("swarm_list", json!({"label": "mcp"}));
    assert_eq!(listed["jobs"].as_array().unwrap().len(), 1);

    // Closing stdin ends the server cleanly.
    drop(client.stdin.take());
    let status = client.child.wait().unwrap();
    assert!(status.success(), "{status:?}");
}
