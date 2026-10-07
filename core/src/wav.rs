use serde::{Deserialize, Serialize};

/// Where the PCM lives inside a WAV file and how to decode it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WavFormat {
    pub sample_rate: u32,
    pub channels: u16,
    /// Bits per sample: 8/16/24/32 integer, or 32/64 when `float`.
    pub bits: u16,
    pub float: bool,
    /// Byte offset of the first sample.
    pub data_offset: u64,
    pub frames: u64,
}

impl WavFormat {
    pub fn bytes_per_frame(&self) -> u64 {
        self.channels as u64 * (self.bits as u64 / 8)
    }

    /// Byte range in the file holding source frames `[frame, frame + n)`.
    pub fn byte_range(&self, frame: u64, n: u64) -> (u64, u64) {
        let start = self.data_offset + frame * self.bytes_per_frame();
        (start, start + n * self.bytes_per_frame())
    }
}

fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}

fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

/// Parses a WAV header. `head` is the start of the file (it must reach the `data`
/// chunk; 64 KiB is plenty for real recorders) and `file_size` its total length,
/// used when a recorder left the data size unset.
pub fn parse_header(head: &[u8], file_size: u64) -> Result<WavFormat, String> {
    if head.len() < 12 || &head[0..4] != b"RIFF" || &head[8..12] != b"WAVE" {
        return Err("not a WAV file".into());
    }
    let mut fmt: Option<(u16, u16, u32, u16)> = None;
    let mut i = 12usize;
    while i + 8 <= head.len() {
        let id = &head[i..i + 4];
        let size = u32_at(head, i + 4) as usize;
        let body = i + 8;
        if id == b"fmt " {
            if body + 16 > head.len() {
                break;
            }
            let mut tag = u16_at(head, body);
            let channels = u16_at(head, body + 2);
            let rate = u32_at(head, body + 4);
            let bits = u16_at(head, body + 14);
            if tag == 0xFFFE && size >= 40 && body + 26 <= head.len() {
                // WAVE_FORMAT_EXTENSIBLE: the real format is the subformat GUID's first two bytes.
                tag = u16_at(head, body + 24);
            }
            fmt = Some((tag, channels, rate, bits));
        } else if id == b"data" {
            let (tag, channels, sample_rate, bits) = fmt.ok_or("data chunk before fmt chunk")?;
            let float = match (tag, bits) {
                (1, 8 | 16 | 24 | 32) => false,
                (3, 32 | 64) => true,
                _ => return Err(format!("unsupported WAV encoding (format {tag}, {bits}-bit)")),
            };
            if channels == 0 || sample_rate == 0 {
                return Err("invalid WAV header".into());
            }
            let available = file_size.saturating_sub(body as u64);
            let declared = size as u64;
            let data_len = if declared == 0 || declared == 0xFFFF_FFFF || declared > available {
                available
            } else {
                declared
            };
            let bpf = channels as u64 * (bits as u64 / 8);
            return Ok(WavFormat {
                sample_rate,
                channels,
                bits,
                float,
                data_offset: body as u64,
                frames: data_len / bpf,
            });
        }
        i = body + size + (size & 1);
    }
    Err("no data chunk in the first part of the file".into())
}

/// Decodes whole frames from `bytes` (which must start on a frame boundary) and
/// appends them to `out` as interleaved f32 in [-1, 1].
pub fn decode(bytes: &[u8], fmt: &WavFormat, out: &mut Vec<f32>) {
    let bps = (fmt.bits / 8) as usize;
    let usable = bytes.len() - bytes.len() % (bps * fmt.channels as usize);
    let b = &bytes[..usable];
    out.reserve(usable / bps);
    match (fmt.float, fmt.bits) {
        (false, 8) => out.extend(b.iter().map(|&s| (s as f32 - 128.0) / 128.0)),
        (false, 16) => out.extend(b.chunks_exact(2).map(|s| i16::from_le_bytes([s[0], s[1]]) as f32 / 32768.0)),
        (false, 24) => out.extend(
            b.chunks_exact(3)
                .map(|s| (i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8) as f32 / 8_388_608.0),
        ),
        (false, 32) => out.extend(
            b.chunks_exact(4)
                .map(|s| i32::from_le_bytes([s[0], s[1], s[2], s[3]]) as f32 / 2_147_483_648.0),
        ),
        (true, 32) => out.extend(b.chunks_exact(4).map(|s| f32::from_le_bytes([s[0], s[1], s[2], s[3]]))),
        (true, 64) => out.extend(b.chunks_exact(8).map(|s| {
            f64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]) as f32
        })),
        _ => {}
    }
}

/// The 44-byte header of a 24-bit PCM WAV holding `frames` frames.
pub fn header_pcm24(sample_rate: u32, channels: u16, frames: u64) -> Vec<u8> {
    let block = channels as u32 * 3;
    let data = (frames * block as u64).min(u32::MAX as u64 - 36) as u32;
    let mut h = Vec::with_capacity(44);
    h.extend_from_slice(b"RIFF");
    h.extend_from_slice(&(36 + data).to_le_bytes());
    h.extend_from_slice(b"WAVEfmt ");
    h.extend_from_slice(&16u32.to_le_bytes());
    h.extend_from_slice(&1u16.to_le_bytes());
    h.extend_from_slice(&channels.to_le_bytes());
    h.extend_from_slice(&sample_rate.to_le_bytes());
    h.extend_from_slice(&(sample_rate * block).to_le_bytes());
    h.extend_from_slice(&(block as u16).to_le_bytes());
    h.extend_from_slice(&24u16.to_le_bytes());
    h.extend_from_slice(b"data");
    h.extend_from_slice(&data.to_le_bytes());
    h
}

/// Encodes interleaved f32 samples as 24-bit PCM, clamping overs.
pub fn encode_pcm24(samples: &[f32], out: &mut Vec<u8>) {
    out.reserve(samples.len() * 3);
    for &s in samples {
        let v = (s.clamp(-1.0, 1.0) * 8_388_607.0).round() as i32;
        out.extend_from_slice(&v.to_le_bytes()[..3]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_pcm24() {
        let samples = [0.0f32, 0.5, -0.5, 1.0, -1.0, 0.25];
        let mut file = header_pcm24(48000, 2, 3);
        encode_pcm24(&samples, &mut file);
        let fmt = parse_header(&file, file.len() as u64).unwrap();
        assert_eq!(
            fmt,
            WavFormat { sample_rate: 48000, channels: 2, bits: 24, float: false, data_offset: 44, frames: 3 }
        );
        let mut out = Vec::new();
        decode(&file[44..], &fmt, &mut out);
        for (a, b) in samples.iter().zip(&out) {
            assert!((a - b).abs() < 1e-6, "{a} vs {b}");
        }
    }

    #[test]
    fn skips_extra_chunks_and_handles_unset_size() {
        let mut f = Vec::new();
        f.extend_from_slice(b"RIFF\0\0\0\0WAVE");
        f.extend_from_slice(b"JUNK");
        f.extend_from_slice(&3u32.to_le_bytes());
        f.extend_from_slice(&[0, 0, 0, 0]); // 3 bytes + pad byte
        f.extend_from_slice(b"fmt ");
        f.extend_from_slice(&16u32.to_le_bytes());
        f.extend_from_slice(&1u16.to_le_bytes());
        f.extend_from_slice(&1u16.to_le_bytes());
        f.extend_from_slice(&44100u32.to_le_bytes());
        f.extend_from_slice(&88200u32.to_le_bytes());
        f.extend_from_slice(&2u16.to_le_bytes());
        f.extend_from_slice(&16u16.to_le_bytes());
        f.extend_from_slice(b"data");
        f.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        let off = f.len() as u64;
        f.extend_from_slice(&[0u8; 200]);
        let fmt = parse_header(&f, f.len() as u64).unwrap();
        assert_eq!(fmt.data_offset, off);
        assert_eq!(fmt.frames, 100);
        assert_eq!(fmt.byte_range(10, 5), (off + 20, off + 30));
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_header(b"hello world, not audio", 22).is_err());
    }
}
