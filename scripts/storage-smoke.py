"""Run Branch Notes against a real local host with disposable accounts/data."""
import argparse
import json
import os
from pathlib import Path
import re
import socket
import subprocess
import time
import urllib.error
import urllib.request
import uuid
import zipfile

from smoke import Browser, form, multipart


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--loki", type=Path, required=True)
    parser.add_argument("--package", type=Path, required=True)
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    data = root / "target" / ("storage-smoke-" + uuid.uuid4().hex)
    data.mkdir()
    resolver = socket.getaddrinfo
    socket.getaddrinfo = lambda host, port, *a, **kw: resolver("127.0.0.1" if isinstance(host, str) and host.endswith(".localhost") else host, port, *a, **kw)
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    origin = f"http://127.0.0.1:{port}"
    log = (data / "console.log").open("wb")

    def launch(directory):
        process = subprocess.Popen([str(args.loki.resolve()), "--data-dir", str(directory), "--bind", f"127.0.0.1:{port}", "--no-tray"], stdout=log, stderr=log, env=dict(os.environ, LOKI_LOCATION_FILE=str(data / "test-location.json"), LOKI_NO_TRAY="true"))
        for _ in range(200):
            try:
                Browser().session(origin)
                return process
            except (OSError, urllib.error.URLError):
                if process.poll() is not None:
                    raise AssertionError((data / "console.log").read_text(errors="replace"))
                time.sleep(.05)
        process.terminate()
        process.wait(15)
        raise AssertionError("Storage test server did not become ready")

    def stop(process):
        if process.poll() is None:
            process.terminate()
        try:
            process.wait(15)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()

    process = launch(data)
    try:
        owner = Browser()
        owner.session(origin)
        password = "development-owner-123"
        owner.api(origin + "/api/setup", {"token": (data / "setup-token.txt").read_text(), "username": "owner", "password": password, "name": "Storage example", "public_url": origin, "startup": False})
        owner.api(origin + "/api/login", {"username": "owner", "password": password})
        owner.session(origin)
        package = args.package.resolve()
        payload, content_type = multipart({}, [("package", package.name, "application/octet-stream", package.read_bytes())])
        assert owner.request(origin + "/api/apps/import", payload, content_type)[0] == 200
        owner.action(origin, "app-action", "storage-notes", {"operation": "enable"})
        app = next(app for app in owner.api(origin + "/api/state")["resources"] if app["id"] == "storage-notes")["url"]
        status, html, _ = owner.request(app + "/")
        assert status == 200
        csrf = re.search(rb'name="csrf_token" value="([^"]+)"', html).group(1).decode()
        assert owner.request(app + "/export")[0] == 404, "Missing file lost its typed host error"
        form(owner, app + "/preferences", {"gold": "on", "csrf_token": csrf})
        form(owner, app + "/notes", {"text": "A persistent branch — 枝", "csrf_token": csrf})
        exported, _ = form(owner, app + "/export", {"csrf_token": csrf})
        assert exported == "A persistent branch — 枝"
        with owner.opener.open(urllib.request.Request(app + "/export", headers={"Range": "bytes=0-3"}), timeout=10) as response:
            assert response.status == 206 and response.read() == b"A pe"
        state = owner.api(app + "/api/state")
        assert state["preferences"]["gold"] and len(state["notes"]) == 1
        # A second approved account has its own keys, records and export.
        member = Browser()
        member.session(origin)
        member.api(origin + "/api/register", {"username": "member", "password": "development-member-123"})
        uid = next(user["id"] for user in owner.api(origin + "/api/state")["users"] if user["username"] == "member")
        owner.action(origin, "user-status", uid, {"status": "approved"})
        owner.action(origin, "resource-access", "storage-notes", {"visibility": "members", "grants": []})
        member.api(origin + "/api/login", {"username": "member", "password": "development-member-123"})
        member.request(app + "/")
        assert member.api(app + "/api/state") == {"preferences": {"gold": False}, "notes": []}
        assert member.request(app + "/export")[0] == 404
        print("PASS SDK identity, typed JSON values, SQL notes, generated export, ranges and per-user isolation")
        owner.action(origin, "app-action", "storage-notes", {"operation": "restart"})
        assert owner.api(app + "/api/state") == state
        # A failed update snapshots and restores generated files and values too.
        broken = data / "broken.loki-app"
        with zipfile.ZipFile(package) as source, zipfile.ZipFile(broken, "w") as destination:
            manifest = json.loads(source.read("manifest.json"))
            manifest["health"] = "/missing-health"
            destination.writestr("manifest.json", json.dumps(manifest))
            for name in source.namelist():
                if name != "manifest.json":
                    destination.writestr(name, source.read(name))
        payload, content_type = multipart({}, [("package", broken.name, "application/octet-stream", broken.read_bytes())])
        assert owner.request(origin + "/api/apps/import", payload, content_type)[0] == 400
        assert owner.api(app + "/api/state") == state
        assert owner.request(app + "/export")[1].decode() == exported
        backup = owner.action(origin, "backup")["file"]
        with zipfile.ZipFile(data / "backups" / backup) as archive:
            assert any(name.startswith("app-data/storage-notes/storage/values/") for name in archive.namelist())
            assert any(name.startswith("app-data/storage-notes/storage/files/") for name in archive.namelist())
        stop(process)
        restored = data.with_name(data.name + "-restored")
        subprocess.run([str(args.loki.resolve()), "--data-dir", str(restored), "restore", str(data / "backups" / backup)], check=True, env=dict(os.environ, LOKI_LOCATION_FILE=str(data / "test-location.json")))
        process = launch(restored)
        for _ in range(200):
            status, body, _ = owner.request(app + "/api/state")
            if status == 200:
                break
            assert process.poll() is None
            time.sleep(.05)
        else:
            raise AssertionError((status, body, (data / "console.log").read_text(errors="replace")))
        assert owner.api(app + "/api/state") == state
        assert owner.request(app + "/export")[1].decode() == exported
        print("PASS restart, failed-update rollback, stopped-app backup and fresh-directory restore of JSON, files and SQL")
        owner.action(origin, "resource-access", "storage-notes", {"visibility": "restricted", "grants": []})
        assert member.request(app + "/export")[0] == 403
        print("PASS revoked grants deny existing-session generated-file downloads")
        print(restored)
    finally:
        stop(process)
        log.close()


if __name__ == "__main__":
    main()
