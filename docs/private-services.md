# Git hosting and personal storage

## Git hosting

LOKI runs Git 2.54 or later, which added the configured hooks LOKI uses to
check every push. The Windows installer includes MinGit from Git for Windows in
its `git` folder, so no separate install is needed. On Linux and macOS, install
Git 2.54 or later from your package manager. LOKI looks for Git in this order:

1. `LOKI_GIT_EXECUTABLE`, an absolute path set before launching LOKI;
2. on Windows, the bundled `git\cmd\git.exe`, then a standard Git for Windows
   install in `Program Files`;
3. `git` on `PATH`.

LOKI checks the version and hook support on first use. Public Git URLs require
HTTPS with LOKI's Caddy or another reverse proxy on the LOKI host. Development
URLs can use loopback HTTP.

The installation owner can create repositories and grant creation to users and
groups from **Repositories → Manage repository creation**. Each repository's
**Manage access** screen controls Read, Write, and Manage roles. A manager can
push the initial `main` branch to an empty repository. After that, submit changes
to `main` through reviewed pull requests. Use **Git credentials** to create a
30-day repository token. Enter the token in your Git client's password prompt or
credential manager. Revoke tokens from the same screen.

Forks are independent private copies. When opening a pull request, the target
repository receives the selected branch's submitted commit and its history.
Reviewers can inspect those changes even if they cannot browse the fork. Refresh
the submission and approve it again after updating the source or target.

Git stores its data in `git-repos` under the LOKI data directory. The server
limits incoming pushes to 64 MiB, individual repositories including their pull
request snapshots to 1 GiB, and combined Git storage to 20 GiB. Pushes and forks
also stop while the data volume has less than 2 GiB free. Git content is
included in full installation backups.

## Personal storage and exports

Add `personal-storage` to an app manifest and use `loki_sdk::PersonalStorage` in
signed-in HTTP handlers. The installation owner enables the app's personal
storage permission from **Applications → Manage**. Each app sees its own folder
for the current user. `PersonalStorage.usage()` reports the account's usage
across those folders. Small generated files use `write`. Use `import_upload`
with a multipart upload and the `uploads` capability for larger files. Use `read`
with an offset or `download` for ranged reads or browser attachments.

Personal storage has no per-account quota; apps can set their own policies for
files they save in these folders, and `usage()` reports the maximum integer
values for its limit fields. LOKI applies its regular upload limits to multipart
requests and refuses personal writes that would leave less than 2 GiB free on
the data volume (status 507). Save Sync has its own quota, and Git has its own
storage limits.

Installation backups include personal files. Users can download selected app
folders, including folders from removed apps, in a private ZIP from
**Account → Export my data**. Each export supports up to 50 GiB and 20,000 files.
The ZIP's `manifest.json` lists every included file and, under `skipped`, any
file an app replaced or deleted while the download was running.

File shares never serve LOKI's data directory, even when a shared folder
contains it, so personal files, Save Sync files and Git repositories stay
private.
