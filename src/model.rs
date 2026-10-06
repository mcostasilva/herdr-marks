//! Mark identity and rendering are pure; no terminals, sockets, or disk access.

use crate::api::{Pane, Snapshot, TOKEN};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Target {
    Pane {
        terminal_id: String,
    },
    Workspace {
        workspace_id: String,
        // A live terminal witnesses the workspace identity; workspace IDs alone
        // can be reused after restore. Losing all witnesses makes the mark stale.
        witnesses: Vec<String>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Mark {
    pub target: Target,
    pub label: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Session {
    pub marks: BTreeMap<char, Mark>,
    pub previous: Option<Target>,
}

#[derive(Clone, Copy)]
pub enum Resolved<'a> {
    Pane(&'a Pane),
    Workspace(&'a str),
}

impl Target {
    pub fn pane(pane: &Pane) -> Self {
        Self::Pane {
            terminal_id: pane.terminal_id.clone(),
        }
    }

    pub fn workspace(snapshot: &Snapshot, id: &str) -> Result<Self> {
        ensure!(
            snapshot.workspaces.iter().any(|w| w.workspace_id == id),
            "workspace no longer exists"
        );
        let witnesses: Vec<_> = snapshot
            .panes
            .iter()
            .filter(|p| p.workspace_id == id)
            .map(|p| p.terminal_id.clone())
            .collect();
        ensure!(
            !witnesses.is_empty(),
            "workspace has no live terminal identity"
        );
        Ok(Self::Workspace {
            workspace_id: id.to_owned(),
            witnesses,
        })
    }

    pub fn resolve<'a>(&'a self, snapshot: &'a Snapshot) -> Option<Resolved<'a>> {
        match self {
            Self::Pane { terminal_id } => {
                let mut matches = snapshot
                    .panes
                    .iter()
                    .filter(|p| &p.terminal_id == terminal_id);
                let pane = matches.next()?;
                // Duplicate identities are ambiguous, even if the API normally
                // guarantees uniqueness. Never choose arbitrarily.
                matches.next().is_none().then_some(Resolved::Pane(pane))
            }
            Self::Workspace {
                workspace_id,
                witnesses,
            } => (snapshot
                .workspaces
                .iter()
                .any(|w| &w.workspace_id == workspace_id)
                && snapshot.panes.iter().any(|p| {
                    &p.workspace_id == workspace_id && witnesses.contains(&p.terminal_id)
                }))
            .then_some(Resolved::Workspace(workspace_id)),
        }
    }
}

pub fn letter(value: &str) -> Result<char> {
    let mut chars = value.chars();
    let c = chars.next().context("expected one letter")?;
    ensure!(
        chars.next().is_none() && c.is_ascii_alphabetic(),
        "expected one ASCII letter (a-z or A-Z)"
    );
    Ok(c)
}

pub fn pane_label(pane: &Pane) -> String {
    pane.label
        .as_deref()
        .or(pane.agent.as_deref())
        .or(pane.title.as_deref())
        .unwrap_or("shell")
        .to_owned()
}

/// Terminal strings must not inject escape sequences or wrap a popup row.
pub fn safe_text(value: &str, limit: usize) -> String {
    let mut columns = 0;
    value
        .chars()
        .filter(|c| !c.is_control())
        .take(limit)
        .take_while(|c| {
            columns += unicode_width::UnicodeWidthChar::width(*c).unwrap_or(0);
            columns <= limit
        })
        .collect()
}

impl Session {
    pub fn set(&mut self, key: char, target: Target, label: String) -> Result<()> {
        ensure!(key.is_ascii_alphabetic(), "mark must be a letter");
        ensure!(
            matches!(
                (&target, key.is_ascii_lowercase()),
                (Target::Pane { .. }, true) | (Target::Workspace { .. }, false)
            ),
            "pane marks use a-z; workspace marks use A-Z"
        );
        self.marks.insert(key, Mark { target, label });
        Ok(())
    }

    /// Refresh live targets and remove missing pane marks. Returns whether any
    /// marks were removed so a read-only picker can persist cleanup if needed.
    pub fn reconcile(&mut self, snapshot: &Snapshot) -> bool {
        // Absence, not failed resolution, proves a pane is gone: duplicate
        // terminal identities remain ambiguous and must not delete marks.
        let terminals: BTreeSet<_> = snapshot
            .panes
            .iter()
            .map(|pane| pane.terminal_id.as_str())
            .collect();
        let before = self.marks.len();
        self.marks.retain(|_, mark| match &mark.target {
            Target::Pane { terminal_id } => terminals.contains(terminal_id.as_str()),
            Target::Workspace { .. } => true,
        });
        for mark in self.marks.values_mut() {
            match mark.target.resolve(snapshot) {
                Some(Resolved::Pane(pane)) => {
                    let workspace = snapshot
                        .workspaces
                        .iter()
                        .find(|w| w.workspace_id == pane.workspace_id);
                    mark.label = format!(
                        "{} · {}",
                        workspace.map(|w| w.label.as_str()).unwrap_or("workspace"),
                        pane_label(pane)
                    );
                }
                Some(Resolved::Workspace(id)) => {
                    let id = id.to_owned();
                    if let Some(workspace) =
                        snapshot.workspaces.iter().find(|w| w.workspace_id == id)
                    {
                        mark.label = workspace.label.clone();
                    }
                    if let Ok(target) = Target::workspace(snapshot, &id) {
                        mark.target = target;
                    }
                }
                None => {} // Keep stale workspace and ambiguous pane marks for manual recovery.
            }
        }
        self.marks.len() != before
    }

    /// Desired token for every resource, including empty values so reassigned
    /// or removed marks are cleared. Shell marks roll up to their Space row.
    pub fn tokens(&self, snapshot: &Snapshot) -> Vec<TokenUpdate> {
        let mut panes: BTreeMap<&str, Vec<String>> = snapshot
            .panes
            .iter()
            .map(|p| (p.pane_id.as_str(), Vec::new()))
            .collect();
        let mut spaces: BTreeMap<&str, Vec<String>> = snapshot
            .workspaces
            .iter()
            .map(|w| (w.workspace_id.as_str(), Vec::new()))
            .collect();
        for (key, mark) in &self.marks {
            match mark.target.resolve(snapshot) {
                Some(Resolved::Pane(pane)) => {
                    if let Some(values) = panes.get_mut(pane.pane_id.as_str()) {
                        values.push(key.to_string());
                    }
                    if pane.agent.is_none()
                        && let Some(values) = spaces.get_mut(pane.workspace_id.as_str())
                    {
                        values.push(key.to_string());
                    }
                }
                Some(Resolved::Workspace(id)) => {
                    if let Some(values) = spaces.get_mut(id) {
                        values.push(key.to_string());
                    }
                }
                None => {}
            }
        }
        let mut updates = Vec::new();
        for pane in &snapshot.panes {
            let value = panes
                .get(pane.pane_id.as_str())
                .map(|v| token_text(v))
                .unwrap_or_default();
            if pane
                .tokens
                .get(TOKEN)
                .map(String::as_str)
                .unwrap_or_default()
                != value
            {
                updates.push(TokenUpdate {
                    kind: Resource::Pane,
                    id: pane.pane_id.clone(),
                    value,
                });
            }
        }
        for workspace in &snapshot.workspaces {
            let value = spaces
                .get(workspace.workspace_id.as_str())
                .map(|v| token_text(v))
                .unwrap_or_default();
            if workspace
                .tokens
                .get(TOKEN)
                .map(String::as_str)
                .unwrap_or_default()
                != value
            {
                updates.push(TokenUpdate {
                    kind: Resource::Workspace,
                    id: workspace.workspace_id.clone(),
                    value,
                });
            }
        }
        updates
    }
}

fn token_text(values: &[String]) -> String {
    let text = values.join(" ");
    if text.chars().count() <= 80 {
        return text;
    }
    // Compact rather than letting Herdr silently truncate away later marks.
    values.concat()
}

#[derive(Debug, PartialEq, Eq)]
pub enum Resource {
    Pane,
    Workspace,
}

#[derive(Debug, PartialEq, Eq)]
pub struct TokenUpdate {
    pub kind: Resource,
    pub id: String,
    pub value: String,
}
