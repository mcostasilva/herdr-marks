//! Newline-delimited JSON over Herdr's local Unix socket. No private TUI socket.

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

pub const PLUGIN: &str = "herdr-marks";
pub const TOKEN: &str = "marks";
const RESPONSE_LIMIT: u64 = 8 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize)]
pub struct Pane {
    pub pane_id: String,
    pub terminal_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    pub label: Option<String>,
    pub agent: Option<String>,
    pub title: Option<String>,
    #[serde(default)]
    pub tokens: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Workspace {
    pub workspace_id: String,
    pub label: String,
    #[serde(default)]
    pub tokens: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Snapshot {
    pub focused_pane_id: Option<String>,
    pub focused_workspace_id: Option<String>,
    pub panes: Vec<Pane>,
    pub workspaces: Vec<Workspace>,
}

pub trait Host {
    fn snapshot(&self) -> Result<Snapshot>;
    fn request(&self, method: &str, params: Value) -> Result<Value>;
}

pub struct Client {
    pub socket: PathBuf,
    pub timeout: Duration,
}

impl Host for Client {
    fn snapshot(&self) -> Result<Snapshot> {
        #[derive(Deserialize)]
        struct Response {
            snapshot: Snapshot,
        }
        Ok(self
            .call::<Response>("session.snapshot", json!({}))?
            .snapshot)
    }

    fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.call(method, params)
    }
}

impl Client {
    fn call<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<T> {
        let mut stream = UnixStream::connect(&self.socket)
            .with_context(|| format!("connect to {}", self.socket.display()))?;
        let deadline = Instant::now() + self.timeout;
        // Configure once before sending. On macOS, resetting socket timeouts
        // after a fast peer has replied and closed can intermittently fail.
        // Nonblocking I/O also enforces one total budget for partial responses.
        stream.set_nonblocking(true)?;
        let id = format!("{PLUGIN}:{}", std::process::id());
        let request = json!({"id": id, "method": method, "params": params});
        let mut bytes = serde_json::to_vec(&request)?;
        bytes.push(b'\n');
        let mut pending = bytes.as_slice();
        while !pending.is_empty() {
            ensure!(
                Instant::now() < deadline,
                "Herdr request {method} timed out"
            );
            match stream.write(pending) {
                Ok(0) => bail!("Herdr connection closed while sending {method}"),
                Ok(count) => pending = &pending[count..],
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error).context("send Herdr request"),
            }
        }
        let mut line = Vec::new();
        let mut buffer = [0; 8192];
        loop {
            ensure!(
                Instant::now() < deadline,
                "Herdr request {method} timed out"
            );
            let count = match stream.read(&mut buffer) {
                Ok(count) => count,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    return Err(error).with_context(|| format!("read Herdr response to {method}"));
                }
            };
            if count == 0 {
                break;
            }
            let newline = buffer[..count].iter().position(|b| *b == b'\n');
            line.extend_from_slice(&buffer[..newline.map(|i| i + 1).unwrap_or(count)]);
            ensure!(
                line.len() as u64 <= RESPONSE_LIMIT,
                "Herdr response exceeds 8 MiB"
            );
            if newline.is_some() {
                break;
            }
        }
        ensure!(
            line.last() == Some(&b'\n'),
            "incomplete Herdr response to {method}"
        );
        let response: Value = serde_json::from_slice(&line).context("invalid Herdr JSON")?;
        ensure!(response["id"] == id, "Herdr response id mismatch");
        if let Some(error) = response.get("error") {
            bail!(
                "{method}: {}",
                error["message"].as_str().unwrap_or("API error")
            );
        }
        serde_json::from_value(response.get("result").context("missing result")?.clone())
            .with_context(|| format!("unexpected response to {method}"))
    }
}
