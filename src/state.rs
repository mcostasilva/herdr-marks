//! A stable lock file serializes all sessions, with durable atomic replacement.

use crate::model::Session;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions, TryLockError},
    io::{Read, Write},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

#[derive(Deserialize, Serialize)]
struct State {
    version: u32,
    sessions: BTreeMap<String, Session>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            version: 1,
            sessions: BTreeMap::new(),
        }
    }
}

pub struct Store {
    _lock: File,
    directory: PathBuf,
    state: State,
    saved: Option<Vec<u8>>,
}

impl Store {
    pub fn acquire(directory: &Path, timeout: Duration) -> Result<Self> {
        fs::create_dir_all(directory).context("create marks state directory")?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(directory.join("state.lock"))
            .context("open marks lock")?;
        let start = Instant::now();
        loop {
            match lock.try_lock() {
                Ok(()) => break,
                Err(TryLockError::WouldBlock) => {
                    ensure!(start.elapsed() < timeout, "marks are busy; retry shortly");
                    thread::sleep(Duration::from_millis(20));
                }
                Err(error) => return Err(error).context("lock marks state"),
            }
        }
        let saved = match File::open(directory.join("state.json")) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
                ensure!(bytes.len() <= 1024 * 1024, "marks state exceeds 1 MiB");
                Some(bytes)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).context("read marks state"),
        };
        let state = saved
            .as_deref()
            .map(serde_json::from_slice::<State>)
            .transpose()
            .context("invalid state.json; preserve it for recovery")?
            .unwrap_or_default();
        ensure!(
            state.version == 1,
            "unsupported marks state version {}",
            state.version
        );
        for session in state.sessions.values() {
            for (key, mark) in &session.marks {
                let mut validation = Session::default();
                validation
                    .set(*key, mark.target.clone(), mark.label.clone())
                    .context("invalid saved mark")?;
            }
        }
        Ok(Self {
            _lock: lock,
            directory: directory.to_owned(),
            state,
            saved,
        })
    }

    pub fn session(&mut self, socket: &str) -> &mut Session {
        self.state.sessions.entry(socket.to_owned()).or_default()
    }

    pub fn save(&mut self) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(&self.state)?;
        ensure!(
            bytes.len() <= 1024 * 1024,
            "marks state exceeds 1 MiB; remove old session entries"
        );
        if self.saved.as_ref() == Some(&bytes) {
            return Ok(());
        }
        let path = self.directory.join("state.tmp");
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(path, self.directory.join("state.json")).context("replace marks state")?;
        File::open(&self.directory)?
            .sync_all()
            .context("sync state directory")?;
        self.saved = Some(bytes);
        Ok(())
    }
}
