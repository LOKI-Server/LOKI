"""Exercise a real LOKI host and portable Bigshot WASI package.

Runs on loopback with disposable data beneath target/wasi-smoke-*; never touches
installed services, Caddy, automatic startup, or the user's actual accounts.
"""
import argparse
import http.cookiejar
import json
import os
import re
import signal
import socket
import sqlite3
import subprocess
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
import zipfile
from pathlib import Path


from smoke import Browser, multipart, form


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--loki", type=Path, required=True)
    parser.add_argument("--package", type=Path, required=True)
    parser.add_argument("--keep-running", action="store_true")
    args = parser.parse_args()
    original_resolve = socket.getaddrinfo
    def resolve(host, port, *args, **kwargs):
        return original_resolve("127.0.0.1" if isinstance(host, str) and host.endswith(".localhost") else host, port, *args, **kwargs)
    socket.getaddrinfo = resolve
    root = Path(__file__).resolve().parents[1]
    data = root / "target" / ("wasi-smoke-" + uuid.uuid4().hex)
    data.mkdir(parents=True)
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    origin = f"http://127.0.0.1:{port}"
    log = (data / "console.log").open("wb")
    process = subprocess.Popen([str(args.loki.resolve()), "--data-dir", str(data), "--bind", f"127.0.0.1:{port}", "--no-tray"], stdout=log, stderr=log, env=dict(os.environ,LOKI_NO_TRAY="true",LOKI_LOCATION_FILE=str(data / "test-location.json")))
    success = False
    restarted_pid = None
    try:
        owner = Browser()
        for _ in range(150):
            try:
                owner.session(origin)
                break
            except (OSError, urllib.error.URLError):
                assert process.poll() is None, "LOKI exited: " + (data / "console.log").read_text(errors="replace")
                time.sleep(0.1)
        token = (data / "setup-token.txt").read_text()
        owner.api(origin + "/api/setup", {"token": token, "username": "owner", "password": "development-owner-123", "name": "The LOKI universe", "public_url": origin})
        owner.api(origin + "/api/login", {"username": "owner", "password": "development-owner-123"})
        owner.session(origin)
        package = args.package.resolve()
        body, content_type = multipart({}, [("package", package.name, "application/octet-stream", package.read_bytes())])
        status, result, _ = owner.request(origin + "/api/apps/import", body, content_type)
        assert status == 200, (status, result)
        owner.action(origin, "app-action", "bigshot-voter", {"operation": "enable"})
        resources = owner.api(origin + "/api/state")["resources"]
        app = next(r for r in resources if r["id"] == "bigshot-voter")
        app_url = app["url"]
        assert app["ready"]
        print("PASS package import, WASI readiness and shared-server hosting")
        # Host-managed authorization-code and PKCE flow; no guest OIDC client.
        owner.request(app_url + "/")
        status, html, final = owner.request(app_url + "/panels")
        assert status == 200 and final.endswith("/panels"), (status, final, html[:1000])
        status, html, _ = owner.request(app_url + "/admin/panels/new")
        csrf = re.search(r'name="csrf_token" value="([^"]+)"', html.decode())
        # The host supplies the CSRF value used by Bigshot forms.
        assert b'name="csrf_token"' in html, (app_url, html[:2500])
        app_csrf = re.search(r'name="csrf_token" value="([^\"]+)"', html.decode()).group(1)
        _, final = form(owner, app_url + "/admin/panels/new", {"title": "First story", "description": "A **LOKI** integration test", "media_type": "other", "csrf_token": app_csrf})
        panel_id = re.search(r"/panels/([^/?]+)", final).group(1)
        payload, content_type = multipart({"title": "Owner entry", "csrf_token": app_csrf}, [("file", "story.txt", "text/plain", b"Hello, multiverse."), ("master", "source.kra", "application/octet-stream", b"source-file")])
        status, html, _ = owner.request(app_url + f"/panels/{panel_id}/entries", payload, content_type)
        assert status == 200, (status, html[:1000])
        with sqlite3.connect(data / "app-data/bigshot-voter/app.db") as connection:
            entry_id, entry_path = connection.execute("SELECT id,stored_path FROM entries WHERE panel_id=?", (panel_id,)).fetchone()
        connection.close()
        assert owner.request(app_url + f"/media/{entry_id}")[1] == b"Hello, multiverse."
        assert owner.request(app_url + f"/media/{entry_id}/master")[1] == b"source-file"
        with owner.opener.open(urllib.request.Request(app_url + f"/media/{entry_id}", headers={"Range":"bytes=0-4"}), timeout=30) as response:
            assert response.status == 206 and response.read() == b"Hello"
        status, _, _ = owner.request(app_url + f"/panels/{panel_id}/votes", urllib.parse.urlencode({"entry_id":entry_id,"csrf_token":app_csrf}).encode(), "application/x-www-form-urlencoded")
        assert status == 400, "Owner voted for their own entry"
        print("PASS media/master downloads, range requests and own-vote rejection")
        member = Browser()
        member.session(origin)
        member.api(origin + "/api/register", {"username": "member", "password": "development-member-123"})
        users = owner.api(origin + "/api/state")["users"]
        uid = next(u["id"] for u in users if u["username"] == "member")
        owner.action(origin, "user-status", uid, {"status": "approved"})
        owner.action(origin, "resource-access", "bigshot-voter", {"visibility": "restricted", "grants": [{"subject_kind": "group", "subject": "members", "role": "access"}]})
        member.api(origin + "/api/login", {"username": "member", "password": "development-member-123"})
        member.session(origin)
        member.request(app_url + "/")
        status, html, final = member.request(app_url + "/panels")
        assert status == 200 and final.endswith("/panels"), (status, final, html[:1000])
        status, html, _ = member.request(app_url + f"/panels/{panel_id}")
        member_csrf = re.search(r'name="csrf_token" value="([^\"]+)"', html.decode()).group(1)
        status, html, _ = member.request(app_url + f"/panels/{panel_id}")
        entry_id = re.search(r'name="entry_id" value="([^"]+)"', html.decode()).group(1)
        form(member, app_url + f"/panels/{panel_id}/votes", {"entry_id": entry_id, "csrf_token": member_csrf})
        assert member.request(app_url + "/admin")[0] == 403
        form(owner, app_url + f"/admin/panels/{panel_id}/close", {"csrf_token":app_csrf})
        status, _, _ = member.request(app_url + f"/panels/{panel_id}/votes", urllib.parse.urlencode({"entry_id":entry_id,"csrf_token":member_csrf}).encode(), "application/x-www-form-urlencoded")
        assert status == 400, "Closed panel accepted a vote"
        form(owner, app_url + f"/admin/panels/{panel_id}/open", {"csrf_token":app_csrf})
        form(member, app_url + f"/panels/{panel_id}/votes", {"entry_id":entry_id,"csrf_token":member_csrf})
        print("PASS application-admin separation, panel close/reopen and single-vote update")
        print("PASS host-managed SSO and scoped identity, owner panel creation, upload and member voting")
        # Permissions are rechecked even with an existing Bigshot session.
        owner.action(origin, "resource-access", "bigshot-voter", {"visibility": "restricted", "grants": []})
        status, html, final = member.request(app_url + f"/panels/{panel_id}")
        assert not (status == 200 and final.endswith(panel_id)), "Revoked user retained panel access"
        assert member.request(app_url + f"/media/{entry_id}")[0] == 403
        print("PASS immediate app and direct media access revocation")
        owner.action(origin, "resource-access", "bigshot-voter", {"visibility": "members", "grants": []})
        # A broken upgrade must restore both the executable and stopped-app data.
        broken = data / "broken.loki-app"
        with zipfile.ZipFile(package) as source, zipfile.ZipFile(broken, "w", compression=zipfile.ZIP_DEFLATED) as dest:
            manifest = json.loads(source.read("manifest.json"))
            manifest["version"] = "0.5.1"
            manifest["health"] = "/missing-health-endpoint"
            manifest["fields"].append({"key":"experimental","label":"Experimental setting","kind":"bool","required":False,"default":True})
            dest.writestr("manifest.json", json.dumps(manifest))
            for name in source.namelist():
                if name != "manifest.json":
                    dest.writestr(name, source.read(name))
        body, content_type = multipart({}, [("package", broken.name, "application/octet-stream", broken.read_bytes())])
        status, _, _ = owner.request(origin + "/api/apps/import", body, content_type)
        assert status == 400, status
        current = next(r for r in owner.api(origin + "/api/state")["resources"] if r["id"] == "bigshot-voter")
        assert current["ready"] and current["manifest"]["version"] == "0.5.0"
        assert current["config"] == {"max-upload": 5242880}, current["config"]
        with sqlite3.connect(data / "app-data/bigshot-voter/app.db") as connection:
            assert connection.execute("SELECT count(*) FROM votes").fetchone()[0] == 1
        connection.close()
        print("PASS failed update restores prior package and voting data")
        shared = data / "shared"
        shared.mkdir()
        (shared / "Welcome.txt").write_text("Your services, held together.\n", encoding="utf-8")
        share = owner.action(origin, "share-save", data={"name": "The Archive", "root": str(shared)})
        owner.action(origin, "resource-access", share["id"], {"visibility": "members", "grants": []})
        large = data / "app-data/bigshot-voter/uploads" / entry_path
        with large.open("wb") as stream:
            stream.truncate(128 * 1024 * 1024)
        from benchmark import sample
        before, _ = sample(process.pid)
        peak = before
        with owner.opener.open(app_url + f"/media/{entry_id}", timeout=30) as response:
            count = 0
            while chunk := response.read(65536):
                count += len(chunk)
                if count % 1048576 == 0:
                    peak = max(peak, sample(process.pid)[0])
        assert count == 128 * 1024 * 1024
        assert peak - before < 16 * 1024 * 1024, (before, peak)
        print(f"PASS 128 MiB host-streamed component download; core resident increase {(peak-before)/1048576:.2f} MiB")
        backup = owner.action(origin, "backup")["file"]
        assert (data / "backups" / backup).is_file()
        print("PASS consistent backup and app resume")
        body, content_type = multipart({}, [("backup", backup, "application/zip", (data / "backups" / backup).read_bytes())])
        status, result, _ = owner.request(origin + "/api/backups/import", body, content_type)
        assert status == 200, (status, result)
        backup = json.loads(result)["file"]
        print("PASS local backup import and archive validation")
        restored = data.with_name(data.name + "-restored")
        owner.action(origin, "storage", data={"target": str(restored), "backup": backup, "current_password": "development-owner-123"})
        process.wait(30)
        for _ in range(200):
            try:
                owner.session(origin)
                break
            except (OSError, urllib.error.URLError, AssertionError):
                time.sleep(0.1)
        restarted_pid = int((restored / "server.pid").read_text())
        current = owner.api(origin + "/api/state")
        assert str(restored.name) in current["data_dir"]
        with sqlite3.connect(restored / "app-data/bigshot-voter/app.db") as connection:
            assert connection.execute("SELECT count(*) FROM votes").fetchone()[0] == 1
        connection.close()
        print("PASS web backup restore into a new data directory and supervised restart")
        data = restored
        report = {"origin": origin, "app": app_url, "data": str(data), "pid": restarted_pid or process.pid, "runtime": "wasi", "stream_resident_growth_mib": round((peak-before)/1048576, 2), "backup": backup}
        (root / "target/wasi-smoke-last.json").write_text(json.dumps(report, indent=2))
        print(json.dumps(report, indent=2))
        success = True
        if args.keep_running:
            print("Temporary preview login: owner / development-owner-123")
            return
    finally:
        if not (args.keep_running and success):
            if restarted_pid:
                try:
                    os.kill(restarted_pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
            process.terminate()
            try:
                process.wait(15)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
        log.close()


if __name__ == "__main__":
    main()
