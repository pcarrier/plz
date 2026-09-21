"""Exercise the real Fish binding and CLI in a PTY, checking rendered output."""

import json
import os
from pathlib import Path
import socket
import sys
import tempfile
import threading

import pexpect
import pyte


class Display:
    def __init__(self):
        self.screen = pyte.Screen(100, 40)
        self.stream = pyte.Stream(self.screen)

    def write(self, data):
        self.stream.feed(data)

    def flush(self):
        pass

    def text(self):
        return "\n".join(line.rstrip() for line in self.screen.display)


def settle(child):
    # Consume the rest of a repaint before checking the terminal's screen.
    try:
        while True:
            child.read_nonblocking(4096, timeout=0.2)
    except pexpect.TIMEOUT:
        pass


def check(binary, *, newlines=0, multiline_prompt=False, answer="n", error=False):
    with tempfile.TemporaryDirectory(prefix="plz-fish-") as tmp:
        root = Path(tmp)
        (root / "state").mkdir()
        (root / "bin").mkdir()
        (root / "bin/plz").symlink_to(binary)
        requests = []
        errors = []
        with socket.socket(socket.AF_UNIX) as listener:
            listener.bind(str(root / "state/socket"))
            listener.listen()
            listener.settimeout(10)

            def respond():
                # Isolate the terminal test from the API, cache, and user daemon.
                replies = ["deny"] if error else ["confirm", "allow" if answer in ("O", "A") else "deny"]
                try:
                    for verdict in replies:
                        connection, _ = listener.accept()
                        with connection, connection.makefile("rb") as reader:
                            requests.append(json.loads(reader.readline()))
                            reply = dict(
                                verdict=verdict,
                                error="test failure" if error else None,
                                cache_hit=False,
                                prompt_supported=True,
                            )
                            connection.sendall((json.dumps(reply) + "\n").encode())
                except Exception as exception:
                    errors.append(exception)

            worker = threading.Thread(target=respond, daemon=True)
            worker.start()
            env = dict(
                os.environ,
                HOME=tmp,
                XDG_CONFIG_HOME=str(root / "config"),
                XDG_DATA_HOME=str(root / "data"),
                PLZ_STATE_DIR=str(root / "state"),
                TYPESAFE_API_KEY="",
                TERM="xterm-256color",
                FISH_TEST_NO_RECURRENT_QUERIES="1",
                PATH=str(root / "bin") + ":" + os.environ["PATH"],
            )
            prompt = "context\\nPROMPT> " if multiline_prompt else "PROMPT> "
            child = pexpect.spawn(
                "fish",
                [
                    "--no-config", "--interactive", "--init-command",
                    "set -g fish_greeting; set -g fish_autosuggestion_enabled 0; "
                    f"function fish_prompt; printf '{prompt}'; end; "
                    "plz init fish | source; "
                    # Match the user's Enter-for-newlines setup. Ctrl-X invokes
                    # the same function as Ctrl-Enter without terminal-specific encoding.
                    "bind enter 'commandline --insert \\n' repaint; "
                    "bind ctrl-x __plz_execute; "
                    "bind ctrl-g 'commandline --current-buffer > buffer; commandline --cursor > cursor'; "
                    "bind ctrl-y 'commandline --replace \"plz status\"; commandline -f repaint'",
                ],
                cwd=tmp, env=env, encoding="utf-8", timeout=10, dimensions=(40, 100),
            )
            display = Display()
            child.logfile_read = display
            try:
                # Reply to Fish's startup terminal query (as Fish's own PTY tests do).
                child.send("\x1b[?123c")
                child.expect_exact("PROMPT> ")
                child.send("touch executed" + "\r" * newlines + "\x18")
                if not error:
                    child.expect_exact("anything else for no: ")
                    assert not (root / "executed").exists()
                    child.send(answer + "\r")
                allowed = not error and answer in ("O", "A")
                if not allowed:
                    child.expect_exact("command not run")
                child.expect_exact("PROMPT> ")
                settle(child)
                assert (root / "executed").exists() == allowed
                if not allowed:
                    text = display.text()
                    message = "plz: test failure; command not run" if error else "plz: command not run"
                    assert message in text, text
                    if not error:
                        assert "plz confirm" in text, text
                        assert f"anything else for no: {answer}".rstrip() in text, text
                    # Denial must retain the exact editable buffer and cursor.
                    child.send("\x07")
                    settle(child)
                    command = "touch executed" + "\n" * newlines
                    assert (root / "buffer").read_text() == command + "\n"
                    assert int((root / "cursor").read_text()) == len(command)
                    # The transcript must survive replacement with the next command too.
                    child.send("\x19")
                    child.expect_exact("plz status")
                    settle(child)
                    assert message in display.text(), display.text()
                worker.join(timeout=2)
                assert not worker.is_alive()
                assert not errors, errors
                assert requests[0]["context"]["command"] == "touch executed" + "\n" * (newlines + 1)
                if not error:
                    expected = {"O": "once", "A": "always"}.get(answer, "no")
                    assert requests[1]["answer"] == expected
            finally:
                child.close(force=True)


binary = Path(sys.argv[1]).resolve()
for multiline_prompt in (False, True):
    for newlines in (0, 1, 2):
        check(binary, newlines=newlines, multiline_prompt=multiline_prompt)
check(binary, newlines=2, answer="")
check(binary, newlines=2, error=True)
check(binary, newlines=2, answer="O")
check(binary, newlines=2, answer="A")
print("Fish PTY checks passed (denial, editable buffer, errors, Once, Always).")
