use crate::{Error, Result};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::collections::BTreeMap;
use wasi::http::types::{
    Fields, IncomingRequest, OutgoingBody, OutgoingResponse, ResponseOutparam,
};

#[doc(hidden)]
pub(crate) mod bindings {
    wit_bindgen::generate!({path:"wit",world:"host-api"});
}

/// Calls to the typed host API (`loki:app@0.3.1`). In native tests a
/// [`testing::with_host`] fake answers them instead, by operation name and JSON.
// Arguments go unused on targets that are neither WASI nor a test.
#[allow(unused_variables)]
pub(crate) mod host {
    use super::*;
    use crate::{FileInfo, FilePage, StorageUsage};
    #[cfg(target_arch = "wasm32")]
    use bindings::loki::app as wit;
    #[cfg(any(test, feature = "testing"))]
    use serde_json::json;

    /// Tries the test fake, then the WASI host.
    macro_rules! host {
        ($operation:literal, $input:expr, |$fake:ident: $output:ty| $convert:expr, $real:expr) => {{
            #[cfg(any(test, feature = "testing"))]
            if let Some(result) = testing::fake::<$output>($operation, $input) {
                return result.and_then(|$fake| $convert);
            }
            #[cfg(target_arch = "wasm32")]
            {
                let result: Result<_> = $real;
                return result;
            }
            #[cfg(not(target_arch = "wasm32"))]
            return Err(no_host());
        }};
    }
    #[cfg(not(target_arch = "wasm32"))]
    fn no_host() -> Error {
        Error::internal(
            "LOKI services require a WASI host. Use loki_sdk::testing for native unit tests.",
        )
    }
    #[cfg(target_arch = "wasm32")]
    impl From<wit::types::Error> for Error {
        fn from(error: wit::types::Error) -> Self {
            Error::new(error.status, error.message)
        }
    }
    #[cfg(target_arch = "wasm32")]
    impl From<wit::types::User> for User {
        fn from(user: wit::types::User) -> Self {
            User {
                id: user.id,
                username: user.username,
                name: user.name,
                email: user.email,
                bio: user.bio,
                avatar: user.avatar,
                roles: user.roles,
            }
        }
    }
    #[cfg(target_arch = "wasm32")]
    impl From<wit::types::Upload> for Upload {
        fn from(upload: wit::types::Upload) -> Self {
            Upload {
                field: upload.field,
                name: upload.name,
                content_type: upload.content_type,
                size: upload.size,
                token: upload.token,
            }
        }
    }
    #[cfg(target_arch = "wasm32")]
    impl From<wit::storage::FileInfo> for FileInfo {
        fn from(info: wit::storage::FileInfo) -> Self {
            FileInfo {
                path: info.path,
                size: info.size,
            }
        }
    }

    pub fn context() -> Result<Context> {
        host!("context", Value::Null, |c: Context| Ok(c), {
            let c = wit::request::get_context();
            Ok(Context {
                app_id: c.app_id,
                origin: c.origin,
                issuer: c.issuer,
                dashboard_url: c.dashboard_url,
                user: c.user.map(Into::into),
                csrf: c.csrf,
                config: serde_json::from_str(&c.config)?,
                panel_action: c.panel_action.map(|a| PanelAction {
                    action: a.action,
                    value: a.value,
                    fields: a.fields.into_iter().collect(),
                    files: a.files.into_iter().map(Into::into).collect(),
                }),
                lan_bind: c.lan_bind,
                lan_ips: c.lan_ips,
            })
        })
    }
    pub fn form() -> Result<Form> {
        host!("form", Value::Null, |f: Form| Ok(f), {
            let f = wit::request::get_form()?;
            Ok(Form {
                fields: f.fields.into_iter().collect(),
                files: f.files.into_iter().map(Into::into).collect(),
            })
        })
    }
    pub fn log(message: &str) -> Result<()> {
        host!("log", json!({"message":message}), |r: ()| Ok(r), {
            wit::request::log(message);
            Ok(())
        })
    }
    pub fn transaction(statements: &[Statement]) -> Result<Vec<QueryResult>> {
        host!(
            "database",
            json!({"statements":statements}),
            |r: Vec<QueryResult>| Ok(r),
            {
                use wit::database::Value as Sql;
                let mut typed = Vec::with_capacity(statements.len());
                for statement in statements {
                    let params = statement
                        .params
                        .as_array()
                        .map(Vec::as_slice)
                        .unwrap_or(&[]);
                    let params = params
                        .iter()
                        .map(|value| match value {
                            Value::Null => Ok(Sql::Null),
                            Value::Bool(b) => Ok(Sql::Integer(i64::from(*b))),
                            Value::Number(n) => Ok(match n.as_i64() {
                                Some(i) => Sql::Integer(i),
                                None => Sql::Real(n.as_f64().unwrap_or(f64::NAN)),
                            }),
                            Value::String(s) => Ok(Sql::Text(s.clone())),
                            _ => Err(Error::bad("SQL parameters must be scalar values.")),
                        })
                        .collect::<Result<Vec<_>>>()?;
                    typed.push(wit::database::Statement {
                        sql: statement.sql.clone(),
                        params,
                    });
                }
                Ok(wit::database::transaction(&typed)?
                    .into_iter()
                    .map(|result| QueryResult {
                        rows: result
                            .rows
                            .into_iter()
                            .map(|row| {
                                let object = result.columns.iter().cloned().zip(
                                    row.into_iter().map(|value| match value {
                                        Sql::Null => Value::Null,
                                        Sql::Integer(i) => serde_json::json!(i),
                                        Sql::Real(r) => serde_json::json!(r),
                                        Sql::Text(s) => Value::String(s),
                                    }),
                                );
                                Value::Object(object.collect())
                            })
                            .collect(),
                        affected: result.affected,
                    })
                    .collect())
            }
        )
    }
    pub fn retain(token: &str, path: String) -> Result<StoredFile> {
        host!(
            "retain-upload",
            json!({"token":token,"path":path}),
            |f: StoredFile| Ok(f),
            {
                wit::uploads::retain(token, &path)?;
                Ok(StoredFile { path })
            }
        )
    }
    pub fn delete_upload(path: &str) -> Result<()> {
        host!("delete-file", json!({"path":path}), |r: ()| Ok(r), {
            Ok(wit::uploads::delete(path)?)
        })
    }
    #[cfg(target_arch = "wasm32")]
    fn serve(
        source: wit::types::FileSource,
        path: &str,
        filename: Option<&str>,
        download: bool,
    ) -> Result<()> {
        let options = wit::responses::ServeOptions {
            filename: filename.map(Into::into),
            download,
        };
        Ok(wit::responses::serve_file(source, path, &options)?)
    }
    pub fn serve_asset(path: &str) -> Result<()> {
        host!("asset", json!({"path":path}), |r: ()| Ok(r), {
            serve(wit::types::FileSource::Asset, path, None, false)
        })
    }
    pub fn serve_upload(path: &str, filename: &str, download: bool) -> Result<()> {
        host!(
            "file",
            json!({"path":path,"filename":filename,"download":download}),
            |r: ()| Ok(r),
            serve(
                wit::types::FileSource::Upload,
                path,
                Some(filename),
                download
            )
        )
    }
    pub fn serve_storage(path: &str, filename: &str, download: bool) -> Result<()> {
        host!(
            "storage-file-serve",
            json!({"path":path,"filename":filename,"download":download}),
            |r: ()| Ok(r),
            serve(
                wit::types::FileSource::Storage,
                path,
                Some(filename),
                download
            )
        )
    }
    pub fn set_json(key: &str, value: &Value) -> Result<()> {
        host!(
            "storage-set",
            json!({"key":key,"value":value}),
            |r: ()| Ok(r),
            Ok(wit::storage::set(key, &value.to_string())?)
        )
    }
    pub fn get_json(key: &str) -> Result<Option<Value>> {
        #[cfg(any(test, feature = "testing"))]
        #[derive(Deserialize)]
        struct Found {
            found: bool,
            #[serde(default)]
            value: Value,
        }
        host!(
            "storage-get",
            json!({"key":key}),
            |f: Found| Ok(f.found.then_some(f.value)),
            wit::storage::get(key)?
                .map(|text| serde_json::from_str(&text).map_err(Into::into))
                .transpose()
        )
    }
    pub fn delete_json(key: &str) -> Result<bool> {
        host!("storage-delete", json!({"key":key}), |r: bool| Ok(r), {
            Ok(wit::storage::delete(key)?)
        })
    }
    pub fn write_file(path: &str, bytes: &[u8]) -> Result<FileInfo> {
        #[cfg(any(test, feature = "testing"))]
        use base64::{Engine, engine::general_purpose::STANDARD};
        host!(
            "storage-file-write",
            json!({"path":path,"data":STANDARD.encode(bytes)}),
            |f: FileInfo| Ok(f),
            Ok(wit::storage::write_file(path, bytes)?.into())
        )
    }
    pub fn read_file(path: &str) -> Result<Vec<u8>> {
        #[cfg(any(test, feature = "testing"))]
        use base64::{Engine, engine::general_purpose::STANDARD};
        #[cfg(any(test, feature = "testing"))]
        #[derive(Deserialize)]
        struct Bytes {
            data: String,
        }
        host!(
            "storage-file-read",
            json!({"path":path}),
            |b: Bytes| STANDARD
                .decode(b.data)
                .map_err(|_| Error::internal("Invalid file bytes from host.")),
            Ok(wit::storage::read_file(path)?)
        )
    }
    pub fn file_info(path: &str) -> Result<FileInfo> {
        host!(
            "storage-file-info",
            json!({"path":path}),
            |f: FileInfo| Ok(f),
            { Ok(wit::storage::get_file_info(path)?.into()) }
        )
    }
    pub fn delete_file(path: &str) -> Result<bool> {
        host!(
            "storage-file-delete",
            json!({"path":path}),
            |r: bool| Ok(r),
            { Ok(wit::storage::delete_file(path)?) }
        )
    }
    pub fn list_files(prefix: &str, after: Option<&str>) -> Result<FilePage> {
        host!(
            "storage-list",
            json!({"prefix":prefix,"after":after}),
            |p: FilePage| Ok(p),
            {
                let page = wit::storage::list_files(prefix, after)?;
                Ok(FilePage {
                    files: page.files.into_iter().map(Into::into).collect(),
                    next: page.next,
                })
            }
        )
    }
    pub fn usage() -> Result<StorageUsage> {
        host!("storage-usage", Value::Null, |u: StorageUsage| Ok(u), {
            let u = wit::storage::get_usage()?;
            Ok(StorageUsage {
                bytes: u.bytes,
                entries: u.entries as usize,
                limit_bytes: u.limit_bytes,
                max_entries: u.max_entries as usize,
                max_file_bytes: u.max_file_bytes,
                max_value_bytes: u.max_value_bytes,
            })
        })
    }
    pub fn append_file(path: &str, bytes: &[u8]) -> Result<FileInfo> {
        #[cfg(any(test, feature = "testing"))]
        use base64::{Engine, engine::general_purpose::STANDARD};
        host!(
            "storage-file-append",
            json!({"path":path,"data":STANDARD.encode(bytes)}),
            |f: FileInfo| Ok(f),
            Ok(wit::storage::append_file(path, bytes)?.into())
        )
    }
    pub fn rename_file(from: &str, to: &str, replace: bool) -> Result<()> {
        host!(
            "storage-file-rename",
            json!({"from":from,"to":to,"replace":replace}),
            |r: Value| Ok(()),
            { Ok(wit::storage::rename_file(from, to, replace)?) }
        )
    }
    #[cfg(feature = "realtime")]
    pub fn call_realtime(room: &str, data: &[u8]) -> Result<Vec<u8>> {
        #[cfg(any(test, feature = "testing"))]
        use base64::{Engine, engine::general_purpose::STANDARD};
        #[cfg(any(test, feature = "testing"))]
        #[derive(Deserialize)]
        struct Bytes {
            data: String,
        }
        host!(
            "realtime-call",
            json!({"room":room,"data":STANDARD.encode(data)}),
            |b: Bytes| STANDARD
                .decode(b.data)
                .map_err(|_| Error::internal("Invalid answer bytes from host.")),
            { Ok(wit::realtime_control::call(room, data)?) }
        )
    }
    #[cfg(feature = "realtime")]
    pub fn list_rooms() -> Result<Vec<crate::realtime::RoomInfo>> {
        host!(
            "realtime-rooms",
            Value::Null,
            |rooms: Vec<crate::realtime::RoomInfo>| Ok(rooms),
            {
                Ok(wit::realtime_control::list_rooms()?
                    .into_iter()
                    .map(|r| crate::realtime::RoomInfo {
                        id: r.id,
                        connections: r.connections,
                    })
                    .collect())
            }
        )
    }
    #[cfg(feature = "realtime")]
    pub fn restart_realtime() -> Result<()> {
        host!("realtime-restart", Value::Null, |r: Value| Ok(()), {
            Ok(wit::realtime_control::restart()?)
        })
    }
    #[cfg(feature = "realtime")]
    #[cfg(target_arch = "wasm32")]
    pub fn file_size(source: wit::types::FileSource, path: &str) -> Result<u64> {
        Ok(wit::files::size(source, path)?)
    }
    #[cfg(feature = "realtime")]
    #[cfg(target_arch = "wasm32")]
    pub fn read_range(
        source: wit::types::FileSource,
        path: &str,
        offset: u64,
        length: u32,
    ) -> Result<Vec<u8>> {
        Ok(wit::files::read(source, path, offset, length)?)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub username: String,
    pub name: String,
    pub email: String,
    /// Profile text from the LOKI account. Empty when the host does not send it.
    #[serde(default)]
    pub bio: String,
    /// Whether this account has a profile picture at `{dashboard}/avatars/{id}`.
    #[serde(default)]
    pub avatar: bool,
    pub roles: Vec<String>,
}
impl User {
    pub fn has_role(&self, role: &str) -> bool {
        self.roles.iter().any(|r| r == role)
    }
    pub fn require_role(&self, role: &str) -> Result<()> {
        if self.has_role(role) {
            Ok(())
        } else {
            Err(Error::forbidden())
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Context {
    pub app_id: String,
    pub origin: String,
    pub issuer: String,
    #[serde(default)]
    pub dashboard_url: String,
    #[serde(default)]
    pub user: Option<User>,
    #[serde(default)]
    pub csrf: String,
    #[serde(default)]
    pub config: Value,
    /// Set when LOKI's dashboard sends an action from the app's panel.
    #[serde(default)]
    pub panel_action: Option<PanelAction>,
    /// The owner's LAN address for a realtime app, such as `0.0.0.0:4767`, if any.
    #[serde(default)]
    pub lan_bind: String,
    /// This server's own IPv4 addresses on its networks, for telling people where to connect.
    #[serde(default)]
    pub lan_ips: Vec<String>,
}
/// One action from the app's panel: a button's `action` and `value`, a form's fields,
/// or an uploaded file to `retain`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PanelAction {
    pub action: String,
    #[serde(default)]
    pub value: String,
    #[serde(default)]
    pub fields: BTreeMap<String, String>,
    #[serde(default)]
    pub files: Vec<Upload>,
}
impl Context {
    pub fn current() -> Result<Self> {
        host::context()
    }
    pub fn require_user(&self) -> Result<&User> {
        self.user.as_ref().ok_or_else(Error::unauthorized)
    }
    pub fn require_role(&self, role: &str) -> Result<&User> {
        let user = self.require_user()?;
        user.require_role(role)?;
        Ok(user)
    }
    pub fn config<T: DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_value(self.config.clone()).map_err(Into::into)
    }
    pub fn csrf_token(&self) -> &str {
        &self.csrf
    }
    pub fn form(&self) -> Result<Form> {
        host::form()
    }
    pub fn database(&self) -> Database {
        Database
    }
    pub fn storage(&self) -> crate::Storage {
        crate::Storage
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Database;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Statement {
    pub sql: String,
    pub params: Value,
}
impl Statement {
    pub fn new(sql: impl Into<String>, params: impl Serialize) -> Result<Self> {
        let params = serde_json::to_value(params)?;
        if !params.is_array() {
            return Err(Error::bad(
                "SQL parameters must be an array. Use params![...].",
            ));
        }
        Ok(Self {
            sql: sql.into(),
            params,
        })
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueryResult {
    pub rows: Vec<Value>,
    pub affected: u64,
}
impl QueryResult {
    pub fn decode<T: DeserializeOwned>(self) -> Result<Vec<T>> {
        self.rows
            .into_iter()
            .map(|v| serde_json::from_value(v).map_err(Into::into))
            .collect()
    }
}
impl Database {
    pub fn transaction(&self, statements: &[Statement]) -> Result<Vec<QueryResult>> {
        host::transaction(statements)
    }
    pub fn query(&self, sql: &str, params: impl Serialize) -> Result<QueryResult> {
        let mut results = self.transaction(&[Statement::new(sql, params)?])?;
        if results.len() != 1 {
            return Err(Error::internal("Host returned an invalid SQL response."));
        }
        Ok(results.remove(0))
    }
    pub fn execute(&self, sql: &str, params: impl Serialize) -> Result<u64> {
        Ok(self.query(sql, params)?.affected)
    }
    pub fn fetch_all<T: DeserializeOwned>(
        &self,
        sql: &str,
        params: impl Serialize,
    ) -> Result<Vec<T>> {
        self.query(sql, params)?.decode()
    }
    pub fn fetch_optional<T: DeserializeOwned>(
        &self,
        sql: &str,
        params: impl Serialize,
    ) -> Result<Option<T>> {
        let mut rows = self.fetch_all(sql, params)?;
        if rows.len() > 1 {
            return Err(Error::internal("Expected at most one database row."));
        }
        Ok(rows.pop())
    }
    pub fn fetch_one<T: DeserializeOwned>(&self, sql: &str, params: impl Serialize) -> Result<T> {
        self.fetch_optional(sql, params)?
            .ok_or_else(|| Error::not_found("Record not found."))
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Form {
    pub fields: BTreeMap<String, String>,
    pub files: Vec<Upload>,
}
impl Form {
    pub fn field(&self, name: &str) -> Option<&str> {
        self.fields.get(name).map(String::as_str)
    }
    pub fn required(&self, name: &str) -> Result<&str> {
        self.field(name)
            .ok_or_else(|| Error::bad(format!("Missing form field: {name}")))
    }
    pub fn file(&self, name: &str) -> Option<&Upload> {
        self.files.iter().find(|u| u.field == name)
    }
    pub fn required_file(&self, name: &str) -> Result<&Upload> {
        self.file(name)
            .ok_or_else(|| Error::bad(format!("Missing upload: {name}")))
    }
    pub fn decode<T: DeserializeOwned>(&self) -> Result<T> {
        let encoded = form_urlencoded::Serializer::new(String::new())
            .extend_pairs(&self.fields)
            .finish();
        serde_urlencoded::from_str(&encoded).map_err(|error| Error::bad(error.to_string()))
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Upload {
    pub field: String,
    pub name: String,
    pub content_type: String,
    pub size: u64,
    token: String,
}
impl Upload {
    pub fn retain(&self, path: impl Into<String>) -> Result<StoredFile> {
        host::retain(&self.token, path.into())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredFile {
    pub path: String,
}
impl StoredFile {
    pub fn at(path: impl Into<String>) -> Self {
        Self { path: path.into() }
    }
    pub fn delete(&self) -> Result<()> {
        host::delete_upload(&self.path)
    }
    pub fn download(&self, filename: &str) -> Result<Response> {
        self.respond(filename, true)
    }
    pub fn inline(&self, filename: &str) -> Result<Response> {
        self.respond(filename, false)
    }
    fn respond(&self, filename: &str, download: bool) -> Result<Response> {
        host::serve_upload(&self.path, filename, download)?;
        Ok(Response::empty(200))
    }
}
pub fn asset(path: &str) -> Result<Response> {
    host::serve_asset(path)?;
    Ok(Response::empty(200))
}
pub fn log(message: impl AsRef<str>) -> Result<()> {
    host::log(message.as_ref())
}
pub fn now() -> std::time::SystemTime {
    std::time::SystemTime::now()
}
/// Generate a UUID-shaped identifier from the host OS/WASI cryptographic random source.
pub fn random_id() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("Cryptographic randomness unavailable");
    bytes[6] = (bytes[6] & 15) | 64;
    bytes[8] = (bytes[8] & 63) | 128;
    let h: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Method {
    Get,
    Head,
    Post,
    Put,
    Patch,
    Delete,
    Options,
    Other(String),
}
impl From<wasi::http::types::Method> for Method {
    fn from(value: wasi::http::types::Method) -> Self {
        use wasi::http::types::Method as M;
        match value {
            M::Get => Self::Get,
            M::Head => Self::Head,
            M::Post => Self::Post,
            M::Put => Self::Put,
            M::Patch => Self::Patch,
            M::Delete => Self::Delete,
            M::Options => Self::Options,
            M::Other(method) => Self::Other(method),
            M::Connect => Self::Other("CONNECT".into()),
            M::Trace => Self::Other("TRACE".into()),
        }
    }
}
pub struct Request {
    method: Method,
    path: String,
    query: String,
    headers: BTreeMap<String, Vec<Vec<u8>>>,
    context: Context,
    params: BTreeMap<String, String>,
    incoming: Option<IncomingRequest>,
}
impl Request {
    fn incoming(incoming: IncomingRequest) -> Result<Self> {
        let uri = incoming.path_with_query().unwrap_or_else(|| "/".into());
        let (path, query) = uri.split_once('?').unwrap_or((&uri, ""));
        let mut headers: BTreeMap<String, Vec<Vec<u8>>> = BTreeMap::new();
        for (name, value) in incoming.headers().entries() {
            headers.entry(name).or_default().push(value);
        }
        Ok(Self {
            method: incoming.method().into(),
            path: path.into(),
            query: query.into(),
            headers,
            context: Context::current()?,
            params: BTreeMap::new(),
            incoming: Some(incoming),
        })
    }
    pub fn method(&self) -> &Method {
        &self.method
    }
    pub fn path(&self) -> &str {
        &self.path
    }
    pub fn context(&self) -> &Context {
        &self.context
    }
    pub fn param(&self, name: &str) -> Result<&str> {
        self.params
            .get(name)
            .map(String::as_str)
            .ok_or_else(|| Error::bad(format!("Missing route parameter: {name}")))
    }
    pub fn query(&self, name: &str) -> Option<String> {
        form_urlencoded::parse(self.query.as_bytes())
            .find(|(key, _)| key == name)
            .map(|(_, v)| v.into_owned())
    }
    pub fn header(&self, name: &str) -> Option<&[u8]> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .and_then(|v| v.first())
            .map(Vec::as_slice)
    }
    pub fn form(&self) -> Result<Form> {
        self.context.form()
    }
    /// Read a small raw body. Prefer host forms/uploads for large or multipart bodies.
    pub fn body(&mut self, max_bytes: usize) -> Result<Vec<u8>> {
        let incoming = self
            .incoming
            .as_ref()
            .ok_or_else(|| Error::bad("No incoming body in this request."))?;
        let body = incoming
            .consume()
            .map_err(|_| Error::bad("Request body was already consumed."))?;
        let stream = body
            .stream()
            .map_err(|_| Error::bad("Request body is unavailable."))?;
        let mut output = Vec::new();
        loop {
            match stream.blocking_read(4096) {
                Ok(chunk) => {
                    if output.len() + chunk.len() > max_bytes {
                        return Err(Error::new(413, "Request body exceeds its limit."));
                    }
                    output.extend_from_slice(&chunk);
                }
                Err(wasi::io::streams::StreamError::Closed) => break,
                Err(_) => return Err(Error::bad("Could not read request body.")),
            }
        }
        drop(stream);
        let _ = wasi::http::types::IncomingBody::finish(body);
        Ok(output)
    }
    pub fn json<T: DeserializeOwned>(&mut self, max_bytes: usize) -> Result<T> {
        serde_json::from_slice(&self.body(max_bytes)?).map_err(|e| Error::bad(e.to_string()))
    }
}

#[derive(Clone, Debug)]
pub struct Response {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}
impl Response {
    pub fn empty(status: u16) -> Self {
        Self {
            status,
            headers: BTreeMap::new(),
            body: Vec::new(),
        }
    }
    pub fn text(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            headers: BTreeMap::from([("content-type".into(), "text/plain; charset=utf-8".into())]),
            body: body.into().into_bytes(),
        }
    }
    pub fn html(body: impl Into<String>) -> Self {
        let mut result = Self::text(body);
        result
            .headers
            .insert("content-type".into(), "text/html; charset=utf-8".into());
        result
    }
    pub fn json(value: &impl Serialize) -> Result<Self> {
        let mut result = Self::text(serde_json::to_string(value)?);
        result
            .headers
            .insert("content-type".into(), "application/json".into());
        Ok(result)
    }
    pub fn redirect(location: &str) -> Result<Self> {
        if location.contains(['\r', '\n']) {
            return Err(Error::bad("Invalid redirect URL."));
        }
        Self::empty(303).header("location", location)
    }
    pub fn header(mut self, name: &str, value: &str) -> Result<Self> {
        if name.is_empty()
            || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || value.contains(['\r', '\n', '\0'])
        {
            return Err(Error::bad("Invalid response header."));
        }
        self.headers.insert(name.to_ascii_lowercase(), value.into());
        Ok(self)
    }
    pub fn with_status(mut self, status: u16) -> Self {
        self.status = status;
        self
    }
    #[cfg(feature = "templates")]
    pub fn render(template: impl askama::Template) -> Result<Self> {
        Ok(Self::html(
            template
                .render()
                .map_err(|e| Error::internal(e.to_string()))?,
        ))
    }
}

type Handler = fn(Request) -> Result<Response>;
pub struct Router {
    routes: Vec<(Method, String, Handler)>,
}
impl Default for Router {
    fn default() -> Self {
        Self::new()
    }
}
impl Router {
    pub fn new() -> Self {
        Self { routes: Vec::new() }
    }
    pub fn route(mut self, method: Method, path: &str, handler: Handler) -> Self {
        self.routes.push((method, path.into(), handler));
        self
    }
    pub fn get(self, path: &str, handler: Handler) -> Self {
        self.route(Method::Get, path, handler)
    }
    pub fn post(self, path: &str, handler: Handler) -> Self {
        self.route(Method::Post, path, handler)
    }
    /// The app's panel in LOKI's dashboard: one handler for showing it and for its
    /// actions (see [`crate::panel`]).
    pub fn panel(self, handler: Handler) -> Self {
        self.get("/_loki/panel", handler)
            .post("/_loki/panel", handler)
    }
    /// A job LOKI runs on a timer, declared under `schedules` in the app's metadata
    /// with the same `name`. It runs without a user; answer 200 when it succeeded.
    pub fn schedule(self, name: &str, handler: Handler) -> Self {
        self.post(&format!("/_loki/schedule/{name}"), handler)
    }
    pub fn handle(&self, mut request: Request) -> Result<Response> {
        let mut path_matched = false;
        for (method, path, handler) in &self.routes {
            if let Some(params) = match_route(path, &request.path) {
                path_matched = true;
                if method == &request.method
                    || (*method == Method::Get && request.method == Method::Head)
                {
                    request.params = params;
                    return handler(request);
                }
            }
        }
        Ok(Response::text(if path_matched {
            "Method not allowed."
        } else {
            "Not found."
        })
        .with_status(if path_matched { 405 } else { 404 }))
    }
}
fn match_route(pattern: &str, path: &str) -> Option<BTreeMap<String, String>> {
    let expected: Vec<_> = pattern.split('/').collect();
    let actual: Vec<_> = path.split('/').collect();
    if expected.len() != actual.len() {
        return None;
    }
    let mut params = BTreeMap::new();
    for (pattern, value) in expected.into_iter().zip(actual) {
        if let Some(name) = pattern.strip_prefix('{').and_then(|p| p.strip_suffix('}')) {
            let decoded = percent_encoding::percent_decode_str(value)
                .decode_utf8()
                .ok()?
                .into_owned();
            if decoded.is_empty() || decoded.contains(['/', '\\', '\0']) {
                return None;
            }
            params.insert(name.into(), decoded);
        } else if pattern != value {
            return None;
        }
    }
    Some(params)
}

#[doc(hidden)]
pub fn serve(incoming: IncomingRequest, out: ResponseOutparam, handler: Handler) {
    let response = Request::incoming(incoming)
        .and_then(handler)
        .unwrap_or_else(|error| {
            let _ = log(error.message());
            Response::text(if error.status() >= 500 {
                "The application could not complete this request."
            } else {
                error.message()
            })
            .with_status(error.status())
        });
    let headers: Vec<_> = response
        .headers
        .iter()
        .map(|(k, v)| (k.clone(), v.as_bytes().to_vec()))
        .collect();
    let fields = match Fields::from_list(&headers) {
        Ok(fields) => fields,
        Err(_) => {
            ResponseOutparam::set(
                out,
                Err(wasi::http::types::ErrorCode::InternalError(Some(
                    "Invalid response headers".into(),
                ))),
            );
            return;
        }
    };
    let outgoing = OutgoingResponse::new(fields);
    if outgoing.set_status_code(response.status).is_err() {
        ResponseOutparam::set(
            out,
            Err(wasi::http::types::ErrorCode::InternalError(Some(
                "Invalid response status".into(),
            ))),
        );
        return;
    }
    let Ok(body) = outgoing.body() else {
        return;
    };
    ResponseOutparam::set(out, Ok(outgoing));
    if !response.body.is_empty() {
        let Ok(stream) = body.write() else {
            return;
        };
        for chunk in response.body.chunks(4096) {
            if stream.blocking_write_and_flush(chunk).is_err() {
                return;
            }
        }
        drop(stream);
    }
    let _ = OutgoingBody::finish(body, None);
}

#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use super::*;
    use std::cell::RefCell;
    type Host = Box<dyn FnMut(&str, &str) -> std::result::Result<String, String>>;
    thread_local! {static HOST:RefCell<Option<Host>>=RefCell::new(None);}
    pub(crate) fn invoke(
        operation: &str,
        input: &str,
    ) -> Option<std::result::Result<String, String>> {
        HOST.with(|host| host.borrow_mut().as_mut().map(|h| h(operation, input)))
    }
    /// Asks the installed fake host, if any, using the operation names and JSON of
    /// the host API before 0.3.
    pub(crate) fn fake<T: DeserializeOwned>(operation: &str, input: Value) -> Option<Result<T>> {
        invoke(operation, &input.to_string()).map(|result| {
            let output = result.map_err(Error::from_host)?;
            serde_json::from_str(&output).map_err(Into::into)
        })
    }
    /// Install a thread-local fake host for one test, restoring it even after panic.
    pub fn with_host<T>(
        host: impl FnMut(&str, &str) -> std::result::Result<String, String> + 'static,
        test: impl FnOnce() -> T,
    ) -> T {
        struct Restore(Option<Host>);
        impl Drop for Restore {
            fn drop(&mut self) {
                HOST.with(|h| *h.borrow_mut() = self.0.take());
            }
        }
        let _restore = Restore(HOST.with(|h| h.borrow_mut().replace(Box::new(host))));
        test()
    }
    pub fn request(method: Method, path: &str, context: Context) -> Request {
        let (path, query) = path.split_once('?').unwrap_or((path, ""));
        Request {
            method,
            path: path.into(),
            query: query.into(),
            context,
            headers: BTreeMap::new(),
            params: BTreeMap::new(),
            incoming: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn context() -> Context {
        serde_json::from_value(json!({"app_id":"demo","origin":"http://demo.localhost","issuer":"http://localhost","user":{"id":"stable-id","username":"a","name":"A","email":"","roles":["access"]},"config":{"limit":12}})).unwrap()
    }
    #[test]
    fn identity_and_configuration_are_typed() {
        #[derive(Deserialize)]
        struct Config {
            limit: u32,
        }
        let ctx = context();
        let user = ctx.require_user().unwrap();
        assert_eq!(user.id, "stable-id");
        assert_eq!(user.bio, "");
        assert!(!user.avatar);
        assert_eq!(ctx.config::<Config>().unwrap().limit, 12);
        assert_eq!(ctx.require_role("admin").unwrap_err().status(), 403);
        let mut anonymous = ctx;
        anonymous.user = None;
        assert_eq!(anonymous.require_user().unwrap_err().status(), 401);
    }
    #[test]
    fn database_binds_values_and_decodes_rows() {
        #[derive(Deserialize)]
        struct Row {
            name: String,
        }
        testing::with_host(
            |op, input| {
                assert_eq!(op, "database");
                let input: Value = serde_json::from_str(input).unwrap();
                assert_eq!(
                    input["statements"][0]["params"],
                    json!(["' OR 1=1", 42, null])
                );
                Ok(json!([{"rows":[{"name":"Ada"}],"affected":0}]).to_string())
            },
            || {
                let row: Row = Database
                    .fetch_one(
                        "SELECT name FROM people WHERE name=? AND id=? AND deleted IS ?",
                        crate::params!["' OR 1=1", 42, None::<i32>],
                    )
                    .unwrap();
                assert_eq!(row.name, "Ada");
            },
        );
        assert!(Statement::new("SELECT ?", "unsafe shape").is_err());
        testing::with_host(
            |_, _| Ok("[]".into()),
            || assert!(Database.query("SELECT 1", crate::params![]).is_err()),
        );
    }
    #[test]
    fn forms_decode_numbers_and_stream_uploads_through_the_host() {
        #[derive(Deserialize)]
        struct Fields {
            title: String,
            count: u32,
        }
        let form: Form = serde_json::from_value(json!({"fields":{"title":"枝 & green","count":"12"},"files":[{"field":"image","name":"picture.png","content_type":"image/png","size":123,"token":"private-token"}]})).unwrap();
        let fields: Fields = form.decode().unwrap();
        assert_eq!(fields.title, "枝 & green");
        assert_eq!(fields.count, 12);
        assert!(form.required("missing").is_err());
        testing::with_host(
            |op, input| {
                let input: Value = serde_json::from_str(input).unwrap();
                match op {
                    "retain-upload" => {
                        assert_eq!(input["token"], "private-token");
                        Ok(json!({"path":input["path"]}).to_string())
                    }
                    "file" => {
                        assert_eq!(
                            input,
                            json!({"path":"images/new.png","filename":"picture.png","download":true})
                        );
                        Ok("null".into())
                    }
                    _ => panic!("Unexpected host operation: {op}"),
                }
            },
            || {
                let file = form
                    .required_file("image")
                    .unwrap()
                    .retain("images/new.png")
                    .unwrap();
                let response = file.download("picture.png").unwrap();
                assert!(response.body.is_empty());
            },
        );
    }
    #[test]
    fn routing_decodes_unicode_and_rejects_encoded_separators() {
        fn item(request: Request) -> Result<Response> {
            Ok(Response::text(format!(
                "{}:{}",
                request.param("id")?,
                request.query("q").unwrap_or_default()
            )))
        }
        let router = Router::new().get("/items/{id}", item);
        let response = router
            .handle(testing::request(
                Method::Get,
                "/items/%E6%9E%9D?q=a%26b",
                context(),
            ))
            .unwrap();
        assert_eq!(response.body, "枝:a&b".as_bytes());
        for path in ["/items/a%2fb", "/items/a%5cb", "/items/%00", "/missing"] {
            assert_eq!(
                router
                    .handle(testing::request(Method::Get, path, context()))
                    .unwrap()
                    .status,
                404
            );
        }
        assert_eq!(
            router
                .handle(testing::request(Method::Post, "/items/a", context()))
                .unwrap()
                .status,
            405
        );
        assert_eq!(
            router
                .handle(testing::request(Method::Head, "/items/a", context()))
                .unwrap()
                .status,
            200
        );
    }
    #[test]
    fn response_headers_cannot_inject_lines() {
        assert!(Response::redirect("/safe\r\nset-cookie: evil").is_err());
        assert!(Response::text("ok").header("x-test", "a\0b").is_err());
        assert!(Response::text("ok").header("bad name", "value").is_err());
        let response = Response::json(&json!({"name":"\"<枝>"})).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&response.body).unwrap()["name"],
            "\"<枝>"
        );
    }
    #[test]
    fn test_hosts_are_scoped_and_restored_after_panics() {
        testing::with_host(
            |_, _| Ok("null".into()),
            || {
                let panic = std::panic::catch_unwind(|| {
                    testing::with_host(
                        |_, _| Err("inner".into()),
                        || {
                            assert!(log("test").is_err());
                            panic!("restore");
                        },
                    )
                });
                assert!(panic.is_err());
                assert!(log("outer").is_ok());
            },
        );
        assert!(log("no host").is_err());
    }
    #[test]
    fn generated_ids_have_random_uuid_v4_bits() {
        let a = random_id();
        let b = random_id();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        assert_eq!(&a[14..15], "4");
        assert!(matches!(&a[19..20], "8" | "9" | "a" | "b"));
    }
}
