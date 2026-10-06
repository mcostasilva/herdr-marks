# Herdr Marks

**Put a letter on a pane or workspace. Jump back to it. See the letter in the sidebar.**

- **a–z** mark individual panes, including shells and editors.
- **A–Z** mark workspaces.
- Reusing a letter moves its mark; several letters can point to one target.
- Jumping to a pane switches to its workspace and tab without changing zoom.
- Marks are scoped to the session's API socket, not shared across named sessions.

Requires **Herdr 0.9.3+**, **Rust/Cargo 1.89+**, and macOS or Linux.

## Install

```sh
herdr plugin install mcostasilva/herdr-marks
```

For local development, build and link your checkout instead:

```sh
cargo build --release --locked --target-dir target
herdr plugin link "$PWD"
```

Add bindings to `~/.config/herdr/config.toml`. These are suggested keys; check
your current bindings for conflicts first. Plugins never claim keys themselves.

```toml
[[keys.command]]
key = "prefix+m"
type = "plugin_action"
command = "herdr-marks.mark-pane"
description = "mark pane with a letter"

[[keys.command]]
key = "prefix+shift+m"
type = "plugin_action"
command = "herdr-marks.mark-workspace"
description = "mark workspace with a letter"

[[keys.command]]
key = "prefix+quote"
type = "plugin_action"
command = "herdr-marks.jump"
description = "jump to mark"

[[keys.command]]
key = "prefix+u"
type = "plugin_action"
command = "herdr-marks.remove"
description = "remove mark"

[[keys.command]]
key = "prefix+backtick"
type = "plugin_action"
command = "herdr-marks.back"
description = "jump back"
```

With the default prefix, **Ctrl+B → m → a** marks the pane. **Ctrl+B → M → a**
marks the workspace as **A** (the workspace prompt uppercases your letter).
**Ctrl+B → ' → a** jumps to pane **a**; uppercase **A** jumps to workspace **A**.
The small popup captures the final letter, so ordinary typing is unaffected.
Its mark letters use the same bold golden color as the sidebar, without brackets.
**Ctrl+B → '** lists marks and jumps; **Ctrl+B → u → letter** removes one. No mark shortcut
uses Alt, so these bindings do not conflict with Alt-based window managers.
**Esc** or **Ctrl+C** cancels. **q** is a valid mark, not a quit key.

## Sidebar

Add `$marks` to your existing row layouts rather than replacing unrelated
customizations. For the standard layout:

```toml
[ui.sidebar.spaces]
rows = [
  ["state_icon", { token = "$marks", fg = "#f9e2af", bold = true }, "workspace"],
  ["branch", "git_status"],
]

[ui.sidebar.agents]
rows = [
  ["state_icon", "machine", "workspace", "tab"],
  [{ token = "$marks", fg = "#f9e2af", bold = true }, "pane"],
]
```

Agent marks appear beside the pane name on the second line. Workspace marks
stay on the first line of Spaces. Agent-specific `rows_by_agent` overrides
replace the default layout; add `$marks` to their second line too if you use
them. Empty tokens disappear. The plugin reserves the
custom token `marks`; other metadata is untouched.

```text
Spaces                          Agents
  ● A api                         ● api · tests
  ● B frontend                        a OpenCode
  ● c services                    ● frontend
                                      b Claude
```

Ordinary shell/editor panes have no Agent row. Their marks appear on their
workspace's Space row instead, such as `c`. A pane that later becomes an
agent gets its own mark label there. No pane/workspace names are changed.
Tokens appear in the **expanded desktop sidebar**; Herdr's collapsed/mobile
layouts do not render custom tokens.
When many marks share a row, labels compact to `Abc` to keep every letter
within Herdr's 80-character metadata limit. The picker retains full labels.

After editing your config:

```sh
herdr config check
herdr server reload-config
herdr plugin action invoke herdr-marks.sync
```

Linking/enabling a plugin does not execute its startup hook. The explicit sync
refreshes an already-running server without a restart.

## CLI

Inside a Herdr pane:

```sh
./target/release/herdr-marks set a                  # calling pane
./target/release/herdr-marks set A                  # calling workspace
./target/release/herdr-marks set b --pane w2:p3
./target/release/herdr-marks set B --workspace w2
./target/release/herdr-marks jump a
./target/release/herdr-marks back
./target/release/herdr-marks remove a
./target/release/herdr-marks list --json
./target/release/herdr-marks sync
```

Plugin actions respect the supplied invocation context. The mark popup captures
the target when opened, so changing focus while it is open cannot mark a different
pane. CLI commands prefer the caller's Herdr IDs; jumps store the actual focused
pane as the return location.

## Identity, persistence, and recovery

State lives at `$HERDR_PLUGIN_STATE_DIR/state.json`, normally
`~/.local/state/herdr/plugins/herdr-marks/state.json`. Direct CLI use falls back
to the same XDG state path. Entries are separated by the canonical API socket
path. All updates share a bounded file lock and use atomic file replacement.

A pane mark records **terminal identity**, not a recycled pane ID. It follows
that terminal when moved across tabs or workspaces. A workspace mark records its
workspace ID and live terminal witnesses; a rename or reorder does not invalidate
it, but moving its panes elsewhere does not move the workspace mark.

Pane marks are automatically deleted when their terminal disappears. Closing a
pane, tab, or workspace triggers a sync; sync and listing commands also clean up
missing terminals. Opening the picker performs and saves the same cleanup before
waiting for input, even if you cancel. A failed snapshot leaves saved marks
untouched. Moving a pane or exiting an agent without closing its pane preserves
the marks. Uppercase workspace marks are not automatically deleted.

Marks survive detach/reconnect and a handoff **when terminal identities are
preserved**. A full restart that recreates terminals removes old pane marks on
the next cleanup. The plugin deliberately does **not** match by folder, label,
or agent name: two similar panes must never silently exchange your marks.
Re-mark restored targets explicitly. Stale workspace marks and ambiguous pane
identities remain in the list with `!`, and jumping to one fails.

Startup and lifecycle hooks republish tokens. If a metadata update fails, the
mark remains saved; run `sync` to repair the display. Detached action errors are
also reported as Herdr notifications. Logs are available through:

```sh
herdr plugin log list --plugin herdr-marks --limit 5
```

No daemon, polling process, shell hooks, or Herdr patches are needed.

## Development

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --release --locked
python3 scripts/test-popup.py
```

Tests use pure snapshots and disposable API sockets, not your real Herdr session.
The optional Python 3 test drives the actual popup in a disposable terminal.
The API transport targets local Unix sockets, including a server reached by
SSH when this plugin runs on that server. Windows is not supported yet.

## Unlink

Remove the bindings and `$marks` sidebar entries, reload your config, then:

```sh
herdr plugin unlink herdr-marks
```

Saved marks are left in place.

## License

Licensed under the [MIT License](LICENSE).
