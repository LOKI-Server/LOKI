//! Persistent app-scoped values and generated files, managed by the LOKI host.
use crate::{Error, Response, Result, guest::call};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

/// Requires the `storage` manifest capability. State is shared by this app's
/// users; include a stable user ID in your keys for per-user preferences.
#[derive(Clone, Copy, Debug, Default)]
pub struct Storage;
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FileInfo {
    pub path: String,
    pub size: u64,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FilePage {
    pub files: Vec<FileInfo>,
    pub next: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StorageUsage {
    pub bytes: u64,
    pub entries: usize,
    pub limit_bytes: u64,
    pub max_entries: usize,
    pub max_file_bytes: u64,
    pub max_value_bytes: u64,
}
impl Storage {
    /// Atomically replace a JSON value (at most 128 KiB encoded).
    pub fn set_json(&self, key: &str, value: &impl Serialize) -> Result<()> {
        let value = serde_json::to_value(value)?;
        if serde_json::to_vec(&value)?.len() > 128 * 1024 {
            return Err(Error::new(
                413,
                "Encoded JSON values are limited to 128 KiB.",
            ));
        }
        call("storage-set", json!({"key":key,"value":value}))
    }
    /// Missing keys return `None`; malformed data remains an error.
    pub fn get_json<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        #[derive(Deserialize)]
        struct Found {
            found: bool,
            #[serde(default)]
            value: Value,
        }
        let result: Found = call("storage-get", json!({"key":key}))?;
        if result.found {
            Ok(Some(serde_json::from_value(result.value)?))
        } else {
            Ok(None)
        }
    }
    pub fn delete_json(&self, key: &str) -> Result<bool> {
        call("storage-delete", json!({"key":key}))
    }
    /// Atomically replace a generated file. Writes and buffered reads are
    /// bounded to 1 MiB. Use host uploads for large incoming media.
    pub fn write(&self, path: &str, bytes: impl AsRef<[u8]>) -> Result<DataFile> {
        let bytes = bytes.as_ref();
        if bytes.len() > 1024 * 1024 {
            return Err(Error::new(
                413,
                "Generated files are limited to 1 MiB per write.",
            ));
        }
        let _: FileInfo = call(
            "storage-file-write",
            json!({"path":path,"data":STANDARD.encode(bytes)}),
        )?;
        Ok(self.file(path))
    }
    pub fn file(&self, path: impl Into<String>) -> DataFile {
        DataFile { path: path.into() }
    }
    /// A sorted flat listing of logical paths. Pass `next` back as `after`.
    /// Pages contain at most 100 files and are not a snapshot across mutations.
    pub fn list_files(&self, prefix: &str, after: Option<&str>) -> Result<FilePage> {
        call("storage-list", json!({"prefix":prefix,"after":after}))
    }
    pub fn usage(&self) -> Result<StorageUsage> {
        call("storage-usage", ())
    }
}
/// A logical generated-file path, not a path on the host computer.
#[derive(Clone, Debug)]
pub struct DataFile {
    path: String,
}
impl DataFile {
    pub fn path(&self) -> &str {
        &self.path
    }
    pub fn read(&self) -> Result<Vec<u8>> {
        #[derive(Deserialize)]
        struct Bytes {
            data: String,
        }
        let result: Bytes = call("storage-file-read", json!({"path":self.path}))?;
        if result.data.len() > 1024usize.pow(2).div_ceil(3) * 4 {
            return Err(Error::new(413, "Generated file exceeds its read limit."));
        }
        STANDARD
            .decode(result.data)
            .map_err(|_| Error::internal("Invalid file bytes from host."))
    }
    pub fn read_text(&self) -> Result<String> {
        String::from_utf8(self.read()?).map_err(|_| Error::bad("File is not UTF-8 text."))
    }
    pub fn info(&self) -> Result<FileInfo> {
        call("storage-file-info", json!({"path":self.path}))
    }
    pub fn delete(&self) -> Result<bool> {
        call("storage-file-delete", json!({"path":self.path}))
    }
    pub fn download(&self, filename: &str) -> Result<Response> {
        self.respond(filename, true)
    }
    pub fn inline(&self, filename: &str) -> Result<Response> {
        self.respond(filename, false)
    }
    fn respond(&self, filename: &str, download: bool) -> Result<Response> {
        call::<()>(
            "storage-file-serve",
            json!({"path":self.path,"filename":filename,"download":download}),
        )?;
        Ok(Response::empty(200))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ErrorKind, testing};
    #[test]
    fn typed_values_distinguish_missing_null_and_invalid_data() {
        testing::with_host(
            |operation, input| {
                let input: Value = serde_json::from_str(input).unwrap();
                assert_eq!(operation, "storage-get");
                Ok(match input["key"].as_str().unwrap() {
                    "missing" => json!({"found":false}),
                    "null" => json!({"found":true,"value":null}),
                    _ => json!({"found":true,"value":{"theme":"forest"}}),
                }
                .to_string())
            },
            || {
                #[derive(Deserialize)]
                struct Preferences {
                    theme: String,
                }
                assert!(
                    Storage
                        .get_json::<Preferences>("missing")
                        .unwrap()
                        .is_none()
                );
                assert_eq!(
                    Storage.get_json::<Value>("null").unwrap(),
                    Some(Value::Null)
                );
                assert!(Storage.get_json::<u32>("null").is_err());
                assert_eq!(
                    Storage
                        .get_json::<Preferences>("prefs")
                        .unwrap()
                        .unwrap()
                        .theme,
                    "forest"
                );
            },
        );
    }
    #[test]
    fn generated_files_are_bounded_and_use_host_streaming() {
        testing::with_host(
            |operation, input| {
                let input: Value = serde_json::from_str(input).unwrap();
                match operation {
                    "storage-file-write" => {
                        assert_eq!(
                            STANDARD.decode(input["data"].as_str().unwrap()).unwrap(),
                            b"hello"
                        );
                        Ok(json!({"path":input["path"],"size":5}).to_string())
                    }
                    "storage-file-serve" => {
                        assert_eq!(input["path"], "reports/hello.txt");
                        assert_eq!(input["download"], true);
                        Ok("null".into())
                    }
                    _ => panic!("Unexpected operation"),
                }
            },
            || {
                let file = Storage.write("reports/hello.txt", b"hello").unwrap();
                assert!(file.download("hello.txt").unwrap().body.is_empty());
                assert_eq!(
                    Storage
                        .write("big", vec![0; 1024 * 1024 + 1])
                        .unwrap_err()
                        .kind(),
                    ErrorKind::LimitExceeded
                );
            },
        );
    }
    #[test]
    fn host_errors_have_stable_kinds_and_legacy_errors_still_work() {
        for (status, kind) in [
            (403, ErrorKind::PermissionDenied),
            (404, ErrorKind::NotFound),
            (409, ErrorKind::Conflict),
            (413, ErrorKind::LimitExceeded),
        ] {
            testing::with_host(
                move |_, _| {
                    Err(
                        json!({"loki_error":1,"status":status,"message":"safe message"})
                            .to_string(),
                    )
                },
                || {
                    assert_eq!(Storage.file("test").read().unwrap_err().kind(), kind);
                },
            );
        }
        testing::with_host(
            |_, _| Err("older host".into()),
            || assert_eq!(Storage.usage().unwrap_err().kind(), ErrorKind::HostFailure),
        );
    }
}
