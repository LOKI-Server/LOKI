# LOKI Files API 1

The read-only HTTP API under `/files/v1` lets native clients download files from
existing LOKI shares. It is independent of Save sync: files stay in their original
server folders, and using this API does not create sync folders, blobs, history,
or a watched directory. The [OpenAPI description](files-api.yaml) describes the
native endpoints. No new client is included.

Use the installation's canonical origin as the base URL, with HTTPS outside
loopback. Clients must check `protocol` and ignore unknown response fields.

## Discovery and sign-in

`GET /files/v1` is public and advertises protocol 1, SHA-256 checksums, byte ranges,
100-entry pages, `max_page: 999`, `pairing: true`, and `auth: "/auth/v1"`.

Use the shared [account device sign-in API](auth-api.md) for code pairing or
password/MFA login. One saved credential works for both file browsing and Save
sync; clients must reuse it instead of asking the person to sign in twice.
Sign-in itself does not require a Save sync grant. Each service checks its own
permissions on every request.

```http
Authorization: Bearer opaque-secret
```

Browser session cookies and app-scoped OIDC tokens are not accepted. Account
device tokens issued through `/auth/v1`, or the compatibility sync/file login
routes, are accepted. Existing sync and file credentials migrate automatically
without changing their raw tokens or device IDs.

Compatibility routes `POST /files/v1/login`, `POST /files/v1/pair`, and
`POST /files/v1/pair/poll` delegate to the shared account API. New clients should
use `/auth/v1`. The account token is stored only as a digest on the server;
passwords are never needed again for file browsing after sign-in.

## Accounts and devices

| Method | Path | Response |
| --- | --- | --- |
| GET | `/auth/v1/me` | `{user, device, access:{sync:boolean}}` |
| DELETE | `/auth/v1/device` | Sign this account device out of both services |
| GET | `/files/v1/me` | Compatibility `{user, device}` response |
| GET | `/files/v1/devices` | `{devices:[{id,name,platform,created,last_seen}]}` for this account |
| DELETE | `/files/v1/device` | Compatibility account-device sign-out |
| DELETE | `/files/v1/devices/{id}` | Revoke one of this account's devices; `{"ok":true}` |

**Account → Devices** in the dashboard is the common place to approve a sign-in
code, rename devices, and sign them out. It is available even when the account
has no Save sync grant. Revoking a device affects both file access and sync.

Device lists never include tokens or their digests. Deletion is idempotent and
cannot affect another user's devices. `created` and `last_seen` are Unix seconds;
activity updates at most once per minute. The cookie/CSRF-protected compatibility
routes `GET /api/file-devices` and `DELETE /api/file-devices/{id}` expose the same
account devices. Account actions `device-rename` and `device-revoke` are preferred
for the dashboard.

Password changes/resets, account suspension, owner recovery, and revoking all
sessions invalidate account device tokens. Permissions are checked on every
request; revocation does not interrupt a response stream already opened.

## Shares and directory listings

`GET /files/v1/shares` returns only accessible shares:

```json
{"shares":[{"id":"…","name":"Archive","visibility":"restricted","kind":"share"}]}
```

`GET /files/v1/shares/{id}` returns one share with the same fields. Absolute
server paths and other administrative settings are never exposed. Existing
public/member visibility and individual/group grants apply.

`GET /files/v1/shares/{id}/entries?path=Games&page=0&sort=name` returns:

```json
{"entries":[{"name":"Example.zip","directory":false,"size":1234,"modified":1793836800}],"total":1,"page":0,"path":"Games"}
```

`path` defaults to the share root (`""`). Paths are share-relative, case-sensitive
as supported by the host filesystem, with forward slash separators. URL-encode
query values, including spaces, Unicode names, `#`, `&`, and `+`. Parent traversal,
absolute paths, backslashes, drive letters, and alternate data streams are
rejected. Symlinks and Windows junctions/reparse points are blocked.

Listings contain 100 entries per zero-based page, with folders first. `sort` is
`name` (default, ascending case-insensitive), `size` (descending), or `modified`
(descending). Name breaks ties. Pages 0–999 are supported, matching the browser
listing limit; higher pages return `400`. `total` counts all visible entries,
even when it exceeds this pagination window. Listings are live rather than
snapshots, so changes between requests can shift entries between pages.

`size` is bytes and `modified` is Unix seconds. Excluded extensions are filtered
before pagination, at every directory depth. Excluded files return `404` on
direct metadata and download requests too. Directories remain browsable.

## Metadata and checksums

`GET /files/v1/shares/{id}/metadata?path=Games/Example.zip` returns:

```json
{"path":"Games/Example.zip","name":"Example.zip","size":1234,"modified":1793836800,"last_modified":"Thu, 05 Nov 2026 00:00:00 GMT"}
```

Add `checksum=true` to request a lowercase hexadecimal `sha256` field. Hashing
streams the file with bounded memory; it reads the entire file, so request it
when needed rather than for each listing. If the file's size or modification
time changes while hashing, the server returns `409` with `code: file_changed`.
Empty files have the usual SHA-256 checksum of an empty byte sequence.

Metadata applies to regular files only. `last_modified` is an HTTP date, or
`null` if unavailable; `modified` is `0` if unavailable.

## Downloading and resuming

`GET /files/v1/shares/{id}/file?path=Games/Example.zip` streams the file with
`Content-Length`, `Accept-Ranges: bytes`, `Last-Modified` when available, and
`Content-Disposition: attachment`. Unicode filenames use `filename*=UTF-8''…`.
`HEAD` on the same URL returns the full file headers without reading the body.
Responses are private and not cached. Content is streamed in bounded chunks.

To resume, send a single byte range:

```http
Range: bytes=1048576-
If-Range: Thu, 05 Nov 2026 00:00:00 GMT
```

`If-Range` is optional and should contain the previous `Last-Modified` exactly.
A match returns `206` with `Content-Range`; a changed, unavailable, or
unrecognized validator returns `200` and the **entire file**. On `200`, replace
the partial local file rather than appending. Weak ETags never match.
`Range` is ignored on `HEAD`.

Explicit (`bytes=0-1023`), open-ended (`bytes=1024-`), and suffix (`bytes=-1024`)
ranges are supported. Invalid, unsatisfiable, and multiple ranges return `416`
with `Content-Range: bytes */SIZE`. An empty file downloads as `200` with zero
bytes; any range against it returns `416`.

Shared files can change outside LOKI. HTTP dates have one-second resolution,
and file responses are not filesystem snapshots. For integrity, get a checksum,
download, then compare the local SHA-256; retry if it differs. Clients downloading
a folder should traverse directory listings and queue individual files.

## Errors

Errors use the existing JSON shape `{"error":"message","code":"optional_code"}`.
Query/body parsing and unsupported method errors may use framework responses.

| Status | Meaning |
| --- | --- |
| 400 | Invalid path, page, sort, device information, or request |
| 401, `invalid_credentials` | Incorrect username, password, or MFA code during login |
| 401, `unauthorized` | Missing, invalid, or revoked account device token |
| 403, `forbidden` | Account not approved at login, or no access to the share |
| 404 | Missing share/file, excluded file, or non-file metadata/download target |
| 409, `file_changed` | File changed while calculating the checksum |
| 416 | Unsupported or unsatisfiable byte range |
| 429 | Shared sign-in rate limit reached |
| 503 | Server restarting or performing maintenance |

The file endpoints expose no upload, edit, delete, or sync operations.
