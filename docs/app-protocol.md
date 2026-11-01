# LOKI app packages and identity

LOKI apps are WASI components. See [the WASI specification](wasi.md) for
manifests, the versioned WIT host API, shared HTTP/identity/storage, resource
limits, and building Bigshot. One portable component package serves all supported
LOKI platforms. This page covers the package format, the identity provider that
apps and their devices sign in with, and updates.

LOKI 0.3 removed native process packages. An installed native app keeps its data
but no longer starts; import a WASI build of the app to update it.

## Package

A `.loki-app` is a ZIP with `manifest.json` at its root. Every other entry must be
a declared regular file. Directories are created implicitly; links, path
traversal, duplicate entries, Windows device paths, and undeclared files are
rejected. Limits: 2,048 entries, 128 KiB manifest, 512 MiB compressed request and
512 MiB expanded content. Checksums detect corruption; they do not establish a
publisher's identity. Import only packages you trust.

`cargo loki package` writes the manifest and checksums; see the
[WASI manifest](wasi.md#manifest) for its fields. IDs are lowercase ASCII
letters/digits/hyphens, at most 64 bytes; `loki-` is reserved. Versions use
semver. Field kinds are `text`, `secret`, `number`, and `bool`. Unknown
configuration keys are rejected. Secret fields are masked in UI state and stored
in LOKI's private data directory.

LOKI removes public routing before replacing an app and restores the old package
and stopped-data snapshot if an enabled update fails to start. Configuration
keeps matching keys and gains new defaults on upgrade; rollback restores the
prior configuration along with the prior package and data.

## Identity and permissions

Apps don't run OpenID Connect themselves. LOKI signs people in on each app's
origin and hands the app the current user, with the roles they hold from current
LOKI grants, on every request; see [shared server and identity](wasi.md#shared-server-and-identity).
LOKI's provider (discovery at `/.well-known/openid-configuration`) serves those
sign-ins, gated routes, and device sign-in below.

It supports `openid`, `profile`, and `email`; RS256 ID tokens and JWKS; and
rotating opaque access/refresh tokens. It does not implement dynamic client
registration, third-party consent, implicit flow, password grant, or identity
federation. Access tokens last 15 minutes, refresh tokens seven days, and ID
tokens five minutes. Reusing a spent refresh token revokes its family.

Use the user's stable `id` as the identity key. Usernames and emails are mutable,
and email is unverified. Public resource visibility does not bypass an app's own
decision to require authentication for private features. Owner access includes
app admin; ordinary members are administrators only when explicitly granted that
role.

## Device sign-in

Devices that cannot open a browser comfortably, such as consoles, TVs, handhelds
and desktop clients, can sign in with a code instead (RFC 8628). Discovery lists
`device_authorization_endpoint`.

1. The device POSTs `client_id` (`component-APP_ID` for an app; optionally
   `scope`, plus `name` and `platform` to show on the approval page) as a form to
   `/oidc/device`. No client secret is
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

The device sends the access token to the app as `Authorization: Bearer
ACCESS_TOKEN`, and the app sees the signed-in user as for a browser. A password
change, suspension or revoked access ends the tokens. The device refreshes with `grant_type=refresh_token`, `refresh_token` and `client_id`, without
a secret; only refresh tokens that came from device sign-in work that way. Refresh
tokens rotate, and reusing a spent one ends the device's sign-in.

## Updating

During initial development, an owner may replace a package with the same semver
version. Older versions are rejected. LOKI
stops the app, snapshots data, retains one prior package, and starts the new one
only if it was previously enabled. Data migrations run inside the app. A rollback
restores both the prior package and its data snapshot; later data changes are
replaced. Installation does not execute package-provided installer scripts.
