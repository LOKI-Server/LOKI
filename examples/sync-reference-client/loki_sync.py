#!/usr/bin/env python3
"""LOKI save sync reference client (standard library only). See docs/sync-api.md."""
import argparse, getpass, hashlib, json, os, platform, sys, time, unicodedata, urllib.error, urllib.request

CONFIG = os.path.expanduser("~/.loki-sync.json")  # server origin -> device token
INDEX, TMP, SETTLE = ".loki-sync-index.json", ".loki-tmp", 3
OCTET = "application/octet-stream"

class ApiError(Exception):
    def __init__(self, status, body):
        self.status, self.code = status, body.get("code")
        super().__init__(f"HTTP {status} {self.code or ''} {body.get('error', '')}".strip())

class Client:
    def __init__(self, server, token=None):
        self.base, self.token = server.rstrip("/") + "/sync/v1", token

    def call(self, method, path, body=None, data=None, headers=None, timeout=60):
        headers = dict(headers or {})
        if body is not None:
            data, headers["Content-Type"] = json.dumps(body).encode(), "application/json"
        if self.token:
            headers["Authorization"] = "Bearer " + self.token
        try:
            return urllib.request.urlopen(urllib.request.Request(
                self.base + path, data=data, method=method, headers=headers), timeout=timeout)
        except urllib.error.HTTPError as e:
            raw = e.read()
            raise ApiError(e.code, json.loads(raw) if raw.startswith(b"{") else {}) from None

    def api(self, method, path, body=None, timeout=60):
        with self.call(method, path, body, timeout=timeout) as r:
            raw = r.read()
        return json.loads(raw) if raw else None

def load_json(path, default):
    try:
        with open(path, encoding="utf-8") as f:
            return json.load(f)
    except FileNotFoundError:
        return default

def save_json(path, value, mode=0o644):
    tmp = path + TMP  # temp file + rename: a crash never leaves half a file
    with os.fdopen(os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, mode), "w", encoding="utf-8") as f:
        json.dump(value, f, indent=1, sort_keys=True)
    os.replace(tmp, path)

def signed_in(args, session):
    config = load_json(CONFIG, {})
    config[args.server.rstrip("/")] = session["token"]
    save_json(CONFIG, config, 0o600)
    print(f"Signed in as {session['user']['username']}; device {session['device']['name']}.")

def device(args):
    return {"name": args.name, "platform": {"win32": "windows", "darwin": "macos"}.get(sys.platform, sys.platform)}

def pair(args):
    c = Client(args.server)
    info = c.api("GET", "")
    if info.get("protocol") != 1 or info.get("hash") != "sha256":
        sys.exit(f"Unsupported server: {info}")
    p = c.api("POST", "/pair", device(args))
    print(f"Go to {p['verification_uri']} and enter {p['user_code']}\nor open {p['verification_uri_complete']}")
    interval, deadline = p["interval"], time.time() + p["expires_in"]
    while time.time() < deadline:
        time.sleep(interval)
        try:
            return signed_in(args, c.api("POST", "/pair/poll", {"device_code": p["device_code"]}))
        except ApiError as e:
            if e.code == "slow_down":
                interval += 5
            elif e.code != "authorization_pending":
                sys.exit(f"Pairing failed: {e}")
    sys.exit("Pairing expired.")

def login(args):
    body = dict(device(args), username=args.username or input("Username: "), password=getpass.getpass())
    code = input("Authentication or recovery code (blank if none): ").strip()
    signed_in(args, Client(args.server).api("POST", "/login", dict(body, code=code) if code else body))

def copy(src, out=None):
    """Stream src (a file or HTTP response) into out, returning its SHA-256."""
    h = hashlib.sha256()
    while chunk := src.read(1 << 16):
        h.update(chunk)
        if out:
            out.write(chunk)
    return h.hexdigest()

def scan(root):
    """NFC folder path -> (size, mtime ms, local path) for every regular file."""
    found = {}
    for top, _, names in os.walk(root):
        for name in names:
            full = os.path.join(top, name)
            if name != INDEX and not name.endswith(TMP) and not os.path.islink(full):
                st = os.stat(full)
                rel = unicodedata.normalize("NFC", os.path.relpath(full, root).replace(os.sep, "/"))
                found[rel] = (st.st_size, st.st_mtime_ns // 1_000_000, full)
    return found

def local_changes(root, files):
    """({path: (hash or None, size, mtime)} that differ from the index, paths still being written)."""
    first = scan(root)
    time.sleep(SETTLE)  # emulators write saves in several steps: skip files that are still moving
    seen, changed = scan(root), {}
    moving = {r for r in set(first) | set(seen) if first.get(r, (0, 0))[:2] != seen.get(r, (0, 0))[:2]}
    for rel, (size, mtime, full) in seen.items():
        old = files.get(rel)
        if rel in moving or (old and old["hash"] and (old["size"], old["mtime"]) == (size, mtime)):
            continue  # size and mtime are only a hint for when to re-hash; the hash decides
        with open(full, "rb") as f:
            digest = copy(f)
        if old and old["hash"] == digest:
            old["size"], old["mtime"] = size, mtime
        else:
            changed[rel] = (digest, size, mtime)
    for rel, old in files.items():
        if old["hash"] and rel not in seen and rel not in moving:
            changed[rel] = (None, 0, 0)
    return changed, moving

def apply(c, root, f):
    """Make the local file match server version f; returns its local (size, mtime)."""
    full = os.path.join(root, *f["path"].split("/"))
    if f["hash"] is None:
        if os.path.exists(full):
            os.remove(full)
        return 0, 0
    os.makedirs(os.path.dirname(full), exist_ok=True)
    tmp = full + TMP
    with c.call("GET", f"/blobs/{f['hash']}", timeout=600) as r, open(tmp, "wb") as out:
        digest = copy(r, out)
        out.flush()
        os.fsync(out.fileno())
    if digest != f["hash"]:
        os.remove(tmp)
        raise RuntimeError(f"Download of {f['path']} does not match its hash")
    if f["mtime"] > 0:
        os.utime(tmp, ns=(f["mtime"] * 1_000_000,) * 2)
    os.replace(tmp, full)  # never write the real file in place
    st = os.stat(full)
    return st.st_size, st.st_mtime_ns // 1_000_000

def upload(c, root, rel, digest, size):
    try:
        return c.call("HEAD", f"/blobs/{digest}").close()
    except ApiError as e:
        if e.status != 404:
            raise
    with open(os.path.join(root, *rel.split("/")), "rb") as f:
        headers = {"Content-Type": OCTET, "Content-Length": str(size)}
        c.call("PUT", f"/blobs/{digest}", data=f, headers=headers, timeout=600).close()

def commit(c, fid, policy, changes):
    try:
        return c.api("POST", f"/folders/{fid}/commit", {"on_conflict": policy, "changes": changes})["results"]
    except ApiError as e:
        if e.status >= 500 or e.status in (401, 403):
            raise
        if len(changes) == 1:
            print(f"skipped {changes[0]['path']}: {e}")
            return []
    results = []  # a request-level error rejects the whole commit: retry one by one to find it
    for change in changes:
        results += commit(c, fid, policy, [change])
    return results

def sync_once(c, fid, root, state, index_path):
    files = state["files"]

    def record(path, version, digest, size, mtime):
        files[path] = {"version": version, "hash": digest, "size": size, "mtime": mtime}

    changed, moving = local_changes(root, files)
    more = True
    while more:  # pull
        feed = c.api("GET", f"/folders/{fid}/changes?since={state['cursor']}")
        if feed.get("reset"):  # the server was restored: our versions are gone, start as a new device
            files.clear()
            state["cursor"] = 0
            save_json(index_path, state)
            return True
        for f in feed["changes"]:
            old = files.get(f["path"])
            if old and (old["version"], old["hash"]) == (f["version"], f["hash"]):
                continue  # our own commit, echoed back
            local = changed.pop(f["path"], None)
            if f["path"] in moving or (local and local[0] != f["hash"]):
                if local:
                    changed[f["path"]] = local  # changed on both sides: the commit reports the conflict
                continue
            record(f["path"], f["version"], f["hash"], *(local[1:] if local else apply(c, root, f)))
            print(f"pulled {f['path']}")
        state["cursor"], more = feed["seq"], feed["more"]
        save_json(index_path, state)
    groups = {"overwrite": [], "stash": [], "reject": []}
    for rel, (digest, size, mtime) in changed.items():  # push
        old = files.get(rel)
        try:
            if digest:
                upload(c, root, rel, digest, size)
        except ApiError as e:
            if e.code not in ("hash_mismatch", "too_large", "quota_exceeded"):
                raise
            print(f"skipped {rel}: {e}")
            continue
        # docs/sync-api.md "Conflict policy": local edits win, a new device never clobbers, deletes yield
        policy = "reject" if digest is None else "overwrite" if old else "stash"
        groups[policy].append({"path": rel, "base_version": old["version"] if old else 0,
                               "hash": digest, "size": size, "mtime": mtime})
    for policy, changes in groups.items():
        for i in range(0, len(changes), 1000):
            for r in commit(c, fid, policy, changes[i:i + 1000]):
                digest, size, mtime = changed[r["path"]]
                if r["status"] == "ok":
                    record(r["path"], r["version"], digest, size, mtime)
                    note = ", replacing a version this device had not seen" if r.get("conflict") else ""
                    print(f"pushed {r['path']} ({policy}{note})")
                elif r["status"] == "rejected":  # case_conflict or path_conflict: needs the user
                    print(f"skipped {r['path']}: {r['code']}: {r['error']}")
                elif r["current"] is None:  # the server never had this path: commit it as new next round
                    files.pop(r["path"], None)
                    print(f"{r['status']} {r['path']}: unknown to the server, retrying next round")
                else:  # conflict or stashed: the server version stays current
                    cur = dict(r["current"], path=r["path"])
                    record(cur["path"], cur["version"], cur["hash"], *apply(c, root, cur))
                    print(f"{r['status']} {r['path']}: took the server version ({policy})")
            save_json(index_path, state)
    return False

def sync(args):
    token = load_json(CONFIG, {}).get(args.server.rstrip("/"))
    if not token:
        sys.exit("Not signed in to this server: run pair or login first.")
    c, root = Client(args.server, token), os.path.abspath(args.local_dir)
    os.makedirs(root, exist_ok=True)
    folder = next((f for f in c.api("GET", "/folders") if f["name"] == args.folder), None)
    folder = folder or c.api("POST", "/folders", {"name": args.folder, "kind": args.kind})
    index_path = os.path.join(root, INDEX)
    state = load_json(index_path, {})
    if state.get("folder") != folder["id"]:
        state = {"folder": folder["id"], "cursor": 0, "files": {}}
    while True:
        while sync_once(c, folder["id"], root, state, index_path):
            print("The server was restored from a backup; syncing again as a new device.")
        if not args.watch:
            return
        c.api("GET", f"/folders/{folder['id']}/changes?since={state['cursor']}&limit=1&wait=30", timeout=45)

def main():
    p = argparse.ArgumentParser(description=__doc__)
    sub = p.add_subparsers(dest="command", required=True)
    for name, fn in (("pair", pair), ("login", login)):
        s = sub.add_parser(name, help=f"sign this device in ({name})")
        s.add_argument("server", help="LOKI origin, e.g. https://loki.example.com")
        s.add_argument("--name", default=platform.node() or "LOKI sync script", help="device name")
        s.add_argument("--username", help=argparse.SUPPRESS if name == "pair" else "account name")
        s.set_defaults(fn=fn)
    s = sub.add_parser("sync", help="two-way sync of one folder")
    s.add_argument("server")
    s.add_argument("folder", help="folder name; created if missing")
    s.add_argument("local_dir")
    s.add_argument("--kind", default="generic", help="kind hint for a new folder, e.g. retroarch-saves")
    s.add_argument("--watch", action="store_true", help="keep syncing, long-polling for changes")
    s.set_defaults(fn=sync)
    args = p.parse_args()
    try:
        args.fn(args)
    except ApiError as e:
        rejected = e.status == 401 and args.command == "sync"
        sys.exit(str(e) + ("\nThe device token was rejected: pair or log in again." if rejected else ""))

if __name__ == "__main__":
    main()
