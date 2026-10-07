//! Waveform overview: per bucket of `BUCKET` source frames, the min and max of the
//! channel-averaged signal as i16. About 1.3 MB per hour at 48 kHz, so a new device
//! can draw the whole waveform long before the audio itself has arrived.
//!
//! File layout: `b"PEAK"`, u32 bucket size, u64 bucket count, then (min, max) i16 pairs.

pub const BUCKET: u64 = 256;
/// Each coarser level merges this many buckets of the level below.
const FAN: usize = 4;

/// Feeds decoded audio in arbitrary-sized pieces and produces the peaks file.
pub struct PeakBuilder {
    channels: usize,
    pairs: Vec<(i16, i16)>,
    cur: (f32, f32),
    filled: u64,
}

impl PeakBuilder {
    pub fn new(channels: u16) -> Self {
        Self { channels: channels as usize, pairs: Vec::new(), cur: (f32::MAX, f32::MIN), filled: 0 }
    }

    /// `samples` is interleaved and holds whole frames.
    pub fn push(&mut self, samples: &[f32]) {
        let inv = 1.0 / self.channels as f32;
        for frame in samples.chunks_exact(self.channels) {
            let v = frame.iter().sum::<f32>() * inv;
            self.cur.0 = self.cur.0.min(v);
            self.cur.1 = self.cur.1.max(v);
            self.filled += 1;
            if self.filled == BUCKET {
                self.flush();
            }
        }
    }

    fn flush(&mut self) {
        let q = |v: f32| (v.clamp(-1.0, 1.0) * 32767.0) as i16;
        self.pairs.push((q(self.cur.0), q(self.cur.1)));
        self.cur = (f32::MAX, f32::MIN);
        self.filled = 0;
    }

    pub fn finish(mut self) -> Vec<u8> {
        if self.filled > 0 {
            self.flush();
        }
        let mut out = Vec::with_capacity(16 + self.pairs.len() * 4);
        out.extend_from_slice(b"PEAK");
        out.extend_from_slice(&(BUCKET as u32).to_le_bytes());
        out.extend_from_slice(&(self.pairs.len() as u64).to_le_bytes());
        for (lo, hi) in self.pairs {
            out.extend_from_slice(&lo.to_le_bytes());
            out.extend_from_slice(&hi.to_le_bytes());
        }
        out
    }
}

/// Loaded peaks with coarser levels so any zoom reads only a few values per pixel.
pub struct Peaks {
    /// `levels[k]` has buckets of `BUCKET * FAN^k` frames.
    levels: Vec<Vec<(i16, i16)>>,
}

impl Peaks {
    pub fn parse(b: &[u8]) -> Result<Self, String> {
        if b.len() < 16 || &b[0..4] != b"PEAK" {
            return Err("bad peaks file".into());
        }
        let bucket = u32::from_le_bytes(b[4..8].try_into().unwrap()) as u64;
        if bucket != BUCKET {
            return Err(format!("unsupported peaks bucket size {bucket}"));
        }
        let n = u64::from_le_bytes(b[8..16].try_into().unwrap()) as usize;
        if b.len() < 16 + n * 4 {
            return Err("truncated peaks file".into());
        }
        let base: Vec<(i16, i16)> = b[16..16 + n * 4]
            .chunks_exact(4)
            .map(|c| (i16::from_le_bytes([c[0], c[1]]), i16::from_le_bytes([c[2], c[3]])))
            .collect();
        let mut levels = vec![base];
        while levels.last().unwrap().len() > 1 {
            let next = levels
                .last()
                .unwrap()
                .chunks(FAN)
                .map(|g| g.iter().fold((i16::MAX, i16::MIN), |a, &(lo, hi)| (a.0.min(lo), a.1.max(hi))))
                .collect();
            levels.push(next);
        }
        Ok(Self { levels })
    }

    /// Min/max (in [-1, 1]) over source frames `[start, end)`, or `None` past the end.
    /// Picks the coarsest level whose buckets still fit the range, so resolution
    /// tracks the zoom.
    pub fn range(&self, start: u64, end: u64) -> Option<(f32, f32)> {
        let span = end.saturating_sub(start).max(1);
        let mut k = 0;
        while k + 1 < self.levels.len() && BUCKET * (FAN as u64).pow(k as u32 + 1) <= span {
            k += 1;
        }
        let size = BUCKET * (FAN as u64).pow(k as u32);
        let lvl = &self.levels[k];
        let a = (start / size) as usize;
        let b = (end.div_ceil(size) as usize).max(a + 1).min(lvl.len());
        if a >= b {
            return None;
        }
        let (lo, hi) = lvl[a..b].iter().fold((i16::MAX, i16::MIN), |m, &(lo, hi)| (m.0.min(lo), m.1.max(hi)));
        Some((lo as f32 / 32767.0, hi as f32 / 32767.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_and_queries() {
        let mut b = PeakBuilder::new(2);
        // 1000 stereo frames: a ramp on the left, silence on the right.
        let samples: Vec<f32> = (0..1000).flat_map(|i| [i as f32 / 1000.0, 0.0]).collect();
        b.push(&samples[..700]);
        b.push(&samples[700..]);
        let p = Peaks::parse(&b.finish()).unwrap();
        let (lo, hi) = p.range(0, 256).unwrap();
        assert!(lo.abs() < 0.001 && (hi - 0.1275).abs() < 0.001, "{lo} {hi}");
        let (_, hi) = p.range(0, 1000).unwrap();
        assert!((hi - 0.4995).abs() < 0.001, "{hi}");
        assert!(p.range(5000, 6000).is_none());
    }
}
