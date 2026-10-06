#!/usr/bin/env python3
"""Run the real popup in a disposable PTY against a fake Herdr API."""

import copy
import errno
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import signal
import socket
import struct
import tempfile
import termios
import threading
import time
import unittest


BINARY = Path(__file__).resolve().parents[1] / "target/release/herdr-marks"
SNAPSHOT = {
    "focused_pane_id": "w1:p1",
    "focused_workspace_id": "w1",
    "panes": [
        {
            "pane_id": "w1:p1",
            "terminal_id": "term_one",
            "workspace_id": "w1",
            "tab_id": "w1:t1",
            "agent": "opencode",
        },
    ],
    "workspaces": [{"workspace_id": "w1", "label": "api"}],
}


class PopupTests(unittest.TestCase):
    def run_popup(
        self, mode, key, target=None, saved_marks=None, initial_snapshot=None
    ):
        with tempfile.TemporaryDirectory(prefix="herdr-marks-popup-") as directory:
            root = Path(directory)
            listener = socket.socket(socket.AF_UNIX)
            listener.bind(str(root / "api.sock"))
            listener.listen()
            listener.settimeout(0.1)
            calls = []
            errors = []
            stop = threading.Event()
            snapshot = copy.deepcopy(
                SNAPSHOT if initial_snapshot is None else initial_snapshot
            )
            if saved_marks is not None:
                (root / "state").mkdir()
                (root / "state/state.json").write_text(
                    json.dumps(
                        {
                            "version": 1,
                            "sessions": {
                                str((root / "api.sock").resolve()): {
                                    "marks": saved_marks,
                                    "previous": None,
                                }
                            },
                        }
                    )
                )

            def serve():
                while not stop.is_set():
                    try:
                        connection, _ = listener.accept()
                    except socket.timeout:
                        continue
                    try:
                        with connection:
                            connection.settimeout(2)
                            request = json.loads(connection.makefile("rb").readline())
                            calls.append(request)
                            method = request["method"]
                            params = request["params"]
                            result = {"type": "ok"}
                            if method == "session.snapshot":
                                result = {"snapshot": snapshot}
                            elif method in (
                                "pane.report_metadata",
                                "workspace.report_metadata",
                            ):
                                resources = snapshot[
                                    "panes"
                                    if method.startswith("pane.")
                                    else "workspaces"
                                ]
                                id_key = (
                                    "pane_id"
                                    if method.startswith("pane.")
                                    else "workspace_id"
                                )
                                resource = next(
                                    r for r in resources if r[id_key] == params[id_key]
                                )
                                resource.setdefault("tokens", {}).update(
                                    params["tokens"]
                                )
                            elif method != "notification.show":
                                raise AssertionError(f"Unexpected method: {method}")
                            connection.sendall(
                                (
                                    json.dumps({"id": request["id"], "result": result})
                                    + "\n"
                                ).encode()
                            )
                    except Exception as error:
                        errors.append(error)

            server = threading.Thread(target=serve)
            pid, master = pty.fork()
            if pid == 0:
                env = dict(os.environ)
                for name in list(env):
                    if name.startswith("HERDR_"):
                        del env[name]
                env.update(
                    {
                        "HERDR_ENV": "1",
                        "HERDR_SOCKET_PATH": str(root / "api.sock"),
                        "HERDR_PLUGIN_STATE_DIR": str(root / "state"),
                        "HERDR_MARKS_MODE": mode,
                        "TERM": "xterm-256color",
                    }
                )
                if target is not None:
                    env["HERDR_MARKS_TARGET"] = json.dumps(target)
                os.execve(BINARY, [str(BINARY), "prompt"], env)
            server.start()
            output = bytearray()
            status = None
            try:
                fcntl.ioctl(
                    master, termios.TIOCSWINSZ, struct.pack("HHHH", 20, 80, 0, 0)
                )
                deadline = time.monotonic() + 10
                sent = False
                while time.monotonic() < deadline:
                    ready, _, _ = select.select([master], [], [], 0.05)
                    if ready:
                        try:
                            chunk = os.read(master, 4096)
                        except OSError as error:
                            if error.errno != errno.EIO:
                                raise
                            chunk = b""
                        output.extend(chunk)
                        if not sent and b"Esc cancels" in output:
                            os.write(master, key)
                            sent = True
                    waited, child_status = os.waitpid(pid, os.WNOHANG)
                    if waited:
                        status = child_status
                        # Capture the final rendered rows even when the child
                        # exits between reading the header and the next PTY read.
                        while select.select([master], [], [], 0)[0]:
                            try:
                                chunk = os.read(master, 4096)
                            except OSError as error:
                                if error.errno != errno.EIO:
                                    raise
                                break
                            if not chunk:
                                break
                            output.extend(chunk)
                        break
                self.assertIsNotNone(status, f"Popup hung: {output!r}")
                self.assertEqual(
                    os.waitstatus_to_exitcode(status),
                    0,
                    output.decode(errors="replace"),
                )
                self.assertTrue(sent, output.decode(errors="replace"))
                self.assertFalse(errors, errors)
                state_file = root / "state/state.json"
                state = (
                    json.loads(state_file.read_text()) if state_file.exists() else None
                )
                return calls, state, output
            finally:
                if status is None:
                    os.kill(pid, signal.SIGKILL)
                    os.waitpid(pid, 0)
                os.close(master)
                stop.set()
                server.join(timeout=3)
                listener.close()

    def test_escape_cancels_without_publishing_or_saving(self):
        calls, state, _ = self.run_popup("jump", b"\x1b")
        self.assertEqual([call["method"] for call in calls], ["session.snapshot"])
        self.assertIsNone(state)

    def test_control_c_cancels(self):
        calls, state, _ = self.run_popup("jump", b"\x03")
        self.assertEqual([call["method"] for call in calls], ["session.snapshot"])
        self.assertIsNone(state)

    def test_q_marks_a_pane_instead_of_quitting(self):
        calls, state, _ = self.run_popup(
            "pane", b"q", {"kind": "pane", "terminal_id": "term_one"}
        )
        report = next(c for c in calls if c["method"] == "pane.report_metadata")
        self.assertEqual(report["params"]["tokens"], {"marks": "q"})
        self.assertIn("q", next(iter(state["sessions"].values()))["marks"])

    def test_workspace_prompt_uppercases_the_letter(self):
        calls, state, _ = self.run_popup(
            "workspace",
            b"a",
            {
                "kind": "workspace",
                "workspace_id": "w1",
                "witnesses": ["term_one"],
            },
        )
        report = next(c for c in calls if c["method"] == "workspace.report_metadata")
        self.assertEqual(report["params"]["tokens"], {"marks": "A"})
        self.assertIn("A", next(iter(state["sessions"].values()))["marks"])

    def test_jump_panel_uses_colored_letters_without_brackets(self):
        _, _, output = self.run_popup(
            "jump",
            b"\x1b",
            saved_marks={
                "b": {
                    "target": {"kind": "pane", "terminal_id": "term_one"},
                    "label": "api",
                },
            },
        )
        self.assertNotIn(b"[b]", output)
        self.assertIn(b"\x1b[38;2;249;226;175m\x1b[1mb", output)

    def test_cancelled_picker_saves_cleanup_and_clears_shell_rollup(self):
        snapshot = copy.deepcopy(SNAPSHOT)
        snapshot["workspaces"][0]["tokens"] = {"marks": "c", "summary": "keep"}
        calls, state, output = self.run_popup(
            "jump",
            b"\x1b",
            initial_snapshot=snapshot,
            saved_marks={
                "b": {
                    "target": {"kind": "pane", "terminal_id": "term_one"},
                    "label": "api",
                },
                "c": {
                    "target": {"kind": "pane", "terminal_id": "closed_shell"},
                    "label": "closed shell",
                },
                "A": {
                    "target": {
                        "kind": "workspace",
                        "workspace_id": "gone",
                        "witnesses": ["gone"],
                    },
                    "label": "old workspace",
                },
            },
        )
        marks = next(iter(state["sessions"].values()))["marks"]
        self.assertEqual(set(marks), {"A", "b"})
        self.assertNotIn(b"closed shell", output)
        self.assertIn(b"old workspace", output)
        report = next(c for c in calls if c["method"] == "workspace.report_metadata")
        self.assertEqual(report["params"]["tokens"], {"marks": None})
        self.assertFalse(any(c["method"].endswith(".focus") for c in calls))


if __name__ == "__main__":
    if not BINARY.exists():
        raise SystemExit("Build first: cargo build --release --locked")
    unittest.main()
