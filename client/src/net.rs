//! fetch() wrappers for the /api routes. Every call carries the stored key as a
//! Bearer token.

use serde::{Serialize, de::DeserializeOwned};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{Blob, Headers, Request, RequestInit, Response};

const KEY: &str = "pod_key";

#[derive(Debug, Clone)]
pub struct HttpError {
    /// 0 when the request never got a response (offline, DNS, ...).
    pub status: u16,
    pub body: String,
}

impl HttpError {
    fn network(e: JsValue) -> Self {
        Self { status: 0, body: format!("{e:?}") }
    }

    /// The server's `{"error": ...}` message, or something readable.
    pub fn message(&self) -> String {
        if self.status == 0 {
            return "offline".into();
        }
        serde_json::from_str::<serde_json::Value>(&self.body)
            .ok()
            .and_then(|v| v["error"].as_str().map(String::from))
            .unwrap_or_else(|| format!("HTTP {}", self.status))
    }
}

pub type Result<T> = std::result::Result<T, HttpError>;

fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok()?
}

pub fn get_local(k: &str) -> Option<String> {
    storage()?.get_item(k).ok()?
}

pub fn set_local(k: &str, v: Option<&str>) {
    if let Some(s) = storage() {
        let _ = match v {
            Some(v) => s.set_item(k, v),
            None => s.remove_item(k),
        };
    }
}

/// Takes `?key=` from the address bar into localStorage (and out of the URL and
/// history), then returns the stored key.
pub fn init_key() -> Option<String> {
    let w = web_sys::window()?;
    let loc = w.location();
    let search = loc.search().ok()?;
    if let Ok(params) = web_sys::UrlSearchParams::new_with_str(&search)
        && let Some(k) = params.get("key")
    {
        set_local(KEY, Some(&k));
        if let Ok(h) = w.history() {
            let path = loc.pathname().unwrap_or_else(|_| "/".into());
            let _ = h.replace_state_with_url(&JsValue::NULL, "", Some(&path));
        }
    }
    key()
}

pub fn key() -> Option<String> {
    get_local(KEY).filter(|k| !k.is_empty())
}

pub fn set_key(k: Option<&str>) {
    set_local(KEY, k);
}

pub enum Body<'a> {
    None,
    Json(String),
    Bytes(&'a [u8]),
    Blob(&'a Blob),
}

pub async fn fetch(method: &str, path: &str, body: Body<'_>, range: Option<(u64, u64)>) -> Result<Response> {
    let init = RequestInit::new();
    init.set_method(method);
    let headers = Headers::new().map_err(HttpError::network)?;
    if let Some(k) = key() {
        headers.set("Authorization", &format!("Bearer {k}")).ok();
    }
    if let Some((a, b)) = range {
        headers.set("Range", &format!("bytes={a}-{}", b - 1)).ok();
    }
    match body {
        Body::None => {}
        Body::Json(s) => {
            headers.set("Content-Type", "application/json").ok();
            init.set_body(&JsValue::from_str(&s));
        }
        Body::Bytes(b) => init.set_body(&js_sys::Uint8Array::from(b)),
        Body::Blob(b) => init.set_body(b),
    }
    init.set_headers(&headers);
    let req = Request::new_with_str_and_init(&format!("/api{path}"), &init).map_err(HttpError::network)?;
    let resp: Response = JsFuture::from(web_sys::window().unwrap().fetch_with_request(&req))
        .await
        .map_err(HttpError::network)?
        .unchecked_into();
    if !resp.ok() {
        let status = resp.status();
        let body = match resp.text() {
            Ok(p) => JsFuture::from(p).await.ok().and_then(|v| v.as_string()).unwrap_or_default(),
            Err(_) => String::new(),
        };
        return Err(HttpError { status, body });
    }
    Ok(resp)
}

pub async fn bytes(resp: &Response) -> Result<Vec<u8>> {
    let buf = JsFuture::from(resp.array_buffer().map_err(HttpError::network)?).await.map_err(HttpError::network)?;
    Ok(js_sys::Uint8Array::new(&buf).to_vec())
}

pub async fn blob_bytes(b: &Blob) -> std::result::Result<Vec<u8>, String> {
    let buf = JsFuture::from(b.array_buffer()).await.map_err(|e| format!("{e:?}"))?;
    Ok(js_sys::Uint8Array::new(&buf).to_vec())
}

async fn text(resp: Response) -> Result<String> {
    let v = JsFuture::from(resp.text().map_err(HttpError::network)?).await.map_err(HttpError::network)?;
    Ok(v.as_string().unwrap_or_default())
}

fn parse<T: DeserializeOwned>(s: &str) -> Result<T> {
    serde_json::from_str(s).map_err(|e| HttpError { status: 500, body: format!("{{\"error\":\"bad response: {e}\"}}") })
}

pub async fn get<T: DeserializeOwned>(path: &str) -> Result<T> {
    parse(&text(fetch("GET", path, Body::None, None).await?).await?)
}

pub async fn send<T: DeserializeOwned>(method: &str, path: &str, body: &impl Serialize) -> Result<T> {
    let s = text(fetch(method, path, Body::Json(serde_json::to_string(body).unwrap()), None).await?).await?;
    if s.is_empty() { parse("null") } else { parse(&s) }
}

pub async fn get_range(path: &str, start: u64, end: u64) -> Result<Vec<u8>> {
    if start >= end {
        return Ok(Vec::new());
    }
    let resp = fetch("GET", path, Body::None, Some((start, end))).await?;
    let mut b = bytes(&resp).await?;
    if resp.status() == 200 {
        // Server ignored the range and sent everything.
        b = b.get(start as usize..(end as usize).min(b.len())).unwrap_or_default().to_vec();
    }
    Ok(b)
}

/// Link that works without headers (for a plain download in a new tab).
pub fn keyed_url(path: &str) -> String {
    let k = key().unwrap_or_default();
    let enc: String = js_sys::encode_uri_component(&k).into();
    format!("/api{path}?key={enc}")
}
