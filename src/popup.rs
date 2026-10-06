//! Popup owns input only while waiting. It never holds the state lock for input.

use crate::{
    api::{Client, Host, PLUGIN},
    model::{Target, safe_text},
    state::Store,
    workflow::{self, Operation, Selection},
};
use anyhow::{Context, Result, ensure};
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute, queue,
    style::{Attribute, Color, ResetColor, SetAttribute, SetForegroundColor},
    terminal::{self, Clear, ClearType},
};
use serde_json::json;
use std::{
    io::{self, Write},
    path::Path,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Pane,
    Workspace,
    Jump,
    Remove,
    List,
}

impl Mode {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "pane" => Ok(Self::Pane),
            "workspace" => Ok(Self::Workspace),
            "jump" => Ok(Self::Jump),
            "remove" => Ok(Self::Remove),
            "list" => Ok(Self::List),
            _ => anyhow::bail!("unknown popup mode {value}"),
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Pane => "pane",
            Self::Workspace => "workspace",
            Self::Jump => "jump",
            Self::Remove => "remove",
            Self::List => "list",
        }
    }
    fn heading(self) -> &'static str {
        match self {
            Self::Pane => "Mark pane: a-z",
            Self::Workspace => "Mark workspace: A-Z",
            Self::Jump | Self::List => "Jump to mark: a-z / A-Z",
            Self::Remove => "Remove mark: a-z / A-Z",
        }
    }
}

pub fn open(client: &Client, mode: Mode, selection: &Selection) -> Result<()> {
    let snapshot = client.snapshot()?;
    let mut env = json!({"HERDR_MARKS_MODE": mode.name()});
    if matches!(mode, Mode::Pane | Mode::Workspace) {
        let key = if mode == Mode::Pane { 'a' } else { 'A' };
        let target = selection.capture(key, &snapshot)?;
        env["HERDR_MARKS_TARGET"] = json!(serde_json::to_string(&target)?);
    }
    client.request(
        "plugin.pane.open",
        json!({"plugin_id": PLUGIN, "entrypoint": "prompt",
        "placement": "popup", "width": 60,
        "height": if mode == Mode::List { json!("70%") } else { json!(8) },
        "env": env}),
    )?;
    Ok(())
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        terminal::enable_raw_mode()?;
        let guard = Self;
        execute!(io::stdout(), Hide)?;
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(
            io::stdout(),
            ResetColor,
            SetAttribute(Attribute::Reset),
            Show
        );
        let _ = terminal::disable_raw_mode();
    }
}

/// Ctrl-C and Esc cancel; q remains a perfectly valid mark.
pub fn input_letter(mode: Mode, key: KeyEvent) -> Option<char> {
    if key.kind == KeyEventKind::Release
        || key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
    {
        return None;
    }
    let KeyCode::Char(c) = key.code else {
        return None;
    };
    if !c.is_ascii_alphabetic() {
        return None;
    }
    Some(match mode {
        Mode::Pane => c.to_ascii_lowercase(),
        Mode::Workspace => c.to_ascii_uppercase(),
        _ => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                c.to_ascii_uppercase()
            } else {
                c
            }
        }
    })
}

pub fn prompt(client: &Client, directory: &Path, scope: &str) -> Result<()> {
    let mode = Mode::parse(&std::env::var("HERDR_MARKS_MODE").context("missing popup mode")?)?;
    let target: Option<Target> = std::env::var("HERDR_MARKS_TARGET")
        .ok()
        .map(|s| serde_json::from_str(&s))
        .transpose()
        .context("invalid captured mark target")?;
    ensure!(
        !matches!(mode, Mode::Pane | Mode::Workspace) || target.is_some(),
        "missing captured mark target"
    );
    let snapshot = client.snapshot()?;
    let rows = {
        let mut store = Store::acquire(directory, client.timeout)?;
        let session = store.session(scope);
        session.reconcile(&snapshot);
        session
            .marks
            .iter()
            .map(|(key, mark)| PopupRow {
                letter: *key,
                reachable: mark.target.resolve(&snapshot).is_some(),
                label: safe_text(&mark.label, 80),
            })
            .collect::<Vec<_>>()
    }; // Release before waiting for the user.
    let guard = TerminalGuard::enter()?;
    let mut offset = 0usize;
    let started = Instant::now();
    draw(mode, &rows, offset)?;
    loop {
        // An abandoned popup should not consume terminal input forever.
        ensure!(
            started.elapsed() < Duration::from_secs(300),
            "marks popup timed out"
        );
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        let key = match event::read()? {
            Event::Key(key) => key,
            Event::Resize(_, _) => {
                draw(mode, &rows, offset)?;
                continue;
            }
            _ => continue,
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        if key.code == KeyCode::Esc
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            return Ok(());
        }
        match key.code {
            KeyCode::Down | KeyCode::PageDown => {
                offset = (offset + 1).min(rows.len().saturating_sub(1));
                draw(mode, &rows, offset)?;
                continue;
            }
            KeyCode::Up | KeyCode::PageUp => {
                offset = offset.saturating_sub(1);
                draw(mode, &rows, offset)?;
                continue;
            }
            _ => {}
        }
        let Some(letter) = input_letter(mode, key) else {
            continue;
        };
        let operation = match mode {
            Mode::Pane | Mode::Workspace => {
                Operation::SetCaptured(letter, target.clone().context("missing target")?)
            }
            Mode::Jump | Mode::List => Operation::Jump(letter),
            Mode::Remove => Operation::Remove(letter),
        };
        // Herdr closes the popup when this process exits. Focusing through the
        // runtime API before exit does not require a private client socket.
        drop(guard);
        let outcome = workflow::run(client, directory, scope, client.timeout, operation)?;
        if let Some(message) = outcome.message {
            let _ = client.request(
                "notification.show",
                json!({"title": "Marks", "body": message}),
            );
        }
        return Ok(());
    }
}

struct PopupRow {
    letter: char,
    reachable: bool,
    label: String,
}

fn draw_row(out: &mut impl Write, row: &PopupRow, width: usize) -> Result<()> {
    if width < 4 {
        return Ok(());
    }
    if row.reachable {
        write!(out, "  ")?;
    } else {
        queue!(out, SetForegroundColor(Color::Red))?;
        write!(out, "! ")?;
        queue!(out, ResetColor)?;
    }
    // Match the sidebar's golden mark color. Style only the letter, not its label.
    queue!(
        out,
        SetForegroundColor(Color::Rgb {
            r: 249,
            g: 226,
            b: 175
        }),
        SetAttribute(Attribute::Bold)
    )?;
    write!(out, "{}", row.letter)?;
    queue!(out, ResetColor, SetAttribute(Attribute::NormalIntensity))?;
    write!(
        out,
        " {}\r\n",
        safe_text(&row.label, width.saturating_sub(4))
    )?;
    Ok(())
}

fn draw(mode: Mode, rows: &[PopupRow], offset: usize) -> Result<()> {
    let (width, height) = terminal::size()?;
    let mut out = io::stdout();
    execute!(out, MoveTo(0, 0), Clear(ClearType::All))?;
    write!(
        out,
        "{}\r\n{}\r\n",
        safe_text(mode.heading(), width.saturating_sub(1) as usize),
        safe_text(
            "Esc cancels · ↑/↓ scroll · ! stale",
            width.saturating_sub(1) as usize
        )
    )?;
    if rows.is_empty() {
        write!(out, "No marks yet.\r\n")?;
    }
    for row in rows
        .iter()
        .skip(offset)
        .take(height.saturating_sub(3) as usize)
    {
        draw_row(&mut out, row, width.saturating_sub(1) as usize)?;
    }
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod render_tests {
    use super::*;

    #[test]
    fn marks_are_colored_letters_without_brackets_and_label_style_is_reset() {
        let mut output = Vec::new();
        draw_row(
            &mut output,
            &PopupRow {
                letter: 'b',
                reachable: true,
                label: "api · shell".into(),
            },
            60,
        )
        .unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(!text.contains("[b]"));
        assert!(text.contains("\x1b[38;2;249;226;175m\x1b[1mb"));
        assert!(text.ends_with(&format!(
            "{}{} api · shell\r\n",
            ResetColor,
            SetAttribute(Attribute::NormalIntensity)
        )));
    }

    #[test]
    fn stale_rows_keep_a_red_non_color_only_indicator() {
        let mut output = Vec::new();
        draw_row(
            &mut output,
            &PopupRow {
                letter: 'A',
                reachable: false,
                label: "gone".into(),
            },
            60,
        )
        .unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.starts_with(&format!("{}! ", SetForegroundColor(Color::Red))));
        assert!(text.contains("! "));
        assert!(!text.contains("[A]"));
    }
}
