# LOKI management API v1

Use the same LOKI server origin as the dashboard. The API uses device pairing
and `Authorization: Bearer TOKEN`. HTTPS is required for a deployed server.
Loopback HTTP is supported for development. Native API calls are separate from
browser sessions and their CSRF checks.

## Pairing

1. Send `POST /api/v1/auth/pair` with
   `{"scope":"repositories","name":"My CLI","platform":"desktop"}`.
2. Display `verification_uri` and `user_code`. Ask the user to sign in to LOKI
   and approve the code shown by their client.
3. At the returned `interval`, send `POST /api/v1/auth/pair/poll` with the same
   `scope` and the returned `device_code`. Continue polling on
   `authorization_pending` and back off on `slow_down`. A successful response
   contains the new `token`, user, and device.
4. Use `GET /api/v1/me` to check the account and grant. Use
   `DELETE /auth/v1/device` to revoke the current credential.

The `repositories` grant permits Skald management according to the user's current
repository permissions. It also works with permitted Save Sync and shared
files on the existing `/sync/v1` and `/files/v1` APIs. Request `owner` for
server administration. Only the installation owner can approve it, using
their current password and configured second factor. Grants expire after 90
days and can be revoked from **My account → Devices**. Regular sync and files
device tokens cannot call the management API.

`GET /api/v1/devices` lists the caller's own devices. Send `{"name":"New name"}`
to `POST /api/v1/devices/{id}` to rename one or use `DELETE` on that path
to revoke it.

A native application can also send `POST /api/v1/auth/login` with `scope` set to
`repositories`, plus `username`, `password`, `name`, and optional `platform`
and MFA `code`. Owner API access uses dashboard-approved pairing.

## Repositories

| Method and path | Operation |
| --- | --- |
| `GET /api/v1/repos` | List visible repos and whether the account can create one. |
| `POST /api/v1/repos` | Create a private repo. Send `name` and optionally `default_branch` or `upstream` to create a private fork. Returns `id` and `remote_url`. |
| `GET /api/v1/repos/{id}` | Read metadata, branches, files, and recent commits. |
| `DELETE /api/v1/repos/{id}` | Delete a repo as its owner or the installation owner. |
| `GET /api/v1/repos/{id}/file` | Read a path using the same `branch`, `revision`, `path`, and `raw` query parameters as the dashboard. |
| `GET, POST /api/v1/repos/{id}/permissions` | List or change user and group grants. Send `subject_kind`, `subject`, and `role` for writes. Use `remove` to remove a grant. |
| `POST /api/v1/repos/{id}/settings` | Set `default_branch` and `protect_default`. |
| `GET, POST /api/v1/repos/{id}/items` | List issues and PRs or create an item. |
| `GET, POST /api/v1/repos/{id}/items/{item}` | View or update an item. |
| `POST /api/v1/repos/{id}/credential` | Exchange API access for a temporary Skald bearer token. Send `{"write":true}` for a push or `{"write":false}` for a clone. Returns `token`, `expires`, and `remote_url`. |

Set `SKALD_TOKEN` for skald or send the token using the Bearer authorization
scheme on the returned remote URL. It is scoped to the requested repository and
expires within one hour. Revoking the paired device revokes its transfer
tokens. LOKI checks current membership and repository grants on each request.
See the [Skald protocol](https://github.com/LOKI-Server/skald-vcs/blob/main/docs/protocol-v1.md).

## Administration

`GET /api/v1/resources` lists resources visible to the caller. Owner grants can
also list `/api/v1/admin/users`, `/groups`, `/grants`, `/activity`, `/settings`, and `/backups`
under the same `/api/v1/admin` prefix.

Send `POST /api/v1/admin/actions` with `{"action":"…","id":"…","data":{…}}`.
The API supports the following actions with the same permission rules as the
dashboard:

| Action | Subject and fields |
| --- | --- |
| `user-create` | `data.username`. Returns an activation URL. |
| `user-status` | User ID and `data.status` of `approved`, `pending`, `rejected`, or `suspended`. |
| `reset-issue` | User ID. Returns a password reset URL. |
| `group-create` | `data.name`. |
| `group-delete` | Group ID. |
| `membership` | User ID, `data.group`, and `data.enabled`. |
| `resource-access` | Resource ID, `data.visibility` and `data.grants` with `subject_kind`, `subject`, and `role` per grant. |
| `share-save` | Existing share ID or empty for a new share. Send `data.name`, `data.root`, and optionally `data.excluded_extensions`. Set `data.supports_updates` to enable Desktop update checks. New shares default to `false`, and omitted values leave existing shares unchanged. |
| `app-action` | App ID and `data.operation`, such as `enable` or `disable`. |
| `sync-quota` | User ID and `data.mb` in MiB or `null` for the default. |
| `backup` | Returns a local backup filename. |

Treat activation and reset URLs like credentials. Host configuration, server
relocation, backup download, and service configuration use the dashboard.
