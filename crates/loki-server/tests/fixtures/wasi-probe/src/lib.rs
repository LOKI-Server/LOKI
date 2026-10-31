use wasi::http::types::*;
mod bindings {
    wit_bindgen::generate!({path:"wit",world:"host-api"});
}
fn call(op: &str, value: serde_json::Value) -> Result<String, String> {
    bindings::loki::app::platform::call(op, &value.to_string())
}
struct Probe;
impl wasi::exports::http::incoming_handler::Guest for Probe {
    fn handle(request: IncomingRequest, out: ResponseOutparam) {
        use serde_json::json;
        let path = request.path_with_query().unwrap_or_default();
        let body=match path.as_str(){
            "/health"=>{call("database",json!({"statements":[{"sql":"CREATE TABLE IF NOT EXISTS probe(value TEXT)","params":[]}]})).unwrap();"ready".into()},
            "/trap"=>panic!("deliberate guest trap"),
            "/loop"=>loop {std::hint::black_box(1);},
            "/memory"=>{let bytes=vec![1u8;128*1024*1024];std::hint::black_box(bytes);"unexpected allocation".into()},
            "/fs"=>json!({"denied":std::fs::read("/identity.pem").is_err()}).to_string(),
            "/network"=>{
                let r=OutgoingRequest::new(Fields::new());r.set_scheme(Some(&Scheme::Http)).unwrap();r.set_authority(Some("127.0.0.1:80")).unwrap();r.set_path_with_query(Some("/")).unwrap();
                json!({"denied":wasi::http::outgoing_handler::handle(r,None).is_err()}).to_string()
            },
            "/sql-escape"=>json!({"denied":call("database",json!({"statements":[{"sql":"ATTACH DATABASE '/tmp/loki-escape.db' AS escaped","params":[]}]})).is_err()}).to_string(),
            "/missing-capability"=>json!({"denied":call("file",json!({"path":"identity.pem"})).is_err()}).to_string(),
            "/missing-storage"=>json!({"denied":call("storage-get",json!({"key":"preferences"})).is_err()}).to_string(),
            "/storage"=>{
                call("storage-set",json!({"key":"preferences","value":{"theme":"forest"}})).unwrap();
                call("storage-file-write",json!({"path":"reports/branch.txt","data":"SGVsbG8sIGJyYW5jaC4="})).unwrap();
                let traversal=call("storage-file-write",json!({"path":"../another-app/secret","data":"eA=="})).unwrap_err();
                json!({"preferences":serde_json::from_str::<serde_json::Value>(&call("storage-get",json!({"key":"preferences"})).unwrap()).unwrap(),"traversal":serde_json::from_str::<serde_json::Value>(&traversal).unwrap()}).to_string()
            },
            "/storage-read"=>call("storage-get",json!({"key":"preferences"})).unwrap(),
            "/storage-download"=>{call("storage-file-serve",json!({"path":"reports/branch.txt","filename":"branch.txt","download":true})).unwrap();"".into()},
            "/storage-abort"=>{call("storage-file-serve",json!({"path":"reports/branch.txt","filename":"branch.txt","download":true})).unwrap();"denied".into()},
            "/storage-active"=>{call("storage-file-write",json!({"path":"active.html","data":"PGgxPmhpPC9oMT4="})).unwrap();call("storage-file-serve",json!({"path":"active.html","filename":"active.html","download":false})).unwrap();"".into()},
            "/cookies"=>json!({"cookies":request.headers().get("cookie").len(),"authorization":request.headers().get("authorization").len()}).to_string(),
            _=>call("context",json!(null)).unwrap(),
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
        if path=="/storage-abort" {response.set_status_code(403).unwrap();}
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
