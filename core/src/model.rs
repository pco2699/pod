use serde::{Deserialize, Serialize};

use crate::wav::WavFormat;

/// How many edits can be undone.
const UNDO_LIMIT: usize = 200;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Project {
    /// Set by the first track; every later track must match it.
    pub sample_rate: Option<u32>,
    pub tracks: Vec<Track>,
    #[serde(default)]
    pub undo: Vec<Vec<Track>>,
    #[serde(default)]
    pub redo: Vec<Vec<Track>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub id: String,
    pub name: String,
    pub source: Source,
    /// Played back to back from timeline frame 0.
    pub clips: Vec<Clip>,
}

/// The uploaded audio behind a track, addressed by content hash (sha256 hex).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Source {
    pub wav: String,
    pub peaks: String,
    pub format: WavFormat,
}

/// A run of source frames `[src, src + len)`; a muted clip plays as silence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Clip {
    pub src: u64,
    pub len: u64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub muted: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    AddTrack { id: String, name: String, source: Source },
    RenameTrack { id: String, name: String },
    RemoveTrack { id: String },
    /// Mute `[start, end)` on the given tracks; the timeline keeps its length.
    Silence { tracks: Vec<String>, start: u64, end: u64 },
    /// Remove `[start, end)` from the given tracks; what follows moves left.
    Cut { tracks: Vec<String>, start: u64, end: u64 },
    Undo,
    Redo,
}

/// A stretch of a track's timeline resolved to its source: `len` frames starting
/// at timeline frame `at`, read from source frame `src`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Piece {
    pub at: u64,
    pub src: u64,
    pub len: u64,
    pub muted: bool,
}

impl Track {
    pub fn len(&self) -> u64 {
        self.clips.iter().map(|c| c.len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The pieces covering timeline `[start, end)`, clipped to that range.
    pub fn pieces(&self, start: u64, end: u64) -> Vec<Piece> {
        let mut out = Vec::new();
        let mut at = 0u64;
        for c in &self.clips {
            let (c0, c1) = (at, at + c.len);
            at = c1;
            if c1 <= start {
                continue;
            }
            if c0 >= end {
                break;
            }
            let s = start.max(c0);
            let e = end.min(c1);
            out.push(Piece { at: s, src: c.src + (s - c0), len: e - s, muted: c.muted });
        }
        out
    }

    /// Splits so a clip boundary falls at timeline frame `t`; returns the index of
    /// the first clip starting at or after `t`.
    fn split_at(&mut self, t: u64) -> usize {
        let mut at = 0u64;
        for i in 0..self.clips.len() {
            let c = self.clips[i];
            if t <= at {
                return i;
            }
            if t < at + c.len {
                let head = t - at;
                self.clips[i].len = head;
                self.clips.insert(i + 1, Clip { src: c.src + head, len: c.len - head, muted: c.muted });
                return i + 1;
            }
            at += c.len;
        }
        self.clips.len()
    }

    /// Merges neighbours that continue each other in the source with the same
    /// mute state, so repeated edits don't grow the clip list without bound.
    fn normalize(&mut self) {
        let mut out: Vec<Clip> = Vec::with_capacity(self.clips.len());
        for c in self.clips.drain(..) {
            if c.len == 0 {
                continue;
            }
            match out.last_mut() {
                Some(p) if p.muted == c.muted && p.src + p.len == c.src => p.len += c.len,
                _ => out.push(c),
            }
        }
        self.clips = out;
    }

    fn silence(&mut self, start: u64, end: u64) {
        let (a, b) = (self.split_at(start), self.split_at(end));
        for c in &mut self.clips[a..b] {
            c.muted = true;
        }
        self.normalize();
    }

    fn cut(&mut self, start: u64, end: u64) {
        let (a, b) = (self.split_at(start), self.split_at(end));
        self.clips.drain(a..b);
        self.normalize();
    }
}

impl Project {
    /// Length of the longest track.
    pub fn len(&self) -> u64 {
        self.tracks.iter().map(Track::len).max().unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn track(&self, id: &str) -> Option<&Track> {
        self.tracks.iter().find(|t| t.id == id)
    }

    /// Applies one op. On error the project is left unchanged.
    pub fn apply(&mut self, op: &Op) -> Result<(), String> {
        match op {
            Op::Undo => {
                let prev = self.undo.pop().ok_or("nothing to undo")?;
                self.redo.push(std::mem::replace(&mut self.tracks, prev));
                Ok(())
            }
            Op::Redo => {
                let next = self.redo.pop().ok_or("nothing to redo")?;
                self.undo.push(std::mem::replace(&mut self.tracks, next));
                Ok(())
            }
            _ => {
                let mut tracks = self.tracks.clone();
                let mut rate = self.sample_rate;
                edit(&mut tracks, &mut rate, op)?;
                self.undo.push(std::mem::replace(&mut self.tracks, tracks));
                if self.undo.len() > UNDO_LIMIT {
                    self.undo.remove(0);
                }
                self.redo.clear();
                self.sample_rate = rate;
                Ok(())
            }
        }
    }
}

fn edit(tracks: &mut Vec<Track>, rate: &mut Option<u32>, op: &Op) -> Result<(), String> {
    let find = |tracks: &mut Vec<Track>, id: &str| -> Result<usize, String> {
        tracks.iter().position(|t| t.id == id).ok_or_else(|| format!("no track {id}"))
    };
    match op {
        Op::AddTrack { id, name, source } => {
            if tracks.iter().any(|t| &t.id == id) {
                return Err(format!("track {id} exists"));
            }
            let sr = source.format.sample_rate;
            match *rate {
                Some(r) if r != sr && !tracks.is_empty() => {
                    return Err(format!("sample rate {sr} Hz does not match the project's {r} Hz"));
                }
                _ => *rate = Some(sr),
            }
            let frames = source.format.frames;
            tracks.push(Track {
                id: id.clone(),
                name: name.clone(),
                source: source.clone(),
                clips: vec![Clip { src: 0, len: frames, muted: false }],
            });
        }
        Op::RenameTrack { id, name } => {
            let i = find(tracks, id)?;
            tracks[i].name = name.clone();
        }
        Op::RemoveTrack { id } => {
            let i = find(tracks, id)?;
            tracks.remove(i);
        }
        Op::Silence { tracks: ids, start, end } | Op::Cut { tracks: ids, start, end } => {
            if start >= end {
                return Err("empty range".into());
            }
            if ids.is_empty() {
                return Err("no tracks selected".into());
            }
            for id in ids {
                let i = find(tracks, id)?;
                if matches!(op, Op::Cut { .. }) {
                    tracks[i].cut(*start, *end);
                } else {
                    tracks[i].silence(*start, *end);
                }
            }
        }
        Op::Undo | Op::Redo => unreachable!(),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(frames: u64) -> Source {
        Source {
            wav: "w".into(),
            peaks: "p".into(),
            format: WavFormat {
                sample_rate: 48000,
                channels: 1,
                bits: 16,
                float: false,
                data_offset: 44,
                frames,
            },
        }
    }

    fn project() -> Project {
        let mut p = Project::default();
        p.apply(&Op::AddTrack { id: "a".into(), name: "Host".into(), source: src(1000) }).unwrap();
        p.apply(&Op::AddTrack { id: "b".into(), name: "Guest".into(), source: src(1000) }).unwrap();
        p
    }

    #[test]
    fn silence_splits_and_mutes() {
        let mut p = project();
        p.apply(&Op::Silence { tracks: vec!["a".into()], start: 100, end: 300 }).unwrap();
        let a = p.track("a").unwrap();
        assert_eq!(
            a.clips,
            vec![
                Clip { src: 0, len: 100, muted: false },
                Clip { src: 100, len: 200, muted: true },
                Clip { src: 300, len: 700, muted: false },
            ]
        );
        assert_eq!(a.len(), 1000);
        assert_eq!(p.track("b").unwrap().clips.len(), 1);
    }

    #[test]
    fn unsilencing_neighbours_merge() {
        let mut p = project();
        p.apply(&Op::Silence { tracks: vec!["a".into()], start: 100, end: 200 }).unwrap();
        p.apply(&Op::Silence { tracks: vec!["a".into()], start: 200, end: 300 }).unwrap();
        assert_eq!(p.track("a").unwrap().clips[1], Clip { src: 100, len: 200, muted: true });
    }

    #[test]
    fn cut_removes_and_shifts() {
        let mut p = project();
        p.apply(&Op::Cut { tracks: vec!["a".into(), "b".into()], start: 100, end: 300 }).unwrap();
        let a = p.track("a").unwrap();
        assert_eq!(a.len(), 800);
        assert_eq!(a.clips, vec![Clip { src: 0, len: 100, muted: false }, Clip { src: 300, len: 700, muted: false }]);
        assert_eq!(
            a.pieces(50, 150),
            vec![
                Piece { at: 50, src: 50, len: 50, muted: false },
                Piece { at: 100, src: 300, len: 50, muted: false },
            ]
        );
    }

    #[test]
    fn cut_past_end_is_clamped() {
        let mut p = project();
        p.apply(&Op::Cut { tracks: vec!["a".into()], start: 900, end: 5000 }).unwrap();
        assert_eq!(p.track("a").unwrap().len(), 900);
        assert_eq!(p.len(), 1000);
    }

    #[test]
    fn undo_redo() {
        let mut p = project();
        let before = p.tracks.clone();
        p.apply(&Op::Cut { tracks: vec!["a".into()], start: 0, end: 10 }).unwrap();
        let after = p.tracks.clone();
        p.apply(&Op::Undo).unwrap();
        assert_eq!(p.tracks, before);
        p.apply(&Op::Redo).unwrap();
        assert_eq!(p.tracks, after);
        assert!(p.apply(&Op::Redo).is_err());
    }

    #[test]
    fn rejects_mismatched_rate_and_bad_ops() {
        let mut p = project();
        let mut s = src(10);
        s.format.sample_rate = 44100;
        assert!(p.apply(&Op::AddTrack { id: "c".into(), name: "x".into(), source: s }).is_err());
        assert!(p.apply(&Op::Cut { tracks: vec!["zz".into()], start: 0, end: 10 }).is_err());
        assert!(p.apply(&Op::Silence { tracks: vec!["a".into()], start: 10, end: 10 }).is_err());
        assert_eq!(p.undo.len(), 2);
    }

    #[test]
    fn ops_roundtrip_json() {
        let op = Op::Silence { tracks: vec!["a".into()], start: 1, end: 2 };
        let s = serde_json::to_string(&op).unwrap();
        assert_eq!(s, r#"{"op":"silence","tracks":["a"],"start":1,"end":2}"#);
        assert_eq!(serde_json::from_str::<Op>(&s).unwrap(), op);
    }
}
