use crate::{
    api::{Host, Snapshot, TOKEN},
    model::{Resource, Session, Target, letter, safe_text},
    popup::{Mode, input_letter},
    state::Store,
    workflow::{self, Operation, Selection},
};
use anyhow::{Result, bail};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use serde_json::{Value, json};
use std::{cell::RefCell, time::Duration};
use tempfile::TempDir;

fn snapshot() -> Snapshot {
    serde_json::from_value(json!({
        "focused_pane_id": "w1:p1", "focused_workspace_id": "w1",
        "workspaces": [{"workspace_id": "w1", "label": "api"},
                       {"workspace_id": "w2", "label": "frontend"}],
        "panes": [
            {"pane_id": "w1:p1", "terminal_id": "term_source", "workspace_id": "w1", "tab_id": "w1:t1", "agent": "opencode"},
            {"pane_id": "w1:p2", "terminal_id": "term_shell", "workspace_id": "w1", "tab_id": "w1:t2", "label": "logs"},
            {"pane_id": "w2:p1", "terminal_id": "term_other", "workspace_id": "w2", "tab_id": "w2:t1", "agent": "claude"}
        ]
    })).unwrap()
}

struct FakeHost {
    snapshot: RefCell<Snapshot>,
    calls: RefCell<Vec<(String, Value)>>,
    fail: RefCell<Option<String>>,
}

impl FakeHost {
    fn new() -> Self {
        Self {
            snapshot: RefCell::new(snapshot()),
            calls: RefCell::new(vec![]),
            fail: RefCell::new(None),
        }
    }
    fn run(&self, directory: &TempDir, operation: Operation) -> Result<workflow::Outcome> {
        workflow::run(
            self,
            directory.path(),
            "session-one",
            Duration::from_secs(1),
            operation,
        )
    }
}

impl Host for FakeHost {
    fn snapshot(&self) -> Result<Snapshot> {
        Ok(self.snapshot.borrow().clone())
    }
    fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.calls
            .borrow_mut()
            .push((method.to_owned(), params.clone()));
        if self.fail.borrow().as_deref() == Some(method) {
            bail!("simulated failure");
        }
        let mut snapshot = self.snapshot.borrow_mut();
        match method {
            "pane.report_metadata" => {
                assert_eq!(params["source"], "herdr-marks");
                let pane = snapshot
                    .panes
                    .iter_mut()
                    .find(|p| p.pane_id == params["pane_id"])
                    .unwrap();
                match params["tokens"][TOKEN].as_str() {
                    Some(s) => {
                        pane.tokens.insert(TOKEN.to_owned(), s.to_owned());
                    }
                    None => {
                        pane.tokens.remove(TOKEN);
                    }
                }
            }
            "workspace.report_metadata" => {
                let workspace = snapshot
                    .workspaces
                    .iter_mut()
                    .find(|w| w.workspace_id == params["workspace_id"])
                    .unwrap();
                match params["tokens"][TOKEN].as_str() {
                    Some(s) => {
                        workspace.tokens.insert(TOKEN.to_owned(), s.to_owned());
                    }
                    None => {
                        workspace.tokens.remove(TOKEN);
                    }
                }
            }
            "pane.focus" => {
                let pane = snapshot
                    .panes
                    .iter()
                    .find(|p| p.pane_id == params["pane_id"])
                    .unwrap();
                let id = pane.pane_id.clone();
                let workspace = pane.workspace_id.clone();
                snapshot.focused_pane_id = Some(id);
                snapshot.focused_workspace_id = Some(workspace);
            }
            "workspace.focus" => {
                snapshot.focused_workspace_id =
                    Some(params["workspace_id"].as_str().unwrap().to_owned());
            }
            _ => panic!("unexpected request {method}"),
        }
        Ok(json!({"type": "ok"}))
    }
}

#[test]
fn letters_are_ascii_and_case_sensitive() {
    assert_eq!(letter("a").unwrap(), 'a');
    assert_eq!(letter("A").unwrap(), 'A');
    for value in ["", "ab", "1", "é", " a", "😀"] {
        assert!(letter(value).is_err());
    }
    let mut session = Session::default();
    let target = Target::pane(&snapshot().panes[0]);
    assert!(session.set('A', target.clone(), "pane".into()).is_err());
    session.set('a', target, "pane".into()).unwrap();
    session
        .set(
            'A',
            Target::workspace(&snapshot(), "w1").unwrap(),
            "workspace".into(),
        )
        .unwrap();
    assert_eq!(session.marks.len(), 2);
}

#[test]
fn pane_identity_follows_moves_but_rejects_recycled_ids() {
    let mut snapshot = snapshot();
    let target = Target::pane(&snapshot.panes[0]);
    snapshot.panes[0].pane_id = "w2:p9".into();
    snapshot.panes[0].workspace_id = "w2".into();
    assert!(target.resolve(&snapshot).is_some());
    snapshot.panes[0].terminal_id = "new_terminal".into();
    assert!(target.resolve(&snapshot).is_none());
}

#[test]
fn duplicate_terminal_identity_is_ambiguous() {
    let mut snapshot = snapshot();
    let target = Target::pane(&snapshot.panes[0]);
    snapshot.panes[1].terminal_id = snapshot.panes[0].terminal_id.clone();
    assert!(target.resolve(&snapshot).is_none());
}

#[test]
fn workspace_mark_needs_a_live_witness_in_the_same_workspace() {
    let mut snapshot = snapshot();
    let target = Target::workspace(&snapshot, "w1").unwrap();
    snapshot.workspaces[0].label = "renamed".into();
    assert!(target.resolve(&snapshot).is_some());
    for pane in snapshot.panes.iter_mut().filter(|p| p.workspace_id == "w1") {
        pane.workspace_id = "w2".into();
    }
    assert!(target.resolve(&snapshot).is_none());
}

#[test]
fn reconcile_refreshes_workspace_witnesses_before_old_panes_close() {
    let mut snapshot = snapshot();
    let mut session = Session::default();
    session
        .set(
            'A',
            Target::workspace(&snapshot, "w1").unwrap(),
            "api".into(),
        )
        .unwrap();
    let mut new = snapshot.panes[0].clone();
    new.terminal_id = "new_shell".into();
    new.pane_id = "w1:p3".into();
    snapshot.panes.push(new);
    session.reconcile(&snapshot);
    snapshot.panes.retain(|p| p.terminal_id == "new_shell");
    assert!(session.marks[&'A'].target.resolve(&snapshot).is_some());
}

#[test]
fn pane_and_shell_tokens_use_the_right_sidebar_resources() {
    let snapshot = snapshot();
    let mut session = Session::default();
    session
        .set('a', Target::pane(&snapshot.panes[0]), "agent".into())
        .unwrap();
    session
        .set('b', Target::pane(&snapshot.panes[1]), "logs".into())
        .unwrap();
    session
        .set(
            'A',
            Target::workspace(&snapshot, "w1").unwrap(),
            "api".into(),
        )
        .unwrap();
    let tokens = session.tokens(&snapshot);
    assert!(
        tokens
            .iter()
            .any(|t| t.kind == Resource::Pane && t.id == "w1:p1" && t.value == "a")
    );
    assert!(
        tokens
            .iter()
            .any(|t| t.kind == Resource::Workspace && t.id == "w1" && t.value == "A b")
    );
    assert_eq!(tokens.len(), 3);
}

#[test]
fn set_reassign_and_remove_clear_old_tokens() {
    let host = FakeHost::new();
    let directory = TempDir::new().unwrap();
    host.run(&directory, Operation::Set('a', Selection::default()))
        .unwrap();
    assert_eq!(host.snapshot.borrow().panes[0].tokens[TOKEN], "a");
    host.run(
        &directory,
        Operation::Set(
            'a',
            Selection {
                pane_id: Some("w2:p1".into()),
                ..Selection::default()
            },
        ),
    )
    .unwrap();
    assert!(!host.snapshot.borrow().panes[0].tokens.contains_key(TOKEN));
    assert_eq!(host.snapshot.borrow().panes[2].tokens[TOKEN], "a");
    host.run(&directory, Operation::Remove('a')).unwrap();
    assert!(!host.snapshot.borrow().panes[2].tokens.contains_key(TOKEN));
    assert!(host.run(&directory, Operation::Remove('a')).is_err());
}

#[test]
fn sync_is_idempotent_and_preserves_unrelated_tokens() {
    let host = FakeHost::new();
    host.snapshot.borrow_mut().panes[0]
        .tokens
        .insert("summary".into(), "user work".into());
    let directory = TempDir::new().unwrap();
    host.run(&directory, Operation::Set('a', Selection::default()))
        .unwrap();
    host.calls.borrow_mut().clear();
    host.run(&directory, Operation::Sync).unwrap();
    assert!(host.calls.borrow().is_empty());
    assert_eq!(
        host.snapshot.borrow().panes[0].tokens["summary"],
        "user work"
    );
}

#[test]
fn jump_and_back_use_terminal_identity_across_workspaces() {
    let host = FakeHost::new();
    let directory = TempDir::new().unwrap();
    host.run(
        &directory,
        Operation::Set(
            'a',
            Selection {
                pane_id: Some("w2:p1".into()),
                ..Selection::default()
            },
        ),
    )
    .unwrap();
    host.run(&directory, Operation::Jump('a')).unwrap();
    assert_eq!(
        host.snapshot.borrow().focused_pane_id.as_deref(),
        Some("w2:p1")
    );
    host.run(&directory, Operation::Back).unwrap();
    assert_eq!(
        host.snapshot.borrow().focused_pane_id.as_deref(),
        Some("w1:p1")
    );
    host.run(&directory, Operation::Back).unwrap();
    assert_eq!(
        host.snapshot.borrow().focused_pane_id.as_deref(),
        Some("w2:p1")
    );
}

#[test]
fn repeated_jump_to_current_pane_preserves_back_location() {
    let host = FakeHost::new();
    let directory = TempDir::new().unwrap();
    host.run(
        &directory,
        Operation::Set(
            'a',
            Selection {
                pane_id: Some("w2:p1".into()),
                ..Selection::default()
            },
        ),
    )
    .unwrap();
    host.run(&directory, Operation::Jump('a')).unwrap();
    host.run(&directory, Operation::Jump('a')).unwrap();
    host.run(&directory, Operation::Back).unwrap();
    assert_eq!(
        host.snapshot.borrow().focused_pane_id.as_deref(),
        Some("w1:p1")
    );
}

#[test]
fn stale_mark_never_sends_focus_and_is_listed_as_stale() {
    let host = FakeHost::new();
    let directory = TempDir::new().unwrap();
    host.run(&directory, Operation::Set('a', Selection::default()))
        .unwrap();
    host.snapshot.borrow_mut().panes[0].terminal_id = "replacement".into();
    host.calls.borrow_mut().clear();
    assert!(host.run(&directory, Operation::Jump('a')).is_err());
    assert!(host.calls.borrow().is_empty());
    let listing = host
        .run(&directory, Operation::List)
        .unwrap()
        .listing
        .unwrap();
    assert_eq!(listing[0]["reachable"], false);
}

#[test]
fn captured_popup_target_cannot_mark_a_replacement_pane() {
    let host = FakeHost::new();
    let directory = TempDir::new().unwrap();
    let target = Target::pane(&host.snapshot.borrow().panes[0]);
    host.snapshot.borrow_mut().panes[0].terminal_id = "replacement".into();
    assert!(
        host.run(&directory, Operation::SetCaptured('a', target))
            .is_err()
    );
}

#[test]
fn metadata_failure_keeps_mark_for_later_repair() {
    let host = FakeHost::new();
    let directory = TempDir::new().unwrap();
    *host.fail.borrow_mut() = Some("pane.report_metadata".into());
    assert!(
        host.run(&directory, Operation::Set('a', Selection::default()))
            .is_err()
    );
    *host.fail.borrow_mut() = None;
    host.run(&directory, Operation::Sync).unwrap();
    assert_eq!(host.snapshot.borrow().panes[0].tokens[TOKEN], "a");
}

#[test]
fn failed_focus_does_not_overwrite_previous_location() {
    let host = FakeHost::new();
    let directory = TempDir::new().unwrap();
    host.run(
        &directory,
        Operation::Set(
            'a',
            Selection {
                pane_id: Some("w2:p1".into()),
                ..Selection::default()
            },
        ),
    )
    .unwrap();
    *host.fail.borrow_mut() = Some("pane.focus".into());
    assert!(host.run(&directory, Operation::Jump('a')).is_err());
    let mut store = Store::acquire(directory.path(), Duration::from_secs(1)).unwrap();
    assert!(store.session("session-one").previous.is_none());
}

#[test]
fn workspace_jump_uses_workspace_focus_not_pane_focus() {
    let host = FakeHost::new();
    let directory = TempDir::new().unwrap();
    host.run(
        &directory,
        Operation::Set(
            'A',
            Selection {
                workspace_id: Some("w2".into()),
                ..Selection::default()
            },
        ),
    )
    .unwrap();
    host.calls.borrow_mut().clear();
    host.run(&directory, Operation::Jump('A')).unwrap();
    assert_eq!(
        host.calls.borrow()[0],
        ("workspace.focus".into(), json!({"workspace_id": "w2"}))
    );
}

#[test]
fn sessions_are_separate_and_state_is_durable() {
    let directory = TempDir::new().unwrap();
    let mut store = Store::acquire(directory.path(), Duration::from_secs(1)).unwrap();
    store
        .session("socket-one")
        .set('a', Target::pane(&snapshot().panes[0]), "one".into())
        .unwrap();
    store.save().unwrap();
    drop(store);
    let mut store = Store::acquire(directory.path(), Duration::from_secs(1)).unwrap();
    assert!(store.session("socket-two").marks.is_empty());
    assert!(store.session("socket-one").marks.contains_key(&'a'));
}

#[test]
fn lock_wait_is_bounded() {
    let directory = TempDir::new().unwrap();
    let _store = Store::acquire(directory.path(), Duration::from_secs(1)).unwrap();
    assert!(Store::acquire(directory.path(), Duration::from_millis(30)).is_err());
}

#[test]
fn corrupt_or_future_state_is_preserved() {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("state.json");
    for content in ["not json", r#"{"version":2,"sessions":{}}"#] {
        std::fs::write(&path, content).unwrap();
        assert!(Store::acquire(directory.path(), Duration::from_secs(1)).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
    }
}

#[test]
fn popup_input_normalizes_only_marking_modes() {
    let key = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
    assert_eq!(input_letter(Mode::Pane, key), Some('q'));
    assert_eq!(input_letter(Mode::Workspace, key), Some('Q'));
    assert_eq!(input_letter(Mode::Jump, key), Some('q'));
    let shifted = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::SHIFT);
    assert_eq!(input_letter(Mode::Jump, shifted), Some('A'));
    assert_eq!(input_letter(Mode::Pane, shifted), Some('a'));
    let mut released = key;
    released.kind = KeyEventKind::Release;
    assert_eq!(input_letter(Mode::Jump, released), None);
    assert_eq!(
        input_letter(
            Mode::Jump,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
        ),
        None
    );
}

#[test]
fn terminal_labels_cannot_inject_controls_or_overflow_columns() {
    assert_eq!(safe_text("one\n\x1b[31mtwo", 80), "one[31mtwo");
    assert_eq!(safe_text("日本語", 5), "日本");
}

#[test]
fn all_letters_fit_metadata_limit_without_losing_marks() {
    let snapshot = snapshot();
    let mut session = Session::default();
    for key in 'a'..='z' {
        session
            .set(key, Target::pane(&snapshot.panes[1]), "logs".into())
            .unwrap();
    }
    for key in 'A'..='Z' {
        session
            .set(
                key,
                Target::workspace(&snapshot, "w1").unwrap(),
                "api".into(),
            )
            .unwrap();
    }
    let tokens = session.tokens(&snapshot);
    let space = tokens
        .iter()
        .find(|t| t.kind == Resource::Workspace)
        .unwrap();
    assert!(space.value.chars().count() <= 80);
    for key in ('a'..='z').chain('A'..='Z') {
        assert!(space.value.contains(key));
    }
    assert_eq!(
        tokens
            .iter()
            .find(|t| t.kind == Resource::Pane)
            .unwrap()
            .value,
        "a b c d e f g h i j k l m n o p q r s t u v w x y z"
    );
}

#[test]
fn concurrent_writers_do_not_lose_marks() {
    let directory = TempDir::new().unwrap();
    let mut workers = Vec::new();
    for key in 'a'..='h' {
        let path = directory.path().to_owned();
        workers.push(std::thread::spawn(move || {
            let mut store = Store::acquire(&path, Duration::from_secs(3)).unwrap();
            store
                .session("shared")
                .set(key, Target::pane(&snapshot().panes[0]), key.to_string())
                .unwrap();
            store.save().unwrap();
        }));
    }
    for worker in workers {
        worker.join().unwrap();
    }
    let mut store = Store::acquire(directory.path(), Duration::from_secs(1)).unwrap();
    assert_eq!(store.session("shared").marks.len(), 8);
}

#[test]
fn manifest_versions_and_entrypoints_match_the_binary() {
    let manifest: toml::Value = toml::from_str(include_str!("../herdr-plugin.toml")).unwrap();
    assert_eq!(
        manifest["version"].as_str().unwrap(),
        env!("CARGO_PKG_VERSION")
    );
    assert_eq!(manifest["id"].as_str().unwrap(), "herdr-marks");
    for section in ["actions", "startup", "events", "panes"] {
        for item in manifest[section].as_array().unwrap() {
            assert_eq!(
                item["command"][0].as_str().unwrap(),
                "target/release/herdr-marks"
            );
        }
    }
}
