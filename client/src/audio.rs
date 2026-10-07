//! Mixing the edited timeline and streaming it to the speakers.
//!
//! Nothing is ever decoded whole: playback renders ~1 s chunks just ahead of the
//! playhead and schedules them back to back as AudioBuffers; export renders the
//! same way into upload chunks. Memory stays flat however long the episode is.

use std::cell::RefCell;

use pod_core::{Track, wav};
use wasm_bindgen::{JsCast, closure::Closure};
use web_sys::{AudioBufferSourceNode, AudioContext};

use crate::store;

/// Renders timeline `[start, start + len)` of `tracks` mixed down to `out_ch`
/// interleaved channels.
pub async fn render(tracks: &[Track], start: u64, len: u64, out_ch: usize) -> Result<Vec<f32>, String> {
    let mut out = vec![0f32; len as usize * out_ch];
    for t in tracks {
        let fmt = t.source.format;
        let ch = fmt.channels as usize;
        for p in t.pieces(start, start + len) {
            let n = p.len.min(fmt.frames.saturating_sub(p.src));
            if p.muted || n == 0 {
                continue;
            }
            let (a, b) = fmt.byte_range(p.src, n);
            let bytes = store::read(&t.source.wav, a, b).await?;
            let mut s = Vec::with_capacity(n as usize * ch);
            wav::decode(&bytes, &fmt, &mut s);
            let base = (p.at - start) as usize * out_ch;
            for (i, frame) in s.chunks_exact(ch).enumerate() {
                let o = &mut out[base + i * out_ch..base + (i + 1) * out_ch];
                if ch == out_ch {
                    o.iter_mut().zip(frame).for_each(|(o, s)| *o += s);
                } else if ch == 1 {
                    o.iter_mut().for_each(|o| *o += frame[0]);
                } else {
                    // Fold extra channels (or downmix to mono).
                    let k = out_ch as f32 / ch as f32;
                    for (c, s) in frame.iter().enumerate() {
                        o[c % out_ch] += s * k.min(1.0);
                    }
                }
            }
        }
    }
    Ok(out)
}

/// Output channel count for a set of tracks: stereo if any track is.
pub fn out_channels(tracks: &[Track]) -> usize {
    if tracks.iter().any(|t| t.source.format.channels > 1) { 2 } else { 1 }
}

thread_local! {
    static CTX: RefCell<Option<AudioContext>> = const { RefCell::new(None) };
}

/// Mobile browsers only let audio start inside a real input event, and egui
/// handles input later, on its own frame. So create/resume the AudioContext from
/// raw DOM listeners on every tap, click and key press.
pub fn install_unlock() {
    let doc = web_sys::window().unwrap().document().unwrap();
    let cb = Closure::<dyn FnMut()>::new(|| {
        let ctx = context();
        if ctx.state() != web_sys::AudioContextState::Running {
            let _ = ctx.resume();
            // Old iOS also wants a sound to be started in the gesture.
            if let Ok(b) = ctx.create_buffer(1, 1, 22050.0)
                && let Ok(src) = ctx.create_buffer_source()
            {
                src.set_buffer(Some(&b));
                let _ = src.connect_with_audio_node(&ctx.destination());
                let _ = src.start();
            }
        }
    });
    for ev in ["pointerdown", "pointerup", "touchend", "keydown"] {
        let _ = doc.add_event_listener_with_callback_and_bool(ev, cb.as_ref().unchecked_ref(), true);
    }
    cb.forget();
}

pub fn context() -> AudioContext {
    CTX.with(|c| c.borrow_mut().get_or_insert_with(|| AudioContext::new().expect("Web Audio unavailable")).clone())
}

/// Seconds of audio to keep scheduled ahead of the playhead.
const LOOKAHEAD: f64 = 2.0;
/// Seconds per rendered chunk.
const CHUNK_SECS: u64 = 1;

pub struct Player {
    pub playing: bool,
    /// Bumped on every start/stop so chunks from an old run are dropped.
    pub generation: u32,
    rate: u32,
    origin_frame: u64,
    /// Context time at which `origin_frame` sounds; set when the first chunk lands.
    origin_time: Option<f64>,
    next_frame: u64,
    end_frame: u64,
    in_flight: bool,
    sources: Vec<(f64, AudioBufferSourceNode)>,
}

pub struct ChunkRequest {
    pub generation: u32,
    pub start: u64,
    pub len: u64,
}

impl Player {
    pub fn new() -> Self {
        Self {
            playing: false,
            generation: 0,
            rate: 48000,
            origin_frame: 0,
            origin_time: None,
            next_frame: 0,
            end_frame: 0,
            in_flight: false,
            sources: Vec::new(),
        }
    }

    pub fn start(&mut self, frame: u64, end: u64, rate: u32) {
        self.stop();
        let ctx = context();
        let _ = ctx.resume();
        self.playing = true;
        self.rate = rate;
        self.origin_frame = frame;
        self.origin_time = None;
        self.next_frame = frame;
        self.end_frame = end;
    }

    pub fn stop(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.playing = false;
        self.in_flight = false;
        for (_, s) in self.sources.drain(..) {
            #[allow(deprecated)]
            let _ = s.stop();
        }
    }

    /// Where playback is now, in timeline frames.
    pub fn position(&self) -> u64 {
        match self.origin_time {
            Some(t0) => {
                let dt = (context().current_time() - t0).max(0.0);
                (self.origin_frame + (dt * self.rate as f64) as u64).min(self.end_frame)
            }
            None => self.origin_frame,
        }
    }

    pub fn finished(&self) -> bool {
        self.playing && self.origin_time.is_some() && self.position() >= self.end_frame
    }

    /// The next chunk to render, if the schedule is running low.
    pub fn want_chunk(&mut self) -> Option<ChunkRequest> {
        if !self.playing || self.in_flight || self.next_frame >= self.end_frame {
            return None;
        }
        if let Some(t0) = self.origin_time {
            let scheduled_until = t0 + (self.next_frame - self.origin_frame) as f64 / self.rate as f64;
            if scheduled_until - context().current_time() > LOOKAHEAD {
                return None;
            }
        }
        let len = (CHUNK_SECS * self.rate as u64).min(self.end_frame - self.next_frame);
        let req = ChunkRequest { generation: self.generation, start: self.next_frame, len };
        self.next_frame += len;
        self.in_flight = true;
        Some(req)
    }

    /// Schedules a rendered chunk (interleaved, `ch` channels).
    pub fn deliver(&mut self, generation: u32, start: u64, samples: Vec<f32>, ch: usize) {
        if generation != self.generation || !self.playing {
            return;
        }
        self.in_flight = false;
        let ctx = context();
        let now = ctx.current_time();
        let t0 = *self.origin_time.get_or_insert(now + 0.05);
        let frames = samples.len() / ch;
        if frames == 0 {
            return;
        }
        let Ok(buf) = ctx.create_buffer(ch as u32, frames as u32, self.rate as f32) else { return };
        let mut plane = vec![0f32; frames];
        for c in 0..ch {
            for (i, p) in plane.iter_mut().enumerate() {
                *p = samples[i * ch + c];
            }
            let _ = buf.copy_to_channel(&plane, c as i32);
        }
        let Ok(src) = ctx.create_buffer_source() else { return };
        src.set_buffer(Some(&buf));
        let _ = src.connect_with_audio_node(&ctx.destination());
        let when = t0 + (start - self.origin_frame) as f64 / self.rate as f64;
        let end = when + frames as f64 / self.rate as f64;
        if when >= now {
            let _ = src.start_with_when(when);
        } else if end > now {
            // Arrived late (slow network): play what's left of it.
            let _ = src.start_with_when_and_grain_offset(now, now - when);
        }
        self.sources.retain(|(e, _)| *e > now);
        self.sources.push((end, src));
    }

    /// A chunk failed to render; retry it on the next frame.
    pub fn failed(&mut self, generation: u32, start: u64) {
        if generation == self.generation {
            self.in_flight = false;
            self.next_frame = self.next_frame.min(start);
        }
    }
}
