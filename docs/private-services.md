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

The installation owner can grant repository creation to users and groups from
**Repositories → Manage repository creation**. Each repository's **Manage
access** screen controls Read, Write, and Manage roles.

### Git credentials

Create a named token under **My account → Git credentials**. Enter your LOKI
username and the token as the password in Git's HTTPS prompt. Save it in Git
Credential Manager or your OS keychain-backed credential helper. One token
works across all your repositories according to your current permissions.
Create a separate token per machine if you want to revoke them independently.
Tokens expire after 90 days. Existing repository tokens retain their original
scope until expiry or revocation.

### Branches and Git LFS

New repositories default to `main` and accept fast-forward pushes from writers.
Managers can choose the default branch and require PR review in repository
settings. Existing repositories keep their protection after upgrade. Force
pushes and deletion of the default branch are blocked.

Run `git lfs install` on each machine and use the same HTTPS remote and
credential for Git and LFS. Use `git lfs track` and commit `.gitattributes` when
adding tracked file types. LOKI keeps historical LFS objects for the life of
the repository. Forks and reviewed pull requests transfer the LFS objects
needed by the history they copy.

### Move an existing repository

Create an empty LOKI repository with the same default branch as the source.
Use its clone URL in the following commands:

```sh
git clone --mirror SOURCE_URL project.git
cd project.git
git remote add loki LOKI_CLONE_URL
git lfs fetch --all origin
git lfs push --all loki
git push loki 'refs/heads/*:refs/heads/*'
git push loki 'refs/tags/*:refs/tags/*'
```

Check `.lfsconfig` and local Git configuration for overrides pointing to the
old LFS endpoint. Test the newest revision and an older asset-heavy commit
from a clean clone with an empty LFS cache, then repeat from a restored backup
before retiring the old remote.

### Storage and recovery

Use **Server settings → Git storage** to adjust transfer and storage limits.
Git defaults to 64 MiB per push, 1 GiB per repo, and 20 GiB combined. Raise
the applicable limits for a large first import. LFS defaults to 4 GiB per
object, 20 GiB per repo, and 50 GiB combined. Transfers keep a 2 GiB free
space buffer on the data volume.

Backups include Git history and LFS objects. Keep an off-host copy somewhere
private. For archives over the 2 GiB browser import limit, use offline restore
into an empty directory. Restore verifies Git integrity and recorded LFS
hashes. Full installation backups support 100 GiB of uncompressed data.

```sh
loki-server --data-dir NEW_EMPTY_DIRECTORY restore BACKUP.zip
```

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
