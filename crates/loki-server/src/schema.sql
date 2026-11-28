CREATE TABLE IF NOT EXISTS settings(id INTEGER PRIMARY KEY CHECK(id=1), json TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS users(
 id TEXT PRIMARY KEY, username TEXT NOT NULL COLLATE NOCASE UNIQUE,
 display_name TEXT NOT NULL, email TEXT NOT NULL DEFAULT '', password TEXT NOT NULL,
 owner INTEGER NOT NULL DEFAULT 0, status TEXT NOT NULL CHECK(status IN('pending','approved','rejected','suspended')),
 avatar INTEGER NOT NULL DEFAULT 0, totp TEXT, totp_pending TEXT, totp_step INTEGER NOT NULL DEFAULT 0, created INTEGER NOT NULL,
 bio TEXT NOT NULL DEFAULT '', auth_version INTEGER NOT NULL DEFAULT 0,
 rejected_at INTEGER
);
CREATE UNIQUE INDEX IF NOT EXISTS sole_owner ON users(owner) WHERE owner=1;
CREATE UNIQUE INDEX IF NOT EXISTS unique_email ON users(email COLLATE NOCASE) WHERE email!='';
CREATE TABLE IF NOT EXISTS sessions(id TEXT PRIMARY KEY, user_id TEXT REFERENCES users(id) ON DELETE CASCADE, csrf TEXT NOT NULL, created INTEGER NOT NULL, expires INTEGER NOT NULL, label TEXT NOT NULL, auth_version INTEGER NOT NULL DEFAULT 0);
CREATE INDEX IF NOT EXISTS sessions_user ON sessions(user_id);
CREATE TABLE IF NOT EXISTS groups(id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE, icon INTEGER NOT NULL DEFAULT 0);
INSERT OR IGNORE INTO groups(id,name) VALUES('members','Members');
CREATE TABLE IF NOT EXISTS memberships(user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE, group_id TEXT NOT NULL REFERENCES groups(id) ON DELETE CASCADE, PRIMARY KEY(user_id,group_id));
CREATE TABLE IF NOT EXISTS resources(id TEXT PRIMARY KEY, kind TEXT NOT NULL, name TEXT NOT NULL, visibility TEXT NOT NULL DEFAULT 'restricted' CHECK(visibility IN('public','members','restricted')), icon INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS grants(resource TEXT NOT NULL REFERENCES resources(id) ON DELETE CASCADE, subject TEXT NOT NULL, subject_kind TEXT NOT NULL CHECK(subject_kind IN('user','group')), role TEXT NOT NULL DEFAULT 'access', PRIMARY KEY(resource,subject,subject_kind,role));
CREATE TABLE IF NOT EXISTS shares(id TEXT PRIMARY KEY REFERENCES resources(id) ON DELETE CASCADE, root TEXT NOT NULL, excluded_extensions TEXT NOT NULL DEFAULT '[]', share_type TEXT NOT NULL DEFAULT 'files' CHECK(share_type IN ('files','apps')));
CREATE TABLE IF NOT EXISTS routes(id TEXT PRIMARY KEY REFERENCES resources(id) ON DELETE CASCADE, hostname TEXT NOT NULL UNIQUE, upstream TEXT NOT NULL, enabled INTEGER NOT NULL DEFAULT 0, gated INTEGER NOT NULL DEFAULT 0, service TEXT NOT NULL DEFAULT '', service_kind TEXT NOT NULL DEFAULT 'none');
CREATE TABLE IF NOT EXISTS apps(id TEXT PRIMARY KEY REFERENCES resources(id) ON DELETE CASCADE, manifest TEXT NOT NULL, enabled INTEGER NOT NULL DEFAULT 0, hostname TEXT NOT NULL DEFAULT '', config TEXT NOT NULL DEFAULT '{}', port INTEGER NOT NULL, secret TEXT NOT NULL, previous TEXT, error TEXT NOT NULL DEFAULT '', previous_config TEXT, lan_bind TEXT NOT NULL DEFAULT '', source TEXT NOT NULL DEFAULT 'upload', storage_mib INTEGER);
CREATE TABLE IF NOT EXISTS clients(id TEXT PRIMARY KEY, secret TEXT NOT NULL, resource TEXT NOT NULL REFERENCES resources(id) ON DELETE CASCADE, redirect TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS codes(id TEXT PRIMARY KEY, client TEXT NOT NULL REFERENCES clients(id) ON DELETE CASCADE, user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE, redirect TEXT NOT NULL, challenge TEXT NOT NULL, nonce TEXT NOT NULL, scope TEXT NOT NULL, expires INTEGER NOT NULL, auth_version INTEGER NOT NULL DEFAULT 0, login_session TEXT);
CREATE TABLE IF NOT EXISTS tokens(id TEXT PRIMARY KEY, client TEXT NOT NULL REFERENCES clients(id) ON DELETE CASCADE, user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE, scope TEXT NOT NULL, expires INTEGER NOT NULL, family TEXT NOT NULL, kind TEXT NOT NULL, used INTEGER NOT NULL DEFAULT 0, auth_version INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS token_families(id TEXT PRIMARY KEY,expires INTEGER NOT NULL,revoked INTEGER NOT NULL DEFAULT 0,public INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS resets(id TEXT PRIMARY KEY, user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE, expires INTEGER NOT NULL, activation INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS recovery_codes(user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE, hash TEXT NOT NULL UNIQUE);
CREATE TABLE IF NOT EXISTS gateway_sessions(id TEXT PRIMARY KEY, resource TEXT NOT NULL REFERENCES resources(id) ON DELETE CASCADE, user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE, expires INTEGER NOT NULL, login_session TEXT);
CREATE TABLE IF NOT EXISTS gateway_flows(id TEXT PRIMARY KEY, resource TEXT NOT NULL REFERENCES resources(id) ON DELETE CASCADE, verifier TEXT NOT NULL, expires INTEGER NOT NULL, next TEXT NOT NULL DEFAULT '/', popup INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS audit(id INTEGER PRIMARY KEY AUTOINCREMENT, actor TEXT NOT NULL, action TEXT NOT NULL, detail TEXT NOT NULL, created INTEGER NOT NULL);
INSERT OR IGNORE INTO resources(id,kind,name,visibility) VALUES('sync','sync','Save sync','restricted');
CREATE TABLE IF NOT EXISTS sync_devices(id TEXT PRIMARY KEY, user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE, name TEXT NOT NULL, platform TEXT NOT NULL DEFAULT '', token TEXT NOT NULL UNIQUE, auth_version INTEGER NOT NULL, created INTEGER NOT NULL, last_seen INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS sync_devices_user ON sync_devices(user_id);
CREATE TABLE IF NOT EXISTS file_devices(id TEXT PRIMARY KEY, user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE, name TEXT NOT NULL, platform TEXT NOT NULL DEFAULT '', token TEXT NOT NULL UNIQUE, auth_version INTEGER NOT NULL, created INTEGER NOT NULL, last_seen INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS file_devices_user ON file_devices(user_id);
CREATE TABLE IF NOT EXISTS devices(id TEXT PRIMARY KEY, user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE, name TEXT NOT NULL, platform TEXT NOT NULL DEFAULT '', token TEXT NOT NULL UNIQUE, auth_version INTEGER NOT NULL, created INTEGER NOT NULL, last_seen INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS devices_user ON devices(user_id);
CREATE TABLE IF NOT EXISTS api_device_grants(device_id TEXT PRIMARY KEY REFERENCES devices(id) ON DELETE CASCADE,scope TEXT NOT NULL CHECK(scope IN('repositories','owner')),expires INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS device_codes(id TEXT PRIMARY KEY, user_code TEXT NOT NULL UNIQUE, kind TEXT NOT NULL, client TEXT NOT NULL DEFAULT '', scope TEXT NOT NULL DEFAULT '', name TEXT NOT NULL, platform TEXT NOT NULL, user_id TEXT REFERENCES users(id) ON DELETE CASCADE, auth_version INTEGER, polled INTEGER NOT NULL DEFAULT 0, expires INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS sync_folders(id TEXT PRIMARY KEY, user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE, name TEXT NOT NULL, kind TEXT NOT NULL DEFAULT 'generic', keep_versions INTEGER NOT NULL DEFAULT 10, seq INTEGER NOT NULL DEFAULT 0, created INTEGER NOT NULL, UNIQUE(user_id,name));
CREATE TABLE IF NOT EXISTS sync_files(folder TEXT NOT NULL REFERENCES sync_folders(id) ON DELETE CASCADE, path TEXT NOT NULL, version INTEGER NOT NULL, hash TEXT, size INTEGER NOT NULL, mtime INTEGER NOT NULL, device TEXT NOT NULL, seq INTEGER NOT NULL, updated INTEGER NOT NULL, PRIMARY KEY(folder,path));
CREATE INDEX IF NOT EXISTS sync_files_seq ON sync_files(folder,seq);
CREATE INDEX IF NOT EXISTS sync_files_hash ON sync_files(hash);
CREATE TABLE IF NOT EXISTS sync_history(id INTEGER PRIMARY KEY AUTOINCREMENT, folder TEXT NOT NULL REFERENCES sync_folders(id) ON DELETE CASCADE, path TEXT NOT NULL, version INTEGER NOT NULL, hash TEXT, size INTEGER NOT NULL, mtime INTEGER NOT NULL, device TEXT NOT NULL, created INTEGER NOT NULL, reason TEXT NOT NULL CHECK(reason IN('replaced','deleted','conflict')), resolved INTEGER NOT NULL DEFAULT 1, since INTEGER NOT NULL DEFAULT 0, stashed INTEGER NOT NULL DEFAULT 0);
CREATE INDEX IF NOT EXISTS sync_history_path ON sync_history(folder,path,id);
CREATE INDEX IF NOT EXISTS sync_history_hash ON sync_history(hash);
CREATE TABLE IF NOT EXISTS sync_uploads(user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE, hash TEXT NOT NULL, size INTEGER NOT NULL, created INTEGER NOT NULL, PRIMARY KEY(user_id,hash));
CREATE INDEX IF NOT EXISTS sync_uploads_hash ON sync_uploads(hash);
CREATE TABLE IF NOT EXISTS sync_quotas(user_id TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE, bytes INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS personal_apps(app_id TEXT PRIMARY KEY REFERENCES apps(id) ON DELETE CASCADE);
CREATE TABLE IF NOT EXISTS personal_files(user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE, app_id TEXT NOT NULL, path TEXT NOT NULL, blob TEXT NOT NULL UNIQUE, size INTEGER NOT NULL CHECK(size>=0), PRIMARY KEY(user_id,app_id,path));

INSERT OR IGNORE INTO resources(id,kind,name,visibility) VALUES('vcs-create','vcs-create','Create Skald repositories','restricted');
INSERT OR IGNORE INTO resources(id,kind,name,visibility) VALUES('vcs-group-create','vcs-group-create','Create Skald repositories in groups','restricted');
CREATE TABLE IF NOT EXISTS skald_repos(id TEXT PRIMARY KEY REFERENCES resources(id),owner_id TEXT NOT NULL REFERENCES users(id),name TEXT NOT NULL,created INTEGER NOT NULL,protect_default INTEGER NOT NULL DEFAULT 0,upstream TEXT REFERENCES skald_repos(id) ON DELETE SET NULL,bytes INTEGER NOT NULL DEFAULT 0,object_count INTEGER NOT NULL DEFAULT 0,group_id TEXT REFERENCES groups(id),UNIQUE(owner_id,name));
-- A pending change of owner. While one exists, code changes to the repository are paused.
CREATE TABLE IF NOT EXISTS vcs_group_quotas(group_id TEXT PRIMARY KEY REFERENCES groups(id) ON DELETE CASCADE,repo_bytes INTEGER CHECK(repo_bytes IS NULL OR repo_bytes>0),total_bytes INTEGER CHECK(total_bytes IS NULL OR total_bytes>0));
CREATE TABLE IF NOT EXISTS vcs_quotas(user_id TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,repo_bytes INTEGER CHECK(repo_bytes IS NULL OR repo_bytes>0),total_bytes INTEGER CHECK(total_bytes IS NULL OR total_bytes>0));
CREATE TABLE IF NOT EXISTS skald_transfers(repo_id TEXT PRIMARY KEY REFERENCES skald_repos(id) ON DELETE CASCADE,to_kind TEXT NOT NULL CHECK(to_kind IN('user','group')),to_id TEXT NOT NULL,requested_by TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,created INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS skald_objects(repo_id TEXT NOT NULL REFERENCES skald_repos(id) ON DELETE CASCADE,object TEXT NOT NULL,size INTEGER NOT NULL CHECK(size>0),created INTEGER NOT NULL,PRIMARY KEY(repo_id,object));
CREATE TABLE IF NOT EXISTS vcs_tokens(id TEXT PRIMARY KEY,digest TEXT NOT NULL UNIQUE,user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,label TEXT NOT NULL,write INTEGER NOT NULL,created INTEGER NOT NULL,expires INTEGER NOT NULL,auth_version INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS vcs_tokens_user ON vcs_tokens(user_id);
CREATE TABLE IF NOT EXISTS vcs_device_tokens(id TEXT PRIMARY KEY,digest TEXT NOT NULL UNIQUE,device_id TEXT NOT NULL REFERENCES api_device_grants(device_id) ON DELETE CASCADE,repo_id TEXT NOT NULL REFERENCES skald_repos(id) ON DELETE CASCADE,write INTEGER NOT NULL,expires INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS vcs_device_tokens_device ON vcs_device_tokens(device_id);

CREATE TABLE IF NOT EXISTS skald_items(seq INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT NOT NULL UNIQUE,repo_id TEXT NOT NULL REFERENCES skald_repos(id) ON DELETE CASCADE,author_id TEXT NOT NULL REFERENCES users(id),kind TEXT NOT NULL CHECK(kind IN('issue','pull')),title TEXT NOT NULL,body TEXT NOT NULL,status TEXT NOT NULL DEFAULT 'open',created INTEGER NOT NULL,source_branch TEXT,base_ref TEXT,base TEXT,head TEXT,approved_head TEXT,reviewer_id TEXT REFERENCES users(id));
CREATE INDEX IF NOT EXISTS skald_items_repo ON skald_items(repo_id,seq);
CREATE TABLE IF NOT EXISTS skald_comments(seq INTEGER PRIMARY KEY AUTOINCREMENT,item_id TEXT NOT NULL REFERENCES skald_items(id) ON DELETE CASCADE,author_id TEXT NOT NULL REFERENCES users(id),body TEXT NOT NULL,created INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS skald_comments_item ON skald_comments(item_id,seq);
