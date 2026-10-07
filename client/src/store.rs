//! Blob access with a per-device cache. Audio is kept in the browser's Cache
//! Storage keyed by its content hash, so each device downloads a WAV once; until
//! it has, reads fall back to HTTP Range requests against the server.

use std::{cell::RefCell, collections::HashMap};

use wasm_bindgen::{JsCast, JsValue, closure::Closure};
use wasm_bindgen_futures::JsFuture;
use web_sys::{Blob, Cache, ReadableStream, ReadableStreamDefaultReader, Response};

use crate::net;

const CACHE: &str = "pod-blobs-v1";

thread_local! {
    /// Blobs available locally (cached, or the File just imported on this device).
    static LOCAL: RefCell<HashMap<String, Blob>> = RefCell::new(HashMap::new());
}

fn path(hash: &str) -> String {
    format!("/blobs/{hash}")
}

fn cache_key(hash: &str) -> String {
    format!("/api/blobs/{hash}")
}

pub fn register(hash: &str, blob: Blob) {
    LOCAL.with(|m| m.borrow_mut().insert(hash.to_string(), blob));
}

pub fn is_local(hash: &str) -> bool {
    LOCAL.with(|m| m.borrow().contains_key(hash))
}

fn local(hash: &str) -> Option<Blob> {
    LOCAL.with(|m| m.borrow().get(hash).cloned())
}

async fn open() -> Result<Cache, String> {
    let caches = web_sys::window().unwrap().caches().map_err(|e| format!("no Cache Storage: {e:?}"))?;
    Ok(JsFuture::from(caches.open(CACHE)).await.map_err(|e| format!("{e:?}"))?.unchecked_into())
}

/// Looks the blob up in Cache Storage and registers it if found.
pub async fn load_cached(hash: &str) -> bool {
    if is_local(hash) {
        return true;
    }
    let Ok(cache) = open().await else { return false };
    let Ok(v) = JsFuture::from(cache.match_with_str(&cache_key(hash))).await else { return false };
    if v.is_undefined() || v.is_null() {
        return false;
    }
    let resp: Response = v.unchecked_into();
    let Ok(p) = resp.blob() else { return false };
    let Ok(b) = JsFuture::from(p).await else { return false };
    register(hash, b.unchecked_into());
    true
}

/// Stores a blob we already have (e.g. a just-imported File) in the cache.
pub async fn put(hash: &str, blob: &Blob) {
    register(hash, blob.clone());
    if let Ok(cache) = open().await
        && let Ok(resp) = Response::new_with_opt_blob(Some(blob))
    {
        let _ = JsFuture::from(cache.put_with_str(&cache_key(hash), &resp)).await;
    }
}

/// Reads bytes `[start, end)` of a blob, locally if possible.
pub async fn read(hash: &str, start: u64, end: u64) -> Result<Vec<u8>, String> {
    if let Some(b) = local(hash) {
        let s = b.slice_with_f64_and_f64(start as f64, end as f64).map_err(|e| format!("{e:?}"))?;
        return net::blob_bytes(&s).await;
    }
    net::get_range(&path(hash), start, end).await.map_err(|e| e.message())
}

/// Reads a small blob whole (peaks), through the cache.
pub async fn read_all(hash: &str) -> Result<Vec<u8>, String> {
    if !load_cached(hash).await {
        let resp = net::fetch("GET", &path(hash), net::Body::None, None).await.map_err(|e| e.message())?;
        let bytes = net::bytes(&resp).await.map_err(|e| e.message())?;
        let blob = Blob::new_with_u8_array_sequence(&js_sys::Array::of1(&js_sys::Uint8Array::from(&bytes[..])))
            .map_err(|e| format!("{e:?}"))?;
        put(hash, &blob).await;
        return Ok(bytes);
    }
    net::blob_bytes(&local(hash).unwrap()).await
}

/// Downloads a whole blob into the cache, reporting bytes received.
pub async fn download(hash: &str, mut progress: impl FnMut(u64) + 'static) -> Result<(), String> {
    if load_cached(hash).await {
        return Ok(());
    }
    let resp = net::fetch("GET", &path(hash), net::Body::None, None).await.map_err(|e| e.message())?;
    let body: ReadableStream = resp.body().ok_or("empty response")?;
    // One branch goes into the cache, the other is only counted for progress.
    let pair = body.tee();
    let (to_cache, to_count): (ReadableStream, ReadableStream) =
        (pair.get(0).unchecked_into(), pair.get(1).unchecked_into());
    let reader: ReadableStreamDefaultReader = to_count.get_reader().unchecked_into();
    wasm_bindgen_futures::spawn_local(async move {
        let mut n = 0u64;
        while let Ok(r) = JsFuture::from(reader.read()).await {
            if js_sys::Reflect::get(&r, &"done".into()).ok().and_then(|d| d.as_bool()).unwrap_or(true) {
                break;
            }
            if let Ok(v) = js_sys::Reflect::get(&r, &"value".into()) {
                n += v.unchecked_ref::<js_sys::Uint8Array>().length() as u64;
                progress(n);
            }
        }
    });
    let cache = open().await?;
    let cached = Response::new_with_opt_readable_stream(Some(&to_cache)).map_err(|e| format!("{e:?}"))?;
    JsFuture::from(cache.put_with_str(&cache_key(hash), &cached)).await.map_err(|e| format!("caching failed: {e:?}"))?;
    if load_cached(hash).await { Ok(()) } else { Err("cache unavailable".into()) }
}

/// Asks the browser not to evict our cache under storage pressure.
pub fn request_persistence() {
    if let Some(w) = web_sys::window() {
        let storage = js_sys::Reflect::get(&w.navigator(), &"storage".into()).unwrap_or(JsValue::UNDEFINED);
        if let Ok(f) = js_sys::Reflect::get(&storage, &"persist".into())
            && let Some(f) = f.dyn_ref::<js_sys::Function>()
        {
            let _ = f.call0(&storage);
        }
    }
}

/// Opens a file picker; `on_pick` gets the chosen files.
pub fn pick_files(accept: &str, on_pick: impl FnOnce(Vec<web_sys::File>) + 'static) {
    let doc = web_sys::window().unwrap().document().unwrap();
    let input: web_sys::HtmlInputElement = doc.create_element("input").unwrap().unchecked_into();
    input.set_type("file");
    input.set_accept(accept);
    input.set_multiple(true);
    let inp = input.clone();
    let mut on_pick = Some(on_pick);
    let cb = Closure::<dyn FnMut()>::new(move || {
        let files = inp.files();
        let list = (0..files.as_ref().map_or(0, |f| f.length())).filter_map(|i| files.as_ref()?.get(i)).collect();
        if let Some(f) = on_pick.take() {
            f(list);
        }
    });
    input.set_onchange(Some(cb.as_ref().unchecked_ref()));
    cb.forget();
    input.click();
}
