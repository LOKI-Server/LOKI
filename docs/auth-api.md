# LOKI account device API 1

Native LOKI clients sign in once under `/auth/v1`. The resulting account device
token works for both `/sync/v1` and `/files/v1`. Clients reuse their saved token
when opening the file browser; they do not sign in again or store a password.
Browser cookies and app-scoped OIDC tokens remain separate.

Use HTTPS outside loopback. Public `GET /auth/v1` returns
`{protocol:1, server:"0.6.0", name:"LOKI", pairing:true, services:["sync","files"]}`.
Clients should reject unsupported protocol versions and ignore unknown fields.
The [OpenAPI description](auth-api.yaml) covers these account endpoints.

## Code pairing

1. `POST /auth/v1/pair` with `{"name":"Desktop","platform":"windows"}`.
   Names are trimmed, contain 1–80 characters, and cannot contain control
   characters. Optional platform is trimmed and limited to 40 characters.
2. Display the returned `user_code` and `verification_uri`, or open
   `verification_uri_complete`. Codes last ten minutes. The private
   `device_code` must never be displayed or logged.
3. The person opens **Account → Devices → Pair a device** in the server dashboard,
   checks the device details, and approves or declines. The `/pair?code=…` link
   opens this same account-level approval flow. Native account pairing does
   not require a Save sync grant. OIDC app codes still require access to the app.
4. Poll `POST /auth/v1/pair/poll` with `{"device_code":"…"}` no faster than the
   returned `interval` (five seconds).

| Status/code | Meaning |
| --- | --- |
| 428 `authorization_pending` | Not approved yet; keep polling |
| 429 `slow_down` | Poll less often |
| 403 `access_denied` | Declined; stop |
| 400 `expired_token` | Expired, unknown, invalidated, or already collected |
| 200 | One account device credential issued; discard the device code |

Approval and credential collection are single use. Account status and credential
version are checked again before issuing a token. Password changes and session
revocation invalidate pending approvals too.

## Password and MFA

`POST /auth/v1/login` accepts:

```json
{"username":"aspen","password":"…","code":"123456","name":"Desktop","platform":"windows"}
```

Username also accepts the account email. `code` may be a TOTP or recovery code;
omit it when MFA is not enabled. Browser, sync, file, and account login routes
share password-guess limits and MFA replay protection. Invalid credentials
return `401` with `code: invalid_credentials`; unapproved accounts return `403`.

Successful password login and pairing return the same shape:

```json
{"token":"opaque-secret","device":{"id":"…","name":"Desktop","platform":"windows"},"user":{"id":"…","username":"aspen","display_name":"Aspen"},"access":{"sync":true}}
```

Store the token like a password and send `Authorization: Bearer <token>` on
authenticated requests. Only a digest is stored in the server database.
`access.sync` lets clients avoid starting sync when the account only uses file
sharing. It is a hint for the UI, not a permission grant; the server rechecks
every service request independently.

## Current device and sign-out

`GET /auth/v1/me` returns `{user, device, access:{sync:boolean}}` for the saved
credential. `DELETE /auth/v1/device` revokes that credential and returns
`{"ok":true}`. It signs the device out of both file access and sync.

Under **Account → Devices**, approved users can list, rename, and revoke their
own native devices regardless of whether they can use Save sync. The dashboard
state exposes `devices` without tokens. Cookie/CSRF-protected `/api/action`
actions `device-rename` (`{name}`), `device-revoke`, `device-lookup` (`{code}`),
`device-approve` (`{code}`), and `device-deny` (`{code}`) power this account page.
Users cannot manage another person's devices.

Tokens have no fixed expiry. Device revocation, password changes/resets, account
suspension, owner recovery, and signing out everywhere invalidate them. An
invalid or revoked credential returns `401` with `code: unauthorized`; a valid
credential lacking sync/share permissions returns `403`. Existing in-flight
streams can finish after revocation.

## Compatibility

The original `/sync/v1/login`, `/sync/v1/pair`, `/sync/v1/pair/poll`,
`/sync/v1/me`, and `/sync/v1/device` routes remain available for older clients.
Legacy sync sign-in still requires sync access, and `/sync/v1/me` retains its
quota fields. The corresponding Files API sign-in and sign-out routes delegate
to account sign-in.

Existing native sync and file credentials migrate to the shared device registry
with their original token digests and IDs. Users do not need to pair again.
Sync history still resolves the device names. New desktop and Android clients
reuse their existing main login; a legacy standalone file login migrates only
when no main login exists. They never substitute a different file account for
an already signed-in main account.
