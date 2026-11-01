# LOKI app protocol 1

**WASI components are the primary app format.** See [the WASI specification](wasi.md)
for manifests, the versioned WIT host API, shared HTTP/identity/storage, resource
limits, and building Bigshot. One portable component package serves all supported
LOKI platforms. Native processes remain supported as described below.

## Native compatibility

Native applications are independently built processes. No Rust ABI, shared
database, shared password hash, or compile-time dependency on the LOKI repository
is required. They can be written in any language that supports HTTP and OIDC.

## Package

A `.loki-app` is a ZIP with `manifest.json` at its root. Every other entry must be
a declared regular file. Directories are created implicitly; links, path
traversal, duplicate entries, Windows device paths, and undeclared files are
rejected. Limits: 2,048 entries, 128 KiB manifest, 512 MiB compressed request and
512 MiB expanded content. Checksums detect corruption; they do not establish a
publisher's identity. Import only packages you trust.

```json
{
  "protocol": 1,
  "id": "branch",
  "name": "Branch",
  "version": "1.0.0",
  "target": "x86_64-windows",
  "entry": "branch.exe",
  "health": "/health",
  "description": "A small public service.",
  "roles": ["admin"],
  "fields": [
    {"key":"message","label":"Welcome message","kind":"text","required":true,"default":"Hello, universe."}
  ],
  "files": {"branch.exe":"HEX_SHA256_OF_EXECUTABLE"}
}
```

Targets combine Rust's architecture and OS names: `x86_64-windows`,
`x86_64-linux`, `aarch64-macos`, and so on. Build a separate package per target.
IDs are lowercase ASCII letters/digits/hyphens, at most 64 bytes; `loki-` is
reserved. Versions use semver. Field kinds are `text`, `secret`, `number`, and
`bool`. Unknown configuration keys are rejected. Secret fields are masked in UI
state and stored in LOKI's private data directory.

The [Branch source](../crates/loki-server/examples/branch.rs) illustrates a native
process. Native archives follow this schema and must provide a checksum for
every regular file. `cargo loki` creates portable WASI packages; use the
[SDK workflow](../crates/loki-sdk/README.md) for new Rust apps.

## Runtime environment

| Variable | Meaning |
| --- | --- |
| `LOKI_PROTOCOL` | `1` |
| `LOKI_BIND` | Assigned loopback `IP:port`; listen here only |
| `LOKI_DATA_DIR` | Persistent application data directory |
| `LOKI_PUBLIC_URL` | Application origin; do not derive redirects from untrusted Host headers |
| `LOKI_ISSUER` | Canonical identity-provider origin |
| `LOKI_CLIENT_ID` | Registered client ID |
| `LOKI_CLIENT_SECRET` | Per-app credential; never log it |
| `LOKI_CONFIG` | JSON object of declared settings |

The working directory is the app data directory. Packages are installed
separately. Embedded static assets are recommended. Preserve app data across
restart, disable, package replacement, and uninstall/reinstall.

Return success from the health path only after the app can serve requests.
LOKI checks startup readiness and the child process, captures rotating stdout and
stderr, terminates the process tree on stop, and backs off after repeated crashes.
It removes public routing before replacing an app and restores the old package
and stopped-data snapshot if an enabled update fails readiness. Configuration
keeps matching keys and gains new defaults on upgrade; rollback restores the
prior configuration along with the prior package and data.

## Identity and permissions

Use OpenID Connect authorization-code flow with PKCE S256, random `state`, and a
random `nonce`. The registered callback is exactly:

```text
{LOKI_PUBLIC_URL}/auth/callback
```

Discovery: `{LOKI_ISSUER}/.well-known/openid-configuration`.

The v1 first-party provider supports `openid`, `profile`, and `email`; RS256 ID
tokens and JWKS; `client_secret_basic` or `client_secret_post`; and rotating opaque
access/refresh tokens. It does not implement dynamic client registration,
third-party consent, implicit flow, password grant, or identity federation.

Codes expire after two minutes and are single-use. Access tokens last 15 minutes,
refresh tokens seven days, and ID tokens five minutes. Reusing a spent refresh
token revokes its family. Keep tokens server-side behind a host-scoped HttpOnly
application session cookie. Serialize concurrent refreshes for the same session.

Validate ID-token signatures against the provider's JWKS and check exact issuer,
audience, expiration, issued-at, and the sign-in nonce. Use `(issuer, sub)` as the
identity key. Usernames and emails are mutable, and email is unverified.

For **every protected request**, POST the access token to `/oidc/introspect`,
authenticating with your app's own credentials. Example response:

```json
{"active":true,"sub":"stable-user-id","client_id":"branch","aud":"branch","exp":1793322000,"scope":"openid profile email","username":"aspen","name":"Aspen","email":"","roles":["access","admin"],"token_type":"Bearer"}
```

`active:false` or unavailable introspection means no access. Never use a local
cached user row or an ID token as ongoing authorization. Roles come from current
LOKI grants. Public resource visibility does not bypass an app's own decision to
require authentication for private features. Owner access includes app admin;
ordinary members are administrators only when explicitly granted that role.

## Device sign-in

Devices that cannot open a browser comfortably, such as consoles, TVs, handhelds
and desktop clients, can sign in with a code instead (RFC 8628). Discovery lists
`device_authorization_endpoint`.

1. The device POSTs `client_id` (and optionally `scope`, plus `name` and `platform`
   to show on the approval page) as a form to `/oidc/device`. No client secret is
   needed, because such devices cannot keep one. The response has `device_code`,
   `user_code`, `verification_uri`, `verification_uri_complete`, `interval` (5)
   and `expires_in` (600).
2. The device shows `user_code`, or a QR code of `verification_uri_complete`. The
   person opens `/pair` in LOKI, checks the device and app, and approves. Only
   people with access to the app can approve.
3. The device POSTs `grant_type=urn:ietf:params:oauth:grant-type:device_code`,
   `client_id` and `device_code` to `/oidc/token` every `interval` seconds. Until
   approval it gets `400 {"error":"authorization_pending"}`; polling too fast gives
   `slow_down`, a declined code `access_denied`, and an expired or spent one
   `expired_token`. Approval returns the usual access, refresh and ID tokens.

The tokens belong to the app's client and are checked with introspection like any
others. A password change, suspension or revoked access ends them. The device
refreshes with `grant_type=refresh_token`, `refresh_token` and `client_id`, without
a secret; only refresh tokens that came from device sign-in work that way. Refresh
tokens rotate, and reusing a spent one ends the device's sign-in.

Bigshot uses host-managed WASI identity through the Rust SDK; it no longer
ships a standalone native server or independent account adapter.

## Updating

During initial development, an owner may replace a package with the same semver
version. Older versions are rejected. LOKI
stops the app, snapshots data, retains one prior package, and starts the new one
only if it was previously enabled. Data migrations run inside the app. A rollback
restores both the prior package and its data snapshot; later data changes are
replaced. Installation does not execute package-provided installer scripts.
