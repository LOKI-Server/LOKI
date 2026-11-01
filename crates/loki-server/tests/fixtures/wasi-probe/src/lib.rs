use loki_sdk::{Context, Database, Storage, StoredFile, params};
use serde_json::{Value, json};
use wasi::http::types::*;

fn denied<T>(result: loki_sdk::Result<T>) -> String {
    json!({"denied":result.is_err()}).to_string()
}
struct Probe;
impl wasi::exports::http::incoming_handler::Guest for Probe {
    fn handle(request: IncomingRequest, out: ResponseOutparam) {
        let path = request.path_with_query().unwrap_or_default();
        let storage = Storage;
        let body = match path.as_str() {
            "/health" => {
                Database
                    .execute("CREATE TABLE IF NOT EXISTS probe(value TEXT)", params![])
                    .unwrap();
                "ready".into()
            }
            "/trap" => panic!("deliberate guest trap"),
            "/loop" => loop {
                std::hint::black_box(1);
            },
            "/memory" => {
                let bytes = vec![1u8; 128 * 1024 * 1024];
                std::hint::black_box(bytes);
                "unexpected allocation".into()
            }
            "/fs" => json!({"denied":std::fs::read("/identity.pem").is_err()}).to_string(),
            "/network" => {
                let r = OutgoingRequest::new(Fields::new());
                r.set_scheme(Some(&Scheme::Http)).unwrap();
                r.set_authority(Some("127.0.0.1:80")).unwrap();
                r.set_path_with_query(Some("/")).unwrap();
                json!({"denied":wasi::http::outgoing_handler::handle(r,None).is_err()}).to_string()
            }
            "/sql-escape" => denied(Database.execute(
                "ATTACH DATABASE '/tmp/loki-escape.db' AS escaped",
                params![],
            )),
            "/missing-capability" => denied(StoredFile::at("identity.pem").download("x")),
            "/missing-storage" => denied(storage.get_json::<Value>("preferences")),
            "/storage" => {
                storage
                    .set_json("preferences", &json!({"theme":"forest"}))
                    .unwrap();
                storage
                    .write("reports/branch.txt", "Hello, branch.")
                    .unwrap();
                let traversal = storage.write("../another-app/secret", "x").unwrap_err();
                let preferences = storage.get_json::<Value>("preferences").unwrap();
                json!({"preferences":{"value":preferences},"traversal":{"status":traversal.status()}})
                    .to_string()
            }
            "/storage-read" => {
                json!({"value":storage.get_json::<Value>("preferences").unwrap()}).to_string()
            }
            "/storage-download" | "/storage-abort" => {
                storage
                    .file("reports/branch.txt")
                    .download("branch.txt")
                    .unwrap();
                if path == "/storage-abort" {
                    "denied".into()
                } else {
                    String::new()
                }
            }
            "/storage-active" => {
                storage.write("active.html", "<h1>hi</h1>").unwrap();
                storage.file("active.html").inline("active.html").unwrap();
                String::new()
            }
            "/cookies" => json!({"cookies":request.headers().get("cookie").len(),"authorization":request.headers().get("authorization").len()}).to_string(),
            _ => serde_json::to_string(&Context::current().unwrap()).unwrap(),
        };
        let headers = Fields::from_list(&[
            ("content-type".into(), b"application/json".to_vec()),
            (
                "set-cookie".into(),
                b"loki_session=forged; Domain=localhost; Path=/".to_vec(),
            ),
        ])
        .unwrap();
        let response = OutgoingResponse::new(headers);
        if path == "/storage-abort" {
            response.set_status_code(403).unwrap();
        }
        let body_stream = response.body().unwrap();
        ResponseOutparam::set(out, Ok(response));
        let stream = body_stream.write().unwrap();
        for chunk in body.as_bytes().chunks(4096) {
            stream.blocking_write_and_flush(chunk).unwrap();
        }
        drop(stream);
        OutgoingBody::finish(body_stream, None).unwrap();
    }
}
wasi::http::proxy::export!(Probe);
