use crate::common::{self, Browser, MEMBER_PASSWORD, PASSWORD, Server, csrf};
use anyhow::{Context, Result, ensure};
use reqwest::{StatusCode, blocking::multipart};
use serde_json::{Value, json};
use std::{fs, io::Read, path::Path};

fn get(owner: &Browser, url: &str) -> Result<common::Reply> {
    let reply = owner.get(url)?;
    reply.ok()?;
    Ok(reply)
}
fn form(owner: &Browser, url: &str, fields: &[(&str, &str)]) -> Result<common::Reply> {
    let reply = owner.form(url, fields)?;
    reply.ok()?;
    Ok(reply)
}
fn app_state(owner: &Browser, app: &str) -> Result<Value> {
    owner.api(&format!("{app}/api/state"))
}
fn member(server: &Server, owner: &Browser, id: &str) -> Result<Browser> {
    let mut member = Browser::new(server.port)?;
    member.session(&server.origin)?;
    member.post_api(
        &format!("{}/api/register", server.origin),
        &json!({"username":"member","password":MEMBER_PASSWORD}),
    )?;
    let state = owner.api(&format!("{}/api/state", server.origin))?;
    let uid = state["users"]
        .as_array()
        .and_then(|users| users.iter().find(|u| u["username"] == "member"))
        .and_then(|u| u["id"].as_str())
        .context("Missing member")?;
    owner.action(
        &server.origin,
        "user-status",
        uid,
        json!({"status":"approved"}),
    )?;
    owner.action(
        &server.origin,
        "resource-access",
        id,
        json!({"visibility":"members","grants":[]}),
    )?;
    member.login(&server.origin, "member", MEMBER_PASSWORD)?;
    Ok(member)
}
pub fn setup(binary: &Path) -> Result<()> {
    let mut server = Server::new(binary, "setup-smoke")?;
    let source = server.data.clone();
    let selected = server.base.join("selected");
    server.expect_restart(&selected)?;
    let mut owner = Browser::new(server.port)?;
    owner.session(&server.origin)?;
    let input = json!({"token":fs::read_to_string(source.join("setup-token.txt"))?,"username":"owner","password":PASSWORD,"name":"Setup test","public_url":"https://loki.example.test","data_dir":selected,"startup":false});
    let result = owner.post_api(&format!("{}/api/setup", server.origin), &input)?;
    ensure!(
        result["restarting"] == true,
        "Setup did not request restart: {result}"
    );
    server.finish_restart(&selected)?;
    ensure!(
        owner.session(&server.origin)?["setup"] == false,
        "Relocation lost owner setup"
    );
    owner.login(&server.origin, "owner", PASSWORD)?;
    ensure!(
        owner.session(&server.origin)?["user"]["owner"] == true,
        "Local owner login failed with HTTPS issuer"
    );
    let state = owner.api(&format!("{}/api/state", server.origin))?;
    ensure!(
        state["data_dir"]
            .as_str()
            .is_some_and(|p| p.contains("selected")),
        "Wrong selected data directory"
    );
    let repeated = owner.post(&format!("{}/api/setup", server.origin), &input)?;
    ensure!(
        repeated.status == StatusCode::CONFLICT && repeated.text().contains("already set up"),
        "Repeated setup was not rejected"
    );
    ensure!(
        source.join("loki.db").is_file() && selected.join("loki.db").is_file(),
        "Relocation did not retain source"
    );
    ensure!(
        !selected.join("setup-token.txt").exists(),
        "Setup token remained after completion"
    );
    println!(
        "PASS setup, preserved source, custom-directory restart, HTTPS issuer/local cookies and repeated-setup rejection\n{}",
        server.base.display()
    );
    Ok(())
}
pub fn storage(binary: &Path, package: &Path) -> Result<()> {
    let mut server = Server::new(binary, "storage-smoke")?;
    let owner = server.initialize()?;
    let app = server.install(&owner, package, "storage-notes")?;
    let token = csrf(&get(&owner, &format!("{app}/"))?)?;
    ensure!(
        owner.get(&format!("{app}/export"))?.status == StatusCode::NOT_FOUND,
        "Missing export lost typed host error"
    );
    form(
        &owner,
        &format!("{app}/preferences"),
        &[("gold", "on"), ("csrf_token", &token)],
    )?;
    form(
        &owner,
        &format!("{app}/notes"),
        &[("text", "A persistent branch — 枝"), ("csrf_token", &token)],
    )?;
    let exported = form(&owner, &format!("{app}/export"), &[("csrf_token", &token)])?.body;
    ensure!(
        exported == "A persistent branch — 枝".as_bytes(),
        "Wrong exported note"
    );
    let range = owner
        .client
        .get(format!("{app}/export"))
        .header("Range", "bytes=0-3")
        .send()?;
    ensure!(
        range.status() == StatusCode::PARTIAL_CONTENT && range.bytes()?.as_ref() == b"A pe",
        "Generated-file range failed"
    );
    let saved = app_state(&owner, &app)?;
    ensure!(
        saved["preferences"]["gold"] == true
            && saved["notes"].as_array().is_some_and(|n| n.len() == 1),
        "Storage state did not persist"
    );
    let member = member(&server, &owner, "storage-notes")?;
    get(&member, &format!("{app}/"))?;
    ensure!(
        app_state(&member, &app)? == json!({"preferences":{"gold":false},"notes":[]}),
        "Per-user storage isolation failed"
    );
    ensure!(
        member.get(&format!("{app}/export"))?.status == StatusCode::NOT_FOUND,
        "Member saw owner's export"
    );
    println!(
        "PASS SDK identity, JSON preferences, SQLite notes, generated exports, ranges and per-user isolation"
    );
    owner.action(
        &server.origin,
        "app-action",
        "storage-notes",
        json!({"operation":"restart"}),
    )?;
    server.app_ready(&owner, "storage-notes")?;
    ensure!(app_state(&owner, &app)? == saved, "App restart lost state");
    let broken = server.base.join("broken.loki-app");
    common::broken_package(package, &broken, |m| m["health"] = json!("/missing-health"))?;
    ensure!(
        owner
            .upload(
                &format!("{}/api/apps/import", server.origin),
                "package",
                &broken
            )?
            .status
            == StatusCode::BAD_REQUEST,
        "Invalid update was accepted"
    );
    server.app_ready(&owner, "storage-notes")?;
    ensure!(
        app_state(&owner, &app)? == saved
            && get(&owner, &format!("{app}/export"))?.body == exported,
        "Rollback lost storage"
    );
    let backup = owner.action(&server.origin, "backup", "", json!({}))?["file"]
        .as_str()
        .context("Missing backup name")?
        .to_string();
    let archive = zip::ZipArchive::new(fs::File::open(server.data.join("backups").join(&backup))?)?;
    for prefix in [
        "app-data/storage-notes/storage/values/",
        "app-data/storage-notes/storage/files/",
    ] {
        ensure!(
            archive.file_names().any(|p| p.starts_with(prefix)),
            "Backup omitted {prefix}"
        );
    }
    drop(archive);
    server.restore_offline(&backup)?;
    server.app_ready(&owner, "storage-notes")?;
    ensure!(
        app_state(&owner, &app)? == saved
            && get(&owner, &format!("{app}/export"))?.body == exported,
        "Fresh restore lost storage"
    );
    println!(
        "PASS restart, failed-update rollback and stopped-app backup/fresh restore of JSON, files and SQL"
    );
    owner.action(
        &server.origin,
        "resource-access",
        "storage-notes",
        json!({"visibility":"restricted","grants":[]}),
    )?;
    ensure!(
        member.get(&format!("{app}/export"))?.status == StatusCode::FORBIDDEN,
        "Revoked member retained export access"
    );
    println!(
        "PASS immediate generated-file access revocation\n{}",
        server.data.display()
    );
    Ok(())
}
pub fn wasi(binary: &Path, package: &Path, keep_running: bool) -> Result<()> {
    let mut server = Server::new(binary, "wasi-smoke")?;
    let owner = server.initialize()?;
    let app = server.install(&owner, package, "bigshot-voter")?;
    println!("PASS package import, WASI readiness and shared-server hosting");
    get(&owner, &format!("{app}/"))?;
    let panels = get(&owner, &format!("{app}/panels"))?;
    ensure!(panels.url.ends_with("/panels"), "Host SSO did not complete");
    let token = csrf(&get(&owner, &format!("{app}/admin/panels/new"))?)?;
    let created = form(
        &owner,
        &format!("{app}/admin/panels/new"),
        &[
            ("title", "First story"),
            ("description", "A **LOKI** integration test"),
            ("media_type", "other"),
            ("csrf_token", &token),
        ],
    )?;
    let panel = common::capture(r"/panels/([^/?]+)", &created.url)?;
    let entry = multipart::Form::new()
        .text("title", "Owner entry")
        .text("csrf_token", token.clone())
        .part(
            "file",
            multipart::Part::bytes(b"Hello, multiverse.".to_vec())
                .file_name("story.txt")
                .mime_str("text/plain")?,
        )
        .part(
            "master",
            multipart::Part::bytes(b"source-file".to_vec())
                .file_name("source.kra")
                .mime_str("application/octet-stream")?,
        );
    owner
        .multipart(&format!("{app}/panels/{panel}/entries"), entry)?
        .ok()?;
    let db = server.data.join("app-data/bigshot-voter/app.db");
    let (entry_id, entry_path) = common::sql_pair(
        &db,
        "SELECT id,stored_path FROM entries WHERE panel_id=?",
        &panel,
    )?;
    ensure!(
        get(&owner, &format!("{app}/media/{entry_id}"))?.body == b"Hello, multiverse.",
        "Media download changed content"
    );
    ensure!(
        get(&owner, &format!("{app}/media/{entry_id}/master"))?.body == b"source-file",
        "Master download changed content"
    );
    let range = owner
        .client
        .get(format!("{app}/media/{entry_id}"))
        .header("Range", "bytes=0-4")
        .send()?;
    ensure!(
        range.status() == StatusCode::PARTIAL_CONTENT && range.bytes()?.as_ref() == b"Hello",
        "Media range failed"
    );
    ensure!(
        owner
            .form(
                &format!("{app}/panels/{panel}/votes"),
                &[("entry_id", &entry_id), ("csrf_token", &token)]
            )?
            .status
            == StatusCode::BAD_REQUEST,
        "Owner voted for own entry"
    );
    println!("PASS media/master downloads, ranges and own-vote rejection");
    let member = member(&server, &owner, "bigshot-voter")?;
    // Exercise the default Members group grant separately from audience=members.
    owner.action(&server.origin,"resource-access","bigshot-voter",json!({"visibility":"restricted","grants":[{"subject_kind":"group","subject":"members","role":"access"}]}))?;
    get(&member, &format!("{app}/"))?;
    let panels = get(&member, &format!("{app}/panels"))?;
    ensure!(
        panels.url.ends_with("/panels"),
        "Member SSO did not complete"
    );
    let page = get(&member, &format!("{app}/panels/{panel}"))?;
    let member_csrf = csrf(&page)?;
    let visible_entry = common::capture(r#"name="entry_id" value="([^"]+)""#, &page.text())?;
    ensure!(visible_entry == entry_id, "Member saw different entry");
    form(
        &member,
        &format!("{app}/panels/{panel}/votes"),
        &[("entry_id", &entry_id), ("csrf_token", &member_csrf)],
    )?;
    ensure!(
        member.get(&format!("{app}/admin"))?.status == StatusCode::FORBIDDEN,
        "Member gained app-admin role"
    );
    form(
        &owner,
        &format!("{app}/admin/panels/{panel}/close"),
        &[("csrf_token", &token)],
    )?;
    ensure!(
        member
            .form(
                &format!("{app}/panels/{panel}/votes"),
                &[("entry_id", &entry_id), ("csrf_token", &member_csrf)]
            )?
            .status
            == StatusCode::BAD_REQUEST,
        "Closed panel accepted vote"
    );
    form(
        &owner,
        &format!("{app}/admin/panels/{panel}/open"),
        &[("csrf_token", &token)],
    )?;
    form(
        &member,
        &format!("{app}/panels/{panel}/votes"),
        &[("entry_id", &entry_id), ("csrf_token", &member_csrf)],
    )?;
    ensure!(
        common::votes(&db)? == 1,
        "Vote update inserted duplicate vote"
    );
    println!("PASS scoped SSO, admin separation, panel close/reopen and single-vote update");
    owner.action(
        &server.origin,
        "resource-access",
        "bigshot-voter",
        json!({"visibility":"restricted","grants":[]}),
    )?;
    let denied = member.get(&format!("{app}/panels/{panel}"))?;
    ensure!(
        !(denied.status == StatusCode::OK && denied.url.ends_with(&panel)),
        "Revoked user retained panel access"
    );
    ensure!(
        member.get(&format!("{app}/media/{entry_id}"))?.status == StatusCode::FORBIDDEN,
        "Revoked member retained direct media access"
    );
    owner.action(
        &server.origin,
        "resource-access",
        "bigshot-voter",
        json!({"visibility":"members","grants":[]}),
    )?;
    println!("PASS immediate app/direct-media revocation");
    let original_manifest = common::package_manifest(package)?;
    let broken = server.base.join("broken.loki-app");
    common::broken_package(package, &broken, |m| {
        m["health"] = json!("/missing-health-endpoint");
        m["version"] = json!("0.5.1");
        m["fields"].as_array_mut().unwrap().push(json!({"key":"experimental","label":"Experimental setting","kind":"bool","required":false,"default":true}));
    })?;
    ensure!(
        owner
            .upload(
                &format!("{}/api/apps/import", server.origin),
                "package",
                &broken
            )?
            .status
            == StatusCode::BAD_REQUEST,
        "Broken update was accepted"
    );
    server.app_ready(&owner, "bigshot-voter")?;
    let state = owner.api(&format!("{}/api/state", server.origin))?;
    let current = common::resource(&state, "bigshot-voter")?;
    ensure!(
        current["manifest"]["version"] == original_manifest["version"]
            && current["config"] == json!({"max-upload":5242880})
            && common::votes(&db)? == 1,
        "Rollback lost package, settings or vote"
    );
    println!("PASS failed update restores prior component, configuration and voting data");
    let shared = server.base.join("shared");
    fs::create_dir(&shared)?;
    fs::write(
        shared.join("Welcome.txt"),
        "Your services, held together.\n",
    )?;
    let share = owner.action(
        &server.origin,
        "share-save",
        "",
        json!({"name":"The Archive","root":shared}),
    )?;
    owner.action(
        &server.origin,
        "resource-access",
        share["id"].as_str().unwrap(),
        json!({"visibility":"members","grants":[]}),
    )?;
    let large = fs::OpenOptions::new().write(true).truncate(true).open(
        server
            .data
            .join("app-data/bigshot-voter/uploads")
            .join(&entry_path),
    )?;
    large.set_len(128 * 1024 * 1024)?;
    drop(large);
    let pid = server.pid()?;
    let (before, _) = crate::benchmark::sample(pid)?;
    let mut peak = before;
    let mut response = owner
        .client
        .get(format!("{app}/media/{entry_id}"))
        .send()?
        .error_for_status()?;
    let mut count = 0u64;
    let mut buffer = [0; 65536];
    let mut next_sample = 1048576;
    loop {
        let n = response.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        count += n as u64;
        if count >= next_sample {
            peak = peak.max(crate::benchmark::sample(pid)?.0);
            next_sample = count + 1048576;
        }
    }
    ensure!(
        count == 128 * 1024 * 1024 && peak.saturating_sub(before) < 16 * 1024 * 1024,
        "128 MiB streaming used too much memory: {before} -> {peak} ({count} bytes)"
    );
    let growth = peak.saturating_sub(before) as f64 / 1048576.0;
    println!("PASS 128 MiB streamed download; resident growth {growth:.2} MiB");
    let backup = owner.action(&server.origin, "backup", "", json!({}))?["file"]
        .as_str()
        .context("Missing backup")?
        .to_string();
    let imported = owner
        .upload(
            &format!("{}/api/backups/import", server.origin),
            "backup",
            &server.data.join("backups").join(backup),
        )?
        .json()?;
    let backup = imported["file"]
        .as_str()
        .context("Missing imported backup")?
        .to_owned();
    println!("PASS stopped-app backup, resume, local backup import and archive validation");
    let restored = server.base.join("restored");
    server.expect_restart(&restored)?;
    owner.action(
        &server.origin,
        "storage",
        "",
        json!({"target":restored,"backup":backup,"current_password":PASSWORD}),
    )?;
    server.finish_restart(&restored)?;
    server.app_ready(&owner, "bigshot-voter")?;
    let state = owner.api(&format!("{}/api/state", server.origin))?;
    ensure!(
        state["data_dir"]
            .as_str()
            .is_some_and(|d| d.contains("restored"))
            && common::votes(&server.data.join("app-data/bigshot-voter/app.db"))? == 1,
        "Web restore/restart lost vote"
    );
    println!("PASS web backup restore to a new directory and supervised restart");
    common::report(
        "wasi-smoke-last.json",
        &json!({"origin":server.origin,"app":app,"data":server.data,"pid":server.pid()?,"runtime":"wasi","stream_resident_growth_mib":growth,"backup":backup}),
    )?;
    if keep_running {
        println!("Temporary preview login: owner / {PASSWORD}");
        server.keep();
    }
    Ok(())
}
