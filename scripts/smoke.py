"""Shared HTTP test helpers; the entry point runs the WASI Bigshot workflow.

Runs on loopback with disposable data beneath target/smoke-*; never touches
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


class Browser:
    def __init__(self):
        self.jar = http.cookiejar.CookieJar()
        self.opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), urllib.request.HTTPCookieProcessor(self.jar))
        self.csrf = ""

    def request(self, url, data=None, content_type=None):
        headers = {}
        if data is not None:
            headers["content-type"] = content_type or "application/json"
            if content_type is None:
                data = json.dumps(data).encode()
            headers["x-csrf-token"] = self.csrf
        request = urllib.request.Request(url, data=data, headers=headers)
        try:
            with self.opener.open(request, timeout=90) as response:
                return response.status, response.read(), response.geturl()
        except urllib.error.HTTPError as response:
            return response.code, response.read(), response.geturl()

    def api(self, url, data=None):
        status, body, _ = self.request(url, data)
        assert status == 200, (url, status, body[:2000])
        return json.loads(body)

    def session(self, origin):
        value = self.api(origin + "/api/session")
        self.csrf = value["csrf"]
        return value

    def action(self, origin, name, resource="", data=None):
        return self.api(origin + "/api/action", {"action": name, "id": resource, "data": data or {}})


def multipart(fields, files=()):
    boundary = "loki-" + uuid.uuid4().hex
    chunks = []
    for name, value in fields.items():
        chunks += [f'--{boundary}\r\nContent-Disposition: form-data; name="{name}"\r\n\r\n{value}\r\n'.encode()]
    for name, filename, mime, content in files:
        chunks += [f'--{boundary}\r\nContent-Disposition: form-data; name="{name}"; filename="{filename}"\r\nContent-Type: {mime}\r\n\r\n'.encode(), content, b"\r\n"]
    chunks += [f"--{boundary}--\r\n".encode()]
    return b"".join(chunks), "multipart/form-data; boundary=" + boundary


def form(browser, url, fields):
    status, body, final = browser.request(url, urllib.parse.urlencode(fields).encode(), "application/x-www-form-urlencoded")
    assert status == 200, (status, body[:2000])
    return body.decode(), final


def main():
    import runpy
    runpy.run_path(str(Path(__file__).with_name("wasi-smoke.py")), run_name="__main__")


if __name__ == "__main__":
    main()
