"""Verify first setup, HTTPS/local cookies, data relocation, and restart in isolation."""
import argparse
import json
import os
import signal
import socket
import subprocess
import time
import uuid
from pathlib import Path
from smoke import Browser


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("binary", type=Path)
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1] / "target" / ("setup-smoke-" + uuid.uuid4().hex)
    source, selected = root / "source", root / "selected"
    source.mkdir(parents=True)
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    origin = f"http://127.0.0.1:{port}"
    restarted_pid = None
    with (root / "console.log").open("wb") as log:
        process = subprocess.Popen([str(args.binary.resolve()), "--data-dir", str(source), "--bind", f"127.0.0.1:{port}", "--no-tray"], stdout=log, stderr=log, env=dict(os.environ, LOKI_NO_TRAY="true", LOKI_LOCATION_FILE=str(root / "location.json")))
        try:
            owner = Browser()
            for _ in range(150):
                try:
                    owner.session(origin)
                    break
                except OSError:
                    assert process.poll() is None, "Server exited during setup"
                    time.sleep(0.1)
            token = (source / "setup-token.txt").read_text()
            input = {"token": token, "username": "owner", "password": "setup-owner-password", "name": "Setup test", "public_url": "https://loki.example.test", "data_dir": str(selected), "startup": False}
            result = owner.api(origin + "/api/setup", input)
            assert result.get("restarting"), result
            process.wait(30)
            for _ in range(200):
                try:
                    state = owner.session(origin)
                    break
                except (OSError, AssertionError):
                    time.sleep(0.1)
            else:
                raise AssertionError("Relocated server did not become ready")
            restarted_pid = int((selected / "server.pid").read_text())
            assert state["setup"] is False
            owner.api(origin + "/api/login", {"username": "owner", "password": "setup-owner-password"})
            assert owner.session(origin)["user"]["owner"]
            assert selected.name in owner.api(origin + "/api/state")["data_dir"]
            status, body, _ = owner.request(origin + "/api/setup", input)
            assert status == 409 and "already set up" in json.loads(body)["error"], (status, body)
            assert (source / "loki.db").is_file() and (selected / "loki.db").is_file()
            assert not (selected / "setup-token.txt").exists()
            print("PASS setup with custom directory, preserved source, supervised restart, HTTPS public URL with local sign-in, and rejection of repeated setup")
        finally:
            if restarted_pid is None and (selected / "server.pid").exists():
                restarted_pid = int((selected / "server.pid").read_text())
            if restarted_pid:
                try:
                    os.kill(restarted_pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
            process.terminate()
            process.wait(15)


if __name__ == "__main__":
    main()
