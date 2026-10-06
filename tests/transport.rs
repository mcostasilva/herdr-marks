//! Exercise the real binary with a disposable socket; never touch the user's session.

use herdr_marks::api::{Client, Host};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixListener,
    process::Command,
    thread,
    time::Duration,
};
use tempfile::TempDir;

fn fixture() -> Value {
    json!({"focused_pane_id": "w1:p1", "focused_workspace_id": "w1",
        "panes": [{"pane_id":"w1:p1", "terminal_id":"term_unique", "workspace_id":"w1", "tab_id":"w1:t1", "agent":"opencode"}],
        "workspaces": [{"workspace_id":"w1", "label":"api"}]})
}

fn command(root: &TempDir) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_herdr-marks"));
    command
        .env("HERDR_ENV", "1")
        .env("HERDR_SOCKET_PATH", root.path().join("api.sock"))
        .env("HERDR_PLUGIN_STATE_DIR", root.path().join("state"))
        .env_remove("HERDR_PLUGIN_CONTEXT_JSON")
        .env_remove("HERDR_PLUGIN_ACTION_ID")
        .env_remove("HERDR_PANE_ID")
        .env_remove("HERDR_WORKSPACE_ID")
        .env_remove("HERDR_ACTIVE_PANE_ID")
        .env_remove("HERDR_ACTIVE_WORKSPACE_ID");
    command
}

#[test]
fn binary_sets_a_mark_and_publishes_exact_api_shape() {
    let root = TempDir::new().unwrap();
    let listener = UnixListener::bind(root.path().join("api.sock")).unwrap();
    let server = thread::spawn(move || {
        let mut calls = Vec::new();
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            let result = if request["method"] == "session.snapshot" {
                json!({"type":"session_snapshot", "snapshot":fixture()})
            } else {
                json!({"type":"pane_info"})
            };
            writeln!(stream, "{}", json!({"id":request["id"], "result":result})).unwrap();
            calls.push(request);
        }
        calls
    });
    let output = command(&root).args(["set", "a"]).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let calls = server.join().unwrap();
    assert_eq!(calls[1]["method"], "pane.report_metadata");
    assert_eq!(
        calls[1]["params"],
        json!({"pane_id":"w1:p1", "source":"herdr-marks", "tokens":{"marks":"a"}})
    );
    let state: Value =
        serde_json::from_slice(&std::fs::read(root.path().join("state/state.json")).unwrap())
            .unwrap();
    assert_eq!(state["version"], 1);
    assert!(String::from_utf8_lossy(&output.stdout).contains("Marked [a]"));
}

#[test]
fn response_error_is_reported_and_not_saved_as_success() {
    let root = TempDir::new().unwrap();
    let listener = UnixListener::bind(root.path().join("api.sock")).unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        let request: Value = serde_json::from_str(&line).unwrap();
        writeln!(stream, "{}", json!({"id":request["id"], "error":{"code":"test_error", "message":"session unavailable"}})).unwrap();
    });
    let output = command(&root).args(["set", "a"]).output().unwrap();
    server.join().unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("session unavailable"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!root.path().join("state/state.json").exists());
}

#[test]
fn help_works_outside_herdr_and_writes_nothing() {
    let output = Command::new(env!("CARGO_BIN_EXE_herdr-marks"))
        .env_remove("HERDR_ENV")
        .arg("--help")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("a-z"));
}

#[test]
fn transport_rejects_mismatched_response_ids() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("api.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        writeln!(
            stream,
            "{}",
            json!({"id":"wrong", "result":{"snapshot":fixture()}})
        )
        .unwrap();
    });
    let error = Client {
        socket: path,
        timeout: Duration::from_secs(1),
    }
    .snapshot()
    .unwrap_err();
    server.join().unwrap();
    assert!(error.to_string().contains("id mismatch"));
}

#[test]
fn transport_does_not_wait_indefinitely_for_a_response() {
    let root = TempDir::new().unwrap();
    let path = root.path().join("api.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        thread::sleep(Duration::from_millis(150));
        drop(stream);
    });
    let error = Client {
        socket: path,
        timeout: Duration::from_millis(40),
    }
    .snapshot()
    .unwrap_err();
    server.join().unwrap();
    assert!(
        error.to_string().contains("read Herdr response")
            || error.to_string().contains("timed out")
    );
}
