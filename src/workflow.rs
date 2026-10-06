use crate::{
    api::{Host, PLUGIN, Snapshot, TOKEN},
    model::{Resolved, Resource, Session, Target, pane_label},
    state::Store,
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};

#[derive(Clone, Debug, Default)]
pub struct Selection {
    pub pane_id: Option<String>,
    pub workspace_id: Option<String>,
}

impl Selection {
    pub fn capture(&self, key: char, snapshot: &Snapshot) -> Result<Target> {
        if key.is_ascii_lowercase() {
            let id = self
                .pane_id
                .as_ref()
                .or(snapshot.focused_pane_id.as_ref())
                .context("no focused pane; specify --pane")?;
            let pane = snapshot
                .panes
                .iter()
                .find(|p| &p.pane_id == id)
                .context("selected pane no longer exists")?;
            Ok(Target::pane(pane))
        } else {
            let id = self
                .workspace_id
                .as_ref()
                .or(snapshot.focused_workspace_id.as_ref())
                .context("no focused workspace; specify --workspace")?;
            Target::workspace(snapshot, id)
        }
    }
}

pub enum Operation {
    Set(char, Selection),
    SetCaptured(char, Target),
    Jump(char),
    Remove(char),
    Back,
    Sync,
    List,
}

pub struct Outcome {
    pub message: Option<String>,
    pub listing: Option<Value>,
}

pub fn run(
    host: &impl Host,
    directory: &Path,
    scope: &str,
    timeout: Duration,
    operation: Operation,
) -> Result<Outcome> {
    let mut store = Store::acquire(directory, timeout)?;
    let snapshot = host.snapshot()?;
    let session = store.session(scope);
    session.reconcile(&snapshot);
    let is_list = matches!(operation, Operation::List);
    let message = match operation {
        Operation::Set(key, selection) => {
            let target = selection.capture(key, &snapshot)?;
            set(session, key, target, &snapshot)?;
            Some(format!("Marked [{key}]"))
        }
        Operation::SetCaptured(key, target) => {
            set(session, key, target, &snapshot)?;
            Some(format!("Marked [{key}]"))
        }
        Operation::Remove(key) => {
            ensure!(
                session.marks.remove(&key).is_some(),
                "mark [{key}] is not set"
            );
            Some(format!("Removed [{key}]"))
        }
        Operation::Jump(key) => {
            let target = session
                .marks
                .get(&key)
                .with_context(|| format!("mark [{key}] is not set"))?
                .target
                .clone();
            jump(host, session, &target, &snapshot)?;
            Some(format!("Jumped to [{key}]"))
        }
        Operation::Back => {
            let target = session
                .previous
                .clone()
                .context("no previous jump location")?;
            jump(host, session, &target, &snapshot)?;
            None
        }
        Operation::Sync | Operation::List => None,
    };
    let listing = is_list.then(|| listing(session, &snapshot));
    let updates = session.tokens(&snapshot);
    // The saved mark is authoritative. If publishing fails, a later sync repairs
    // the sidebar instead of losing the user's mark.
    store.save()?;
    let mut failures = Vec::new();
    for update in updates {
        let (method, key) = match update.kind {
            Resource::Pane => ("pane.report_metadata", "pane_id"),
            Resource::Workspace => ("workspace.report_metadata", "workspace_id"),
        };
        let value = if update.value.is_empty() {
            Value::Null
        } else {
            json!(update.value)
        };
        let params = json!({(key): update.id, "source": PLUGIN, "tokens": {(TOKEN): value}});
        if let Err(error) = host.request(method, params) {
            failures.push(error.to_string());
        }
    }
    ensure!(
        failures.is_empty(),
        "marks saved, but sidebar refresh failed; run sync: {}",
        failures.join("; ")
    );
    Ok(Outcome { message, listing })
}

fn set(session: &mut Session, key: char, target: Target, snapshot: &Snapshot) -> Result<()> {
    let label = match target
        .resolve(snapshot)
        .context("mark target is stale; select it again")?
    {
        Resolved::Pane(pane) => pane_label(pane),
        Resolved::Workspace(id) => snapshot
            .workspaces
            .iter()
            .find(|w| w.workspace_id == id)
            .context("workspace no longer exists")?
            .label
            .clone(),
    };
    session.set(key, target, label)?;
    session.reconcile(snapshot);
    Ok(())
}

fn jump(
    host: &impl Host,
    session: &mut Session,
    target: &Target,
    snapshot: &Snapshot,
) -> Result<()> {
    let origin = snapshot
        .focused_pane_id
        .as_ref()
        .and_then(|id| snapshot.panes.iter().find(|p| &p.pane_id == id))
        .map(Target::pane);
    match target.resolve(snapshot) {
        Some(Resolved::Pane(pane)) => {
            host.request("pane.focus", json!({"pane_id": pane.pane_id}))?;
        }
        Some(Resolved::Workspace(id)) => {
            host.request("workspace.focus", json!({"workspace_id": id}))?;
        }
        None => bail!("mark target is stale; re-mark it or remove the letter"),
    }
    if let Some(origin) = origin
        && &origin != target
    {
        session.previous = Some(origin);
    }
    Ok(())
}

pub fn listing(session: &Session, snapshot: &Snapshot) -> Value {
    let rows: Vec<_> = session
        .marks
        .iter()
        .map(|(key, mark)| {
            let resolved = mark.target.resolve(snapshot);
            let (pane_id, workspace_id, tab_id) = match resolved {
                Some(Resolved::Pane(pane)) => (
                    Some(pane.pane_id.as_str()),
                    Some(pane.workspace_id.as_str()),
                    Some(pane.tab_id.as_str()),
                ),
                Some(Resolved::Workspace(id)) => (None, Some(id), None),
                None => (None, None, None),
            };
            json!({"letter": key.to_string(), "label": mark.label, "reachable": resolved.is_some(),
            "pane_id": pane_id, "workspace_id": workspace_id, "tab_id": tab_id})
        })
        .collect();
    json!(rows)
}
