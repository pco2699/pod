//! pod server: stores WAV/peaks blobs, project op logs and exports. All editing,
//! decoding and rendering happens in the browser; this only persists and serves.
//!
//! Auth (same scheme as kusa): `Authorization: Bearer <token>` or `?key=<token>`
//! on every /api route, or the `proxy_auth_header` a signing-in reverse proxy sets.
//! The app shell itself is public and holds no data.
//!
//! On disk, under `data_dir`:
//!   blobs/<sha256>             immutable, content addressed
//!   uploads/<sha256>.part      partial upload, appended chunk by chunk
//!   projects/<id>/meta.json    { name, created }
//!   projects/<id>/ops.jsonl    one op per line; version = line count
//!   projects/<id>/view.json    last playhead/zoom/selection (last writer wins)
//!   exports/<id>.wav + .json   rendered mixdowns

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Path as UrlPath, Query, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, Uri, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use pod_core::{Op, Project};
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncWriteExt},
    sync::Mutex,
};
use tower::ServiceExt;
use tower_http::services::ServeFile;

/// Upload chunks are 8 MiB; leave headroom.
const MAX_BODY: usize = 16 * 1024 * 1024;

#[derive(RustEmbed)]
#[folder = "../client/dist"]
struct Assets;

#[derive(Deserialize)]
struct Config {
    port: u16,
    token: String,
    #[serde(default = "default_data_dir")]
    data_dir: PathBuf,
    /// Header a signing-in reverse proxy (oauth2-proxy behind nginx auth_request,
    /// say) sets for a logged-in user; a request carrying it needs no key. Only
    /// safe when the proxy always overwrites it and nothing else reaches the port.
    #[serde(default)]
    proxy_auth_header: Option<String>,
}

fn default_data_dir() -> PathBuf {
    "data".into()
}

#[derive(Clone, Serialize, Deserialize)]
struct Meta {
    name: String,
    created: u64,
}

struct Entry {
    meta: Meta,
    project: Project,
    ops: Vec<Op>,
    view: Value,
}

struct App {
    token: String,
    proxy_auth_header: Option<header::HeaderName>,
    dir: PathBuf,
    projects: Mutex<HashMap<String, Entry>>,
    /// Serializes chunk appends so offsets stay consistent.
    uploads: Mutex<()>,
}

type Shared = Arc<App>;

#[derive(Debug)]
struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<std::io::Error> for ApiError {
    fn from(e: std::io::Error) -> Self {
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    }
}

type ApiResult<T> = Result<T, ApiError>;

fn bad(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, msg.into())
}

fn not_found() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "not found".into())
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn new_id() -> String {
    format!("{:016x}", rand::random::<u64>())
}

fn valid_hash(h: &str) -> bool {
    h.len() == 64 && h.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 32 && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let cfg_path = std::env::args().nth(1).unwrap_or_else(|| "config.json".into());
    let cfg: Config = serde_json::from_str(
        &std::fs::read_to_string(&cfg_path).unwrap_or_else(|e| panic!("reading {cfg_path}: {e}")),
    )
    .expect("parsing config");
    if cfg.token.len() < 16 {
        panic!("token must be at least 16 characters");
    }
    for sub in ["blobs", "uploads", "projects", "exports"] {
        std::fs::create_dir_all(cfg.data_dir.join(sub)).expect("creating data dirs");
    }
    let projects = load_projects(&cfg.data_dir.join("projects")).await;
    tracing::info!("loaded {} projects", projects.len());
    let proxy_auth_header = cfg.proxy_auth_header.map(|h| {
        header::HeaderName::try_from(h.as_str()).unwrap_or_else(|e| panic!("proxy_auth_header {h:?}: {e}"))
    });
    let app = Arc::new(App {
        token: cfg.token,
        proxy_auth_header,
        dir: cfg.data_dir,
        projects: Mutex::new(projects),
        uploads: Mutex::new(()),
    });

    let api = Router::new()
        .route("/projects", get(list_projects).post(create_project))
        .route("/projects/{id}", get(get_project).patch(rename_project))
        .route("/projects/{id}/ops", get(get_ops).post(post_ops))
        .route("/projects/{id}/view", put(put_view))
        .route("/projects/{id}/exports", post(create_export))
        .route("/blobs/{hash}", get(get_blob).put(put_blob_chunk))
        .route("/blobs/{hash}/status", get(blob_status))
        .route("/blobs/{hash}/complete", post(complete_blob))
        .route("/exports/{id}", get(get_export).put(put_export_chunk).delete(delete_export))
        .route("/exports/{id}/complete", post(complete_export))
        .layer(middleware::from_fn_with_state(app.clone(), auth))
        .layer(DefaultBodyLimit::max(MAX_BODY));

    let router = Router::new().nest("/api", api).fallback(static_file).with_state(app);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", cfg.port)).await.expect("bind");
    tracing::info!("listening on 127.0.0.1:{}", cfg.port);
    axum::serve(listener, router).await.unwrap();
}

async fn load_projects(dir: &Path) -> HashMap<String, Entry> {
    let mut out = HashMap::new();
    let Ok(mut rd) = fs::read_dir(dir).await else { return out };
    while let Ok(Some(e)) = rd.next_entry().await {
        let id = e.file_name().to_string_lossy().to_string();
        let p = e.path();
        let Ok(meta) = fs::read_to_string(p.join("meta.json")).await else { continue };
        let Ok(meta) = serde_json::from_str::<Meta>(&meta) else { continue };
        let mut project = Project::default();
        let mut ops = Vec::new();
        for line in fs::read_to_string(p.join("ops.jsonl")).await.unwrap_or_default().lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Op>(line).map_err(|e| e.to_string()).and_then(|op| {
                project.apply(&op)?;
                Ok(op)
            }) {
                Ok(op) => ops.push(op),
                Err(e) => tracing::warn!("project {id}: skipping op: {e}"),
            }
        }
        let view = fs::read_to_string(p.join("view.json"))
            .await
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(Value::Null);
        out.insert(id, Entry { meta, project, ops, view });
    }
    out
}

async fn auth(State(app): State<Shared>, req: Request, next: Next) -> Response {
    let vouched = app
        .proxy_auth_header
        .as_ref()
        .is_some_and(|h| req.headers().get(h).is_some_and(|v| !v.is_empty()));
    if vouched {
        return next.run(req).await;
    }
    let bearer = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let key = req.uri().query().and_then(|q| {
        q.split('&').find_map(|kv| kv.strip_prefix("key=")).map(|k| percent_decode(k))
    });
    let ok = bearer.is_some_and(|b| constant_eq(b, &app.token))
        || key.is_some_and(|k| constant_eq(&k, &app.token));
    if !ok {
        return ApiError(StatusCode::UNAUTHORIZED, "unauthorized".into()).into_response();
    }
    next.run(req).await
}

fn constant_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------- projects ----------

async fn list_projects(State(app): State<Shared>) -> Json<Value> {
    let ps = app.projects.lock().await;
    let mut list: Vec<Value> = ps
        .iter()
        .map(|(id, e)| {
            json!({
                "id": id,
                "name": e.meta.name,
                "created": e.meta.created,
                "version": e.ops.len(),
                "tracks": e.project.tracks.len(),
                "frames": e.project.len(),
                "sample_rate": e.project.sample_rate,
            })
        })
        .collect();
    list.sort_by_key(|v| std::cmp::Reverse(v["created"].as_u64()));
    Json(Value::Array(list))
}

#[derive(Deserialize)]
struct NameBody {
    name: String,
}

async fn create_project(State(app): State<Shared>, Json(b): Json<NameBody>) -> ApiResult<Json<Value>> {
    let name = b.name.trim();
    if name.is_empty() {
        return Err(bad("name required"));
    }
    let id = new_id();
    let meta = Meta { name: name.to_string(), created: now() };
    let dir = app.dir.join("projects").join(&id);
    fs::create_dir_all(&dir).await?;
    fs::write(dir.join("meta.json"), serde_json::to_vec(&meta).unwrap()).await?;
    fs::write(dir.join("ops.jsonl"), b"").await?;
    app.projects.lock().await.insert(
        id.clone(),
        Entry { meta, project: Project::default(), ops: Vec::new(), view: Value::Null },
    );
    Ok(Json(json!({ "id": id })))
}

async fn rename_project(
    State(app): State<Shared>,
    UrlPath(id): UrlPath<String>,
    Json(b): Json<NameBody>,
) -> ApiResult<StatusCode> {
    let mut ps = app.projects.lock().await;
    let e = ps.get_mut(&id).ok_or_else(not_found)?;
    e.meta.name = b.name.trim().to_string();
    fs::write(app.dir.join("projects").join(&id).join("meta.json"), serde_json::to_vec(&e.meta).unwrap()).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_project(State(app): State<Shared>, UrlPath(id): UrlPath<String>) -> ApiResult<Json<Value>> {
    let exports = list_exports(&app.dir, &id).await;
    let ps = app.projects.lock().await;
    let e = ps.get(&id).ok_or_else(not_found)?;
    Ok(Json(json!({
        "id": id,
        "name": e.meta.name,
        "version": e.ops.len(),
        "project": e.project,
        "view": e.view,
        "exports": exports,
    })))
}

#[derive(Deserialize)]
struct Since {
    #[serde(default)]
    since: usize,
}

async fn get_ops(
    State(app): State<Shared>,
    UrlPath(id): UrlPath<String>,
    Query(q): Query<Since>,
) -> ApiResult<Json<Value>> {
    let ps = app.projects.lock().await;
    let e = ps.get(&id).ok_or_else(not_found)?;
    let since = q.since.min(e.ops.len());
    Ok(Json(json!({ "version": e.ops.len(), "ops": &e.ops[since..] })))
}

#[derive(Deserialize)]
struct OpsBody {
    base: usize,
    ops: Vec<Op>,
}

/// Appends ops if the client was up to date; otherwise 409 with what it missed so
/// it can rebase its pending ops and retry.
async fn post_ops(
    State(app): State<Shared>,
    UrlPath(id): UrlPath<String>,
    Json(b): Json<OpsBody>,
) -> ApiResult<Response> {
    let mut ps = app.projects.lock().await;
    let e = ps.get_mut(&id).ok_or_else(not_found)?;
    if b.base != e.ops.len() {
        let since = b.base.min(e.ops.len());
        return Ok((StatusCode::CONFLICT, Json(json!({ "version": e.ops.len(), "ops": &e.ops[since..] })))
            .into_response());
    }
    let mut next = e.project.clone();
    let mut lines = String::new();
    for op in &b.ops {
        if let Op::AddTrack { source, .. } = op {
            for h in [&source.wav, &source.peaks] {
                if !valid_hash(h) || !app.dir.join("blobs").join(h).exists() {
                    return Err(bad(format!("blob {h} has not been uploaded")));
                }
            }
        }
        next.apply(op).map_err(bad)?;
        lines.push_str(&serde_json::to_string(op).unwrap());
        lines.push('\n');
    }
    let mut f = fs::OpenOptions::new().append(true).open(app.dir.join("projects").join(&id).join("ops.jsonl")).await?;
    f.write_all(lines.as_bytes()).await?;
    f.sync_data().await?;
    e.project = next;
    e.ops.extend(b.ops);
    Ok(Json(json!({ "version": e.ops.len() })).into_response())
}

async fn put_view(
    State(app): State<Shared>,
    UrlPath(id): UrlPath<String>,
    Json(view): Json<Value>,
) -> ApiResult<StatusCode> {
    let mut ps = app.projects.lock().await;
    let e = ps.get_mut(&id).ok_or_else(not_found)?;
    fs::write(app.dir.join("projects").join(&id).join("view.json"), serde_json::to_vec(&view).unwrap()).await?;
    e.view = view;
    Ok(StatusCode::NO_CONTENT)
}

// ---------- blobs ----------

#[derive(Deserialize)]
struct Offset {
    offset: u64,
}

fn blob_paths(app: &App, hash: &str) -> ApiResult<(PathBuf, PathBuf)> {
    if !valid_hash(hash) {
        return Err(bad("bad hash"));
    }
    Ok((app.dir.join("blobs").join(hash), app.dir.join("uploads").join(format!("{hash}.part"))))
}

async fn file_len(p: &Path) -> Option<u64> {
    fs::metadata(p).await.ok().map(|m| m.len())
}

async fn blob_status(State(app): State<Shared>, UrlPath(hash): UrlPath<String>) -> ApiResult<Json<Value>> {
    let (done, part) = blob_paths(&app, &hash)?;
    Ok(Json(match file_len(&done).await {
        Some(n) => json!({ "complete": true, "size": n }),
        None => json!({ "complete": false, "size": file_len(&part).await.unwrap_or(0) }),
    }))
}

/// Appends `body` at `offset`, which must equal the bytes received so far.
async fn append_chunk(path: &Path, offset: u64, body: &[u8]) -> ApiResult<Response> {
    let have = file_len(path).await.unwrap_or(0);
    if offset != have {
        return Ok((StatusCode::CONFLICT, Json(json!({ "size": have }))).into_response());
    }
    let mut f = fs::OpenOptions::new().create(true).append(true).open(path).await?;
    f.write_all(body).await?;
    f.flush().await?;
    Ok(Json(json!({ "size": have + body.len() as u64 })).into_response())
}

async fn put_blob_chunk(
    State(app): State<Shared>,
    UrlPath(hash): UrlPath<String>,
    Query(q): Query<Offset>,
    body: Bytes,
) -> ApiResult<Response> {
    let (done, part) = blob_paths(&app, &hash)?;
    let _g = app.uploads.lock().await;
    if done.exists() {
        return Ok(Json(json!({ "complete": true })).into_response());
    }
    append_chunk(&part, q.offset, &body).await
}

async fn complete_blob(State(app): State<Shared>, UrlPath(hash): UrlPath<String>) -> ApiResult<Json<Value>> {
    let (done, part) = blob_paths(&app, &hash)?;
    let _g = app.uploads.lock().await;
    if done.exists() {
        return Ok(Json(json!({ "complete": true })));
    }
    let actual = sha256_file(&part).await?;
    if actual != hash {
        fs::remove_file(&part).await.ok();
        return Err(bad("hash mismatch; upload discarded, please retry"));
    }
    fs::rename(&part, &done).await?;
    Ok(Json(json!({ "complete": true })))
}

async fn sha256_file(p: &Path) -> std::io::Result<String> {
    let mut f = fs::File::open(p).await?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

/// Serves a file with Range support (the client streams playback this way).
async fn serve_file(path: PathBuf, req: Request, extra: HeaderMap) -> ApiResult<Response> {
    if !path.exists() {
        return Err(not_found());
    }
    let mut res = match ServeFile::new(path).oneshot(req).await {
        Ok(r) => r.map(Body::new),
        Err(e) => match e {},
    };
    res.headers_mut().extend(extra);
    Ok(res)
}

async fn get_blob(State(app): State<Shared>, UrlPath(hash): UrlPath<String>, req: Request) -> ApiResult<Response> {
    let (done, _) = blob_paths(&app, &hash)?;
    let mut h = HeaderMap::new();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, max-age=31536000, immutable"));
    serve_file(done, req, h).await
}

// ---------- exports ----------

#[derive(Clone, Serialize, Deserialize)]
struct ExportMeta {
    id: String,
    project: String,
    name: String,
    size: u64,
    complete: bool,
    created: u64,
}

fn export_paths(app: &App, id: &str) -> ApiResult<(PathBuf, PathBuf)> {
    if !valid_id(id) {
        return Err(bad("bad id"));
    }
    let d = app.dir.join("exports");
    Ok((d.join(format!("{id}.wav")), d.join(format!("{id}.json"))))
}

async fn read_export(app: &App, id: &str) -> ApiResult<ExportMeta> {
    let (_, meta) = export_paths(app, id)?;
    let s = fs::read_to_string(meta).await.map_err(|_| not_found())?;
    serde_json::from_str(&s).map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
}

async fn list_exports(dir: &Path, project: &str) -> Vec<ExportMeta> {
    let mut out = Vec::new();
    let Ok(mut rd) = fs::read_dir(dir.join("exports")).await else { return out };
    while let Ok(Some(e)) = rd.next_entry().await {
        if e.path().extension().is_some_and(|x| x == "json")
            && let Ok(s) = fs::read_to_string(e.path()).await
            && let Ok(m) = serde_json::from_str::<ExportMeta>(&s)
            && m.project == project
            && m.complete
        {
            out.push(m);
        }
    }
    out.sort_by_key(|m| std::cmp::Reverse(m.created));
    out
}

#[derive(Deserialize)]
struct ExportBody {
    name: String,
    size: u64,
}

async fn create_export(
    State(app): State<Shared>,
    UrlPath(project): UrlPath<String>,
    Json(b): Json<ExportBody>,
) -> ApiResult<Json<Value>> {
    if !app.projects.lock().await.contains_key(&project) {
        return Err(not_found());
    }
    let id = new_id();
    let meta = ExportMeta { id: id.clone(), project, name: b.name, size: b.size, complete: false, created: now() };
    let (_, mp) = export_paths(&app, &id)?;
    fs::write(mp, serde_json::to_vec(&meta).unwrap()).await?;
    Ok(Json(json!({ "id": id })))
}

async fn put_export_chunk(
    State(app): State<Shared>,
    UrlPath(id): UrlPath<String>,
    Query(q): Query<Offset>,
    body: Bytes,
) -> ApiResult<Response> {
    let m = read_export(&app, &id).await?;
    if m.complete || q.offset + body.len() as u64 > m.size {
        return Err(bad("export is complete or chunk exceeds its size"));
    }
    let (wav, _) = export_paths(&app, &id)?;
    let _g = app.uploads.lock().await;
    append_chunk(&wav, q.offset, &body).await
}

async fn complete_export(State(app): State<Shared>, UrlPath(id): UrlPath<String>) -> ApiResult<Json<ExportMeta>> {
    let mut m = read_export(&app, &id).await?;
    let (wav, mp) = export_paths(&app, &id)?;
    let have = file_len(&wav).await.unwrap_or(0);
    if have != m.size {
        return Err(bad(format!("export has {have} of {} bytes", m.size)));
    }
    m.complete = true;
    fs::write(mp, serde_json::to_vec(&m).unwrap()).await?;
    Ok(Json(m))
}

async fn get_export(State(app): State<Shared>, UrlPath(id): UrlPath<String>, req: Request) -> ApiResult<Response> {
    let m = read_export(&app, &id).await?;
    let (wav, _) = export_paths(&app, &id)?;
    let safe: String =
        m.name.chars().map(|c| if c.is_ascii_alphanumeric() || " -_.()".contains(c) { c } else { '_' }).collect();
    let mut h = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{safe}.wav\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    serve_file(wav, req, h).await
}

async fn delete_export(State(app): State<Shared>, UrlPath(id): UrlPath<String>) -> ApiResult<StatusCode> {
    let (wav, mp) = export_paths(&app, &id)?;
    read_export(&app, &id).await?;
    fs::remove_file(wav).await.ok();
    fs::remove_file(mp).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------- app shell ----------

async fn static_file(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let (path, file) = match Assets::get(path) {
        Some(f) if !path.is_empty() => (path, f),
        _ => match Assets::get("index.html") {
            Some(f) => ("index.html", f),
            None => return (StatusCode::NOT_FOUND, "client not built").into_response(),
        },
    };
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    // trunk content-hashes the wasm/js names, so those can be cached forever.
    let cache = if path.ends_with(".wasm") || (path.ends_with(".js") && path.contains('-')) {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    (
        [(header::CONTENT_TYPE, mime.as_ref().to_string()), (header::CACHE_CONTROL, cache.to_string())],
        file.data.into_owned(),
    )
        .into_response()
}
