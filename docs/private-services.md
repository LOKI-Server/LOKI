# Repositories and personal storage

## Skald hosting

Open **Repositories** to manage private Skald projects. The installation
owner can grant repository creation to selected people and groups under
**Manage repository creation**. Each repository has Read, Write, and Manage
roles. Managers can set the default branch or require review to advance it
after the first push. The dashboard supports private forks, issues, and reviews of branches in the
same repository. A reviewed submission can advance the default branch
when it fast-forwards. For diverged history, merge the current default branch
into the source branch with skald, resolve any conflicts, push the result, then
refresh and review the submission.

Create an expiring read or write token under **My account â†’ Skald
credentials**. Set SKALD_TOKEN in the shell that runs skald, or supply the token
to an application using the native SDK. Paired management API clients can
request repository-scoped transfer tokens. Both credentials follow current
account and repository permissions.

Use **Server settings â†’ Skald storage** for per-repository and installation
quotas. The limits count logical bytes of unique objects within each repository,
including history, pending uploads, and metadata. Compression may reduce
physical disk usage. The default limits are 1 GiB per repository and 20 GiB
combined. The server retains its free-space buffer before accepting uploads.
Repositories expose verification and collection from the management view.
Hosted collection retains upload objects for at least 24 hours and protects
refs, active review data, and historical review submissions.

Installation backups include the SQLite database and native repository data
under skald-repos. The restore command verifies the native object graph before
installing the backup. Public deployments use HTTPS via LOKI's Caddy or a
loopback reverse proxy. Loopback HTTP works for development. See the
[protocol reference](https://github.com/LOKI-Server/skald-vcs/blob/main/docs/protocol-v1.md).

## Preserve existing Git-hosted data

Before upgrading, create and download a full installation backup from the
existing LOKI server. Stop it and make a private, off-host copy of its complete
data directory. Retain the database and the git-repos and git-lfs directories.
The database contains repository ownership, permissions, issues, pull requests,
and LFS associations. The bare repositories retain history, refs, and review
snapshots. Keep the former server version and its Git/LFS tooling with this
archive for offline recovery. Preserve signing keys and restrict access to
the backup.

On first startup the new server creates a separate skald-repos directory.
Existing directories and SQLite records are retained. Subsequent installation
backups also carry those files as archival data. The native dashboard lists
new Skald repositories. Old tokens and clone URLs do not authorize Skald.
Legacy Git/LFS integrity is best checked on a restored copy using the old
server and tools before upgrade. For backups larger than the browser import
limit, restore offline into an empty directory.

~~~sh
loki-server --data-dir NEW_EMPTY_DIRECTORY restore BACKUP.zip
~~~

### Import a working snapshot

1. Before retiring the previous server, use your existing Git client to clone
   and check out the revision you want to import. Hydrate any Git LFS files and
   check that large assets contain their expected bytes. Restore submodules
   separately if their files belong in the new project.
2. Copy the checked-out project files to a new working directory. Exclude all
   Git metadata directories and files, including any nested .git entries. Save
   any legacy tooling config you still need outside the import copy.
3. Create an empty Skald repository from the dashboard. Use the same default
   branch name or create and switch to the matching branch locally.
4. In the new working directory, initialize and verify a native snapshot, add
   the dashboard's remote URL, and publish it using a new write token.

~~~sh
skald init
skald add .
skald commit --author "Migration operator" -m "Import working snapshot"
skald verify
skald remote set origin https://loki.example.com/skald/v1/repos/REPOSITORY_ID
skald push
~~~

Clone the Skald repository into a new directory and compare the important
files before retiring the prior service. This procedure imports the checked-out
files as a new initial revision. Full-history conversion, legacy pull-request
conversion, and automatic LFS-pointer conversion are unsupported. The old
history and discussions remain in the preserved archive.

## Personal storage and exports

Add `personal-storage` to an app manifest and use `loki_sdk::PersonalStorage` in
signed-in HTTP handlers. The installation owner enables the app's personal
storage permission from **Applications Ã¢â€ â€™ Manage**. Each app sees its own folder
for the current user. `PersonalStorage.usage()` reports the account's usage
across those folders. Small generated files use `write`. Use `import_upload`
with a multipart upload and the `uploads` capability for larger files. Use `read`
with an offset or `download` for ranged reads or browser attachments.

Personal storage has no per-account quota; apps can set their own policies for
files they save in these folders, and `usage()` reports the maximum integer
values for its limit fields. LOKI applies its regular upload limits to multipart
requests and refuses personal writes that would leave less than 2 GiB free on
the data volume (status 507). Save Sync has its own quota, and Skald has its own
storage limits.

Installation backups include personal files. Users can download selected app
folders, including folders from removed apps, in a private ZIP from
**Account Ã¢â€ â€™ Export my data**. Each export supports up to 50 GiB and 20,000 files.
The ZIP's `manifest.json` lists every included file and, under `skipped`, any
file an app replaced or deleted while the download was running.

File shares never serve LOKI's data directory, even when a shared folder
contains it, so personal files, Save Sync files and Skald repositories stay
private.
