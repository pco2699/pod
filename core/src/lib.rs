//! Shared between the wasm client and the server: the edit model (a project is a
//! list of tracks, each a list of clips over one source WAV), the ops that change
//! it, WAV parsing/decoding/writing and the waveform-peaks format.
//!
//! Everything on the timeline is measured in sample frames at the project rate.

pub mod model;
pub mod peaks;
pub mod wav;

pub use model::{Clip, Op, Project, Source, Track};
pub use wav::WavFormat;
