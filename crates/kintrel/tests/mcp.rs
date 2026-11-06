//! `kintrel mcp` over its real stdio protocol (newline-delimited JSON-RPC, SPEC §12.2).

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

use kintrel::daemon::{self, Options};
use kintrel_core::log::Durability;
use serde_json::{Value, json};

#[test]
fn edit_a_symbol_over_mcp() {
    let repo = std::env::temp_dir().join(format!("kt-mcp-{}", std::process::id()));
    std::fs::remove_dir_all(&repo).ok();
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("a.py"), "def f():\n    return 1\n\n\ndef g():\n    return 2\n").unwrap();
    kintrel_core::board::init(&repo).unwrap();
    let opts = Options { repo: repo.clone(), durability: Durability::Normal, promote_every: None };
    std::thread::spawn(move || {
        tokio::runtime::Runtime::new().unwrap().block_on(daemon::run(opts)).unwrap();
    });
    let sock = daemon::socket_path(&repo).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    while rt.block_on(kintrel::ipc::connect(&sock)).is_err() {
        std::thread::sleep(Duration::from_millis(10));
    }

    let mut child = Command::new(env!("CARGO_BIN_EXE_kintrel"))
        .arg("--repo")
        .arg(&repo)
        .args(["mcp", "--agent", "mcp-test"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut send = |msg: Value| writeln!(stdin, "{msg}").unwrap();
    let mut recv = |id: u64| -> Value {
        loop {
            let mut line = String::new();
            assert!(stdout.read_line(&mut line).unwrap() > 0, "server closed stdout");
            let v: Value = serde_json::from_str(&line).unwrap();
            if v["id"] == id {
                return v;
            }
        }
    };
    let call = |id: u64, name: &str, args: Value| {
        json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": { "name": name, "arguments": args } })
    };
    let text = |v: &Value| v["result"]["content"][0]["text"].as_str().unwrap_or_default().to_owned();

    send(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "test", "version": "0" } } }));
    let init = recv(1);
    assert!(init["result"]["instructions"].as_str().unwrap().contains("Kintrel"));
    send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));

    send(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }));
    let tools: Vec<String> =
        recv(2)["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_owned()).collect();
    for t in ["kintrel_read_file", "kintrel_edit_symbol", "kintrel_list_symbols", "kintrel_status", "kintrel_refresh"] {
        assert!(tools.iter().any(|x| x == t), "missing tool {t}: {tools:?}");
    }

    send(call(3, "kintrel_list_symbols", json!({ "path": "a.py" })));
    assert!(text(&recv(3)).contains("a.py::def g"));

    send(call(4, "kintrel_edit_symbol", json!({ "key": "a.py::def g", "content": "def g():\n    return 20\n" })));
    let edited = recv(4);
    assert!(text(&edited).contains("a.py::def g"), "{edited}");

    send(call(5, "kintrel_read_file", json!({ "path": "a.py" })));
    assert_eq!(text(&recv(5)), "def f():\n    return 1\n\n\ndef g():\n    return 20\n");

    send(call(6, "kintrel_read_symbol", json!({ "key": "a.py::def nope" })));
    let missing = recv(6);
    assert!(missing.get("error").is_some() || missing["result"]["isError"] == true, "{missing}");

    child.kill().ok();
    child.wait().ok();
    std::fs::remove_dir_all(kintrel_core::board::runtime_dir(&repo).unwrap()).ok();
    std::fs::remove_dir_all(&repo).ok();
}
