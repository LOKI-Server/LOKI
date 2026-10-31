use loki_sdk::{Database, Error, Request, Response, Result, Router, Storage, params};
use serde::{Deserialize, Serialize};

#[derive(Default, Serialize, Deserialize)]
struct Preferences {
    #[serde(default)]
    gold: bool,
}
#[derive(Serialize, Deserialize)]
struct Note {
    id: String,
    text: String,
}
#[derive(askama::Template)]
#[template(path = "home.html")]
struct Page {
    username: String,
    csrf: String,
    gold: bool,
    notes: Vec<Note>,
}

fn preferences(user_id: &str) -> Result<Preferences> {
    Ok(Storage
        .get_json(&format!("prefs-{user_id}"))?
        .unwrap_or_default())
}
fn notes(user_id: &str) -> Result<Vec<Note>> {
    Database.fetch_all(
        "SELECT id,text FROM notes WHERE user_id=? ORDER BY rowid DESC LIMIT 100",
        params![user_id],
    )
}
fn export_path(user_id: &str) -> String {
    format!("exports/{user_id}.txt")
}
fn health(_: Request) -> Result<Response> {
    Database.execute("CREATE TABLE IF NOT EXISTS notes(id TEXT PRIMARY KEY,user_id TEXT NOT NULL,text TEXT NOT NULL)",params![])?;
    Ok(Response::text("ready"))
}
fn home(request: Request) -> Result<Response> {
    let user = request.context().require_user()?;
    Response::render(Page {
        username: if user.name.trim().is_empty() {
            user.username.clone()
        } else {
            user.name.clone()
        },
        csrf: request.context().csrf_token().into(),
        gold: preferences(&user.id)?.gold,
        notes: notes(&user.id)?,
    })
}
fn save_preferences(request: Request) -> Result<Response> {
    let user = request.context().require_user()?;
    let form = request.form()?;
    Storage.set_json(
        &format!("prefs-{}", user.id),
        &Preferences {
            gold: form.field("gold") == Some("on"),
        },
    )?;
    Response::redirect("/")
}
fn add_note(request: Request) -> Result<Response> {
    let user = request.context().require_user()?;
    let form = request.form()?;
    let text = form.required("text")?.trim();
    if text.is_empty() || text.len() > 4096 {
        return Err(Error::bad("Write a note of 1–4,096 UTF-8 bytes."));
    }
    Database.execute(
        "INSERT INTO notes(id,user_id,text) VALUES(?,?,?)",
        params![loki_sdk::random_id(), user.id, text],
    )?;
    Response::redirect("/")
}
fn delete_note(request: Request) -> Result<Response> {
    let user = request.context().require_user()?;
    Database.execute(
        "DELETE FROM notes WHERE id=? AND user_id=?",
        params![request.param("id")?, user.id],
    )?;
    Response::redirect("/")
}
fn make_export(request: Request) -> Result<Response> {
    let user = request.context().require_user()?;
    let text = notes(&user.id)?
        .into_iter()
        .map(|note| note.text)
        .collect::<Vec<_>>()
        .join("\n\n");
    Storage.write(&export_path(&user.id), text.as_bytes())?;
    Response::redirect("/export")
}
fn download_export(request: Request) -> Result<Response> {
    let user = request.context().require_user()?;
    Storage
        .file(export_path(&user.id))
        .download("my-branch-notes.txt")
}
fn state(request: Request) -> Result<Response> {
    let user = request.context().require_user()?;
    #[derive(Serialize)]
    struct State {
        preferences: Preferences,
        notes: Vec<Note>,
    }
    Response::json(&State {
        preferences: preferences(&user.id)?,
        notes: notes(&user.id)?,
    })
}
fn stylesheet(_: Request) -> Result<Response> {
    loki_sdk::asset("static/style.css")
}
fn handle(request: Request) -> Result<Response> {
    Router::new()
        .get("/health", health)
        .get("/", home)
        .post("/preferences", save_preferences)
        .post("/notes", add_note)
        .post("/notes/{id}/delete", delete_note)
        .post("/export", make_export)
        .get("/export", download_export)
        .get("/api/state", state)
        .get("/static/style.css", stylesheet)
        .handle(request)
}
loki_sdk::export!(handle);
