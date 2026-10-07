//! Import (analyze + upload a WAV) and export (render + upload a mixdown).

use pod_core::{Source, Track, peaks::PeakBuilder, wav};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use web_sys::{Blob, File};

use crate::{audio, net, store};

const CHUNK: u64 = 8 * 1024 * 1024;

async fn slice(b: &Blob, start: u64, end: u64) -> Result<Vec<u8>, String> {
    let s = b.slice_with_f64_and_f64(start as f64, end as f64).map_err(|e| format!("{e:?}"))?;
    net::blob_bytes(&s).await
}

#[derive(Deserialize)]
struct Status {
    complete: bool,
    size: u64,
}

/// Uploads `blob` as `/blobs/<hash>`, resuming a partial upload if there is one.
async fn upload_blob(hash: &str, blob: &Blob, mut progress: impl FnMut(f32)) -> Result<(), String> {
    let total = blob.size() as u64;
    let st: Status = net::get(&format!("/blobs/{hash}/status")).await.map_err(|e| e.message())?;
    if st.complete {
        return Ok(());
    }
    let mut off = st.size.min(total);
    while off < total {
        let end = (off + CHUNK).min(total);
        let part = blob.slice_with_f64_and_f64(off as f64, end as f64).map_err(|e| format!("{e:?}"))?;
        match net::fetch("PUT", &format!("/blobs/{hash}?offset={off}"), net::Body::Blob(&part), None).await {
            Ok(_) => off = end,
            Err(e) if e.status == 409 => {
                // Server has a different amount than we thought: continue from there.
                off = serde_json::from_str::<serde_json::Value>(&e.body)
                    .ok()
                    .and_then(|v| v["size"].as_u64())
                    .ok_or("upload out of sync")?;
            }
            Err(e) => return Err(e.message()),
        }
        progress(off as f32 / total.max(1) as f32);
    }
    net::fetch("POST", &format!("/blobs/{hash}/complete"), net::Body::None, None).await.map_err(|e| e.message())?;
    Ok(())
}

/// Hashes the file, builds its waveform peaks, uploads both and caches them
/// locally. Returns the track source. `progress(phase, fraction)`.
pub async fn import(file: File, mut progress: impl FnMut(&str, f32)) -> Result<Source, String> {
    let size = file.size() as u64;
    let head = slice(&file, 0, size.min(1 << 16)).await?;
    let fmt = wav::parse_header(&head, size)?;
    let data_end = fmt.data_offset + fmt.frames * fmt.bytes_per_frame();

    // One pass: sha256 over the whole file, peaks over the samples.
    let mut hash = Sha256::new();
    let mut peaks = PeakBuilder::new(fmt.channels);
    hash.update(&head[..fmt.data_offset as usize]);
    let step = CHUNK / fmt.bytes_per_frame() * fmt.bytes_per_frame();
    let mut off = fmt.data_offset;
    let mut samples = Vec::new();
    while off < size {
        let end = if off < data_end { (off + step).min(data_end) } else { size };
        let bytes = slice(&file, off, end).await?;
        hash.update(&bytes);
        if off < data_end {
            samples.clear();
            wav::decode(&bytes, &fmt, &mut samples);
            peaks.push(&samples);
        }
        off = end;
        progress("Analyzing", off as f32 / size as f32);
    }
    let wav_hash = hex::encode(hash.finalize());
    let peaks_bytes = peaks.finish();
    let peaks_hash = hex::encode(Sha256::digest(&peaks_bytes));
    let peaks_blob =
        Blob::new_with_u8_array_sequence(&js_sys::Array::of1(&js_sys::Uint8Array::from(&peaks_bytes[..])))
            .map_err(|e| format!("{e:?}"))?;

    upload_blob(&peaks_hash, &peaks_blob, |_| {}).await?;
    store::put(&peaks_hash, &peaks_blob).await;
    upload_blob(&wav_hash, &file, |f| progress("Uploading", f)).await?;
    store::put(&wav_hash, &file).await;
    Ok(Source { wav: wav_hash, peaks: peaks_hash, format: fmt })
}

#[derive(Deserialize, Clone)]
pub struct ExportInfo {
    pub id: String,
    pub name: String,
    pub size: u64,
}

/// Renders `tracks` from 0 to `frames` as 24-bit WAV, streaming it to the server.
pub async fn export(
    project: &str,
    name: &str,
    tracks: Vec<Track>,
    rate: u32,
    frames: u64,
    mut progress: impl FnMut(f32),
) -> Result<ExportInfo, String> {
    let ch = audio::out_channels(&tracks);
    let header = wav::header_pcm24(rate, ch as u16, frames);
    let size = header.len() as u64 + frames * ch as u64 * 3;
    #[derive(Deserialize)]
    struct Created {
        id: String,
    }
    let c: Created = net::send(
        "POST",
        &format!("/projects/{project}/exports"),
        &serde_json::json!({ "name": name, "size": size }),
    )
    .await
    .map_err(|e| e.message())?;

    let step = 10 * rate as u64;
    let mut buf = header;
    let mut off = 0u64;
    let mut at = 0u64;
    while at < frames {
        let n = step.min(frames - at);
        let mix = audio::render(&tracks, at, n, ch).await?;
        wav::encode_pcm24(&mix, &mut buf);
        net::fetch("PUT", &format!("/exports/{}?offset={off}", c.id), net::Body::Bytes(&buf), None)
            .await
            .map_err(|e| e.message())?;
        off += buf.len() as u64;
        buf.clear();
        at += n;
        progress(at as f32 / frames as f32);
    }
    net::send("POST", &format!("/exports/{}/complete", c.id), &()).await.map_err(|e| e.message())
}
