"""Measure idle LOKI resident memory and one-core CPU use; stdlib only."""
import argparse
import ctypes
import http.cookiejar
import json
import os
import platform
import socket
import subprocess
import time
import urllib.request
import uuid
import zipfile
from pathlib import Path


def sample(pid):
    if os.name == "nt":
        from ctypes import wintypes as w
        class Counters(ctypes.Structure):
            _fields_ = [("cb", w.DWORD), ("page_faults", w.DWORD)] + [(n, ctypes.c_size_t) for n in ("peak_working_set", "working_set", "paged_peak", "paged", "nonpaged_peak", "nonpaged", "pagefile", "peak_pagefile")]
        kernel = ctypes.WinDLL("kernel32", use_last_error=True)
        kernel.OpenProcess.restype = w.HANDLE
        handle = kernel.OpenProcess(0x410, False, pid)
        if not handle:
            raise ctypes.WinError(ctypes.get_last_error())
        try:
            counters = Counters()
            counters.cb = ctypes.sizeof(counters)
            psapi = ctypes.WinDLL("psapi", use_last_error=True)
            psapi.GetProcessMemoryInfo.argtypes = [w.HANDLE, ctypes.POINTER(Counters), w.DWORD]
            assert psapi.GetProcessMemoryInfo(handle, ctypes.byref(counters), counters.cb)
            times = [w.FILETIME() for _ in range(4)]
            kernel.GetProcessTimes.argtypes = [w.HANDLE] + [ctypes.POINTER(w.FILETIME)] * 4
            assert kernel.GetProcessTimes(handle, *(ctypes.byref(t) for t in times))
            cpu = sum((t.dwHighDateTime << 32) | t.dwLowDateTime for t in times[2:]) / 10_000_000
            return counters.working_set, cpu
        finally:
            kernel.CloseHandle.argtypes = [w.HANDLE]
            kernel.CloseHandle(handle)
    if platform.system() == "Linux":
        fields = Path(f"/proc/{pid}/stat").read_text().split()
        rss = int(fields[23]) * os.sysconf("SC_PAGE_SIZE")
        return rss, (int(fields[13]) + int(fields[14])) / os.sysconf("SC_CLK_TCK")
    rss = int(subprocess.check_output(["ps", "-o", "rss=", "-p", str(pid)]).strip()) * 1024
    raw = subprocess.check_output(["ps", "-o", "time=", "-p", str(pid)], text=True).strip().replace("-", ":")
    cpu = 0
    for part in raw.split(":"):
        cpu = cpu * 60 + float(part)
    return rss, cpu


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("binary", type=Path)
    parser.add_argument("--seconds", type=int, default=20)
    parser.add_argument("--package", type=Path, help="Include one enabled WASI app in the idle sample")
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    data = root / "target" / ("benchmark-" + uuid.uuid4().hex)
    data.mkdir(parents=True)
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    with (data / "console.log").open("wb") as log:
        process = subprocess.Popen([str(args.binary.resolve()), "--data-dir", str(data), "--bind", f"127.0.0.1:{port}", "--no-tray"], stdout=log, stderr=log, env=dict(os.environ, LOKI_NO_TRAY="true", LOKI_LOCATION_FILE=str(data / "test-location.json")))
        try:
            client = urllib.request.build_opener(urllib.request.ProxyHandler({}), urllib.request.HTTPCookieProcessor(http.cookiejar.CookieJar()))
            origin = f"http://127.0.0.1:{port}"
            ready = False
            for _ in range(150):
                try:
                    with client.open(origin + "/health", timeout=1):
                        ready = True
                        break
                except OSError:
                    assert process.poll() is None, "Server exited during startup"
                    time.sleep(0.1)
            assert ready, "Server did not become ready"
            # Measure an initialized installation, after an actual owner login.
            with client.open(origin + "/api/session", timeout=10) as response:
                csrf = json.load(response)["csrf"]
            def post(path, payload):
                request = urllib.request.Request(origin + path, data=json.dumps(payload).encode(), headers={"Content-Type": "application/json", "X-CSRF-Token": csrf})
                with client.open(request, timeout=30) as response:
                    assert response.status == 200
            post("/api/setup", {"token": (data / "setup-token.txt").read_text(), "username": "owner", "password": "benchmark-owner-123", "name": "Benchmark", "public_url": origin})
            post("/api/login", {"username": "owner", "password": "benchmark-owner-123"})
            if args.package:
                from smoke import multipart
                with client.open(origin + "/api/session", timeout=10) as response:
                    csrf = json.load(response)["csrf"]
                package = args.package.read_bytes()
                body, content_type = multipart({}, [("package", args.package.name, "application/octet-stream", package)])
                request = urllib.request.Request(origin + "/api/apps/import", data=body, headers={"Content-Type":content_type,"X-CSRF-Token":csrf})
                with client.open(request, timeout=60) as response:
                    assert response.status == 200
                with zipfile.ZipFile(args.package) as archive:
                    app_id = json.loads(archive.read("manifest.json"))["id"]
                post("/api/action", {"action":"app-action","id":app_id,"data":{"operation":"enable"}})
            time.sleep(3)
            before_mem, before_cpu = sample(process.pid)
            start = time.monotonic()
            time.sleep(args.seconds)
            memory, cpu = sample(process.pid)
            report = {"platform": platform.platform(), "processor": platform.processor(), "binary": str(args.binary.resolve()), "initialized": True, "sample_seconds": round(time.monotonic() - start, 2), "resident_mib": round(max(before_mem, memory) / 1048576, 2), "cpu_percent_one_core": round((cpu - before_cpu) / (time.monotonic() - start) * 100, 3), "apps": int(args.package is not None), "runtime": "wasi" if args.package else None, "caddy": False}
            (root / ("target/benchmark-wasi-last.json" if args.package else "target/benchmark-last.json")).write_text(json.dumps(report, indent=2))
            print(json.dumps(report, indent=2))
        finally:
            process.terminate()
            process.wait(15)


if __name__ == "__main__":
    main()
