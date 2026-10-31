"""Exercise cargo-loki and an SDK-only plugin with disposable local data."""
import argparse
import json
import os
from pathlib import Path
import re
import signal
import socket
import subprocess
import time
import uuid

from smoke import Browser


def wait_for(check, process, log, seconds=90):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        result = check()
        if result:
            return result
        assert process.poll() is None, log.read_text(errors="replace")
        time.sleep(0.1)
    raise AssertionError("Timed out: " + log.read_text(errors="replace"))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--tool", type=Path, required=True)
    parser.add_argument("--server", type=Path, required=True)
    parser.add_argument("--offline", action="store_true")
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    project = root / "target" / ("sdk-smoke-" + uuid.uuid4().hex)
    env = dict(os.environ, CARGO_TARGET_DIR=str(project / "target"))
    tool = [str(args.tool.resolve()), "loki"]
    subprocess.run(tool + ["new", "sdk-smoke", "--path", str(project), "--sdk-path", str(root / "crates/loki-sdk")], check=True, env=env)
    options = ["--manifest-path", str(project / "Cargo.toml")] + (["--offline"] if args.offline else [])
    subprocess.run(tool + ["check"] + options, check=True, env=env)
    subprocess.run(tool + ["package"] + options, check=True, env=env)
    assert (project / "target/loki/sdk-smoke-0.1.0.loki-app").is_file()
    resolver = socket.getaddrinfo
    socket.getaddrinfo = lambda host, port, *a, **kw: resolver("127.0.0.1" if isinstance(host, str) and host.endswith(".localhost") else host, port, *a, **kw)
    log = project / "preview.log"

    def launch():
        output = log.open("wb")
        process = subprocess.Popen(tool + ["dev"] + options + ["--server", str(args.server.resolve()), "--port", "0"], stdout=output, stderr=output, env=env, creationflags=subprocess.CREATE_NEW_PROCESS_GROUP if os.name == "nt" else 0)
        return process, output

    def stop(process):
        if process.poll() is None:
            process.send_signal(signal.CTRL_BREAK_EVENT if os.name == "nt" else signal.SIGINT)
            process.wait(timeout=15)

    origin = None
    for run in range(2):
        process, output = launch()
        try:
            text = wait_for(lambda: (s if "Plugin: " in (s := log.read_text(errors="replace")) else None), process, log)
            dashboard = re.search(r"Dashboard: (\S+)", text).group(1)
            plugin = re.search(r"Plugin: (\S+)", text).group(1)
            if origin:
                assert dashboard == origin, "Restart changed the persisted identity issuer"
            origin = dashboard
            profile = project / "target/loki/dev/sdk-smoke"
            credentials = json.loads((profile / "cargo-loki-dev.json").read_text())
            server_pid = int((profile / "server.pid").read_text())
            browser = Browser()
            browser.session(dashboard)
            browser.api(dashboard + "/api/login", {"username": credentials["username"], "password": credentials["password"]})
            status, body, _ = browser.request(plugin + "/")
            assert status == 200 and b"LOKI plugin" in body
            assert browser.request(plugin + "/static/style.css")[0] == 200
            if run == 0:
                source = project / "src/lib.rs"
                original = source.read_text()
                updated = original.replace("Your LOKI plugin is running.", "Your LOKI plugin rebuilt successfully.")
                source.write_text(updated)
                wait_for(lambda: "Updated " in log.read_text(errors="replace"), process, log)
                assert b"rebuilt successfully" in browser.request(plugin + "/")[1]
                source.write_text(updated + "\nthis is invalid Rust\n")
                wait_for(lambda: "Update failed" in log.read_text(errors="replace"), process, log)
                assert b"rebuilt successfully" in browser.request(plugin + "/")[1]
                source.write_text(original)
                wait_for(lambda: log.read_text(errors="replace").count("Updated ") >= 2, process, log)
            print(f"PASS SDK component, host SSO, assets and persistent dev profile (run {run + 1})")
        finally:
            stop(process)
            output.close()
        # The dev server must have stopped with its parent, including on Windows.
        with socket.socket() as probe:
            assert probe.connect_ex(("127.0.0.1", int(dashboard.rsplit(":", 1)[1]))) != 0, f"Preview server {server_pid} still listening after Ctrl+C"
    print("PASS watched rebuild, failed-build preservation, stable port and Ctrl+C cleanup")
    print(project)


if __name__ == "__main__":
    main()
