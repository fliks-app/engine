//! The pure half of `Separate`: the STFT, soft masks and overlap-add around a two-stem mask
//! network (Spleeter's 2-stem U-Net). The network itself runs in the engine. Spec:
//! `docs/spec/karaoke.md` section 3.
use super::merge::{absolute, plain};
use crate::api::JobKind;

/// Sample rate the network was trained on.
pub const SAMPLE_RATE: u32 = 44_100;
pub const N_FFT: usize = 4096;
pub const HOP: usize = 1024;
/// One-sided bins of an `N_FFT` frame.
pub const BINS: usize = N_FFT / 2 + 1;
/// Bins the network masks; the ones above (about 11 kHz) it never sees.
pub const MODEL_BINS: usize = 1024;
/// Frames the network takes at once.
pub const CHUNK_FRAMES: usize = 512;
/// Samples of one chunk's output that are final once it is done.
pub const CHUNK_SAMPLES: usize = CHUNK_FRAMES * HOP;
/// Zeros in front of the signal so its first samples sit under the whole window overlap.
pub const LEAD: usize = N_FFT - HOP;
/// Samples of a chunk's frames from its first frame's start to its last frame's end.
pub const CHUNK_SPAN: usize = (CHUNK_FRAMES - 1) * HOP + N_FFT;
const EPS: f32 = 1e-10;
pub const DEFAULT_GUIDE_VOCALS_GAIN: f64 = 0.25;
pub const DEFAULT_BITRATE_KBPS: u32 = 96;

pub fn validate(kind: &JobKind) -> Result<(), String> {
    let JobKind::Separate {
        vocals,
        instrumental,
        guide,
        guide_vocals_gain,
        bitrate_kbps,
        ..
    } = kind
    else {
        return Ok(());
    };
    let outs = [vocals, instrumental, guide];
    if outs.iter().all(|o| o.is_none()) {
        return Err("Separate needs at least one of vocals, instrumental or guide".into());
    }
    let named: Vec<&str> = outs.iter().filter_map(|o| o.as_deref()).collect();
    if let Some(bad) = named.iter().find(|p| !plain(p) || !absolute(p)) {
        return Err(format!(
            "output must be an absolute path without control characters: {bad}"
        ));
    }
    if named
        .iter()
        .enumerate()
        .any(|(i, a)| named[..i].contains(a))
    {
        return Err("Separate outputs must be distinct paths".into());
    }
    if guide_vocals_gain.is_some_and(|g| !(0.0..=1.0).contains(&g)) {
        return Err("guideVocalsGain must be within 0 and 1".into());
    }
    if bitrate_kbps.is_some_and(|b| !(16..=512).contains(&b)) {
        return Err("bitrateKbps must be within 16 and 512".into());
    }
    Ok(())
}

/// In-place radix-2 complex FFT of one size.
pub struct Fft {
    n: usize,
    rev: Vec<u32>,
    cos: Vec<f32>,
    sin: Vec<f32>,
}

impl Fft {
    pub fn new(n: usize) -> Self {
        assert!(n.is_power_of_two());
        let bits = n.trailing_zeros();
        let rev = (0..n as u32)
            .map(|i| i.reverse_bits() >> (32 - bits))
            .collect();
        let (cos, sin) = (0..n / 2)
            .map(|k| {
                let a = -2.0 * std::f64::consts::PI * k as f64 / n as f64;
                (a.cos() as f32, a.sin() as f32)
            })
            .unzip();
        Self { n, rev, cos, sin }
    }

    pub fn forward(&self, re: &mut [f32], im: &mut [f32]) {
        let n = self.n;
        for i in 0..n {
            let j = self.rev[i] as usize;
            if j > i {
                re.swap(i, j);
                im.swap(i, j);
            }
        }
        let mut len = 2;
        while len <= n {
            let (half, step) = (len / 2, n / len);
            for start in (0..n).step_by(len) {
                for k in 0..half {
                    let (c, s) = (self.cos[k * step], self.sin[k * step]);
                    let (a, b) = (start + k, start + k + half);
                    let tr = re[b] * c - im[b] * s;
                    let ti = re[b] * s + im[b] * c;
                    re[b] = re[a] - tr;
                    im[b] = im[a] - ti;
                    re[a] += tr;
                    im[a] += ti;
                }
            }
            len *= 2;
        }
    }

    /// The inverse, scaled by `1/n`.
    pub fn inverse(&self, re: &mut [f32], im: &mut [f32]) {
        im.iter_mut().for_each(|v| *v = -*v);
        self.forward(re, im);
        let k = 1.0 / self.n as f32;
        re.iter_mut().for_each(|v| *v *= k);
        im.iter_mut().for_each(|v| *v *= -k);
    }
}

/// Spectra of one chunk: `[channel][frame][bin]`, `BINS` bins per frame.
pub struct Spectra {
    pub re: Vec<f32>,
    pub im: Vec<f32>,
}

impl Spectra {
    fn at(ch: usize, frame: usize, bin: usize) -> usize {
        (ch * CHUNK_FRAMES + frame) * BINS + bin
    }

    /// The network's input: `[2][CHUNK_FRAMES][MODEL_BINS]` magnitudes.
    pub fn magnitudes(&self) -> Vec<f32> {
        let mut m = Vec::with_capacity(2 * CHUNK_FRAMES * MODEL_BINS);
        for ch in 0..2 {
            for f in 0..CHUNK_FRAMES {
                let base = Self::at(ch, f, 0);
                m.extend((0..MODEL_BINS).map(|k| self.re[base + k].hypot(self.im[base + k])));
            }
        }
        m
    }
}

/// Soft masks of the two stems from the network's outputs (`[2][CHUNK_FRAMES][MODEL_BINS]`
/// each): the share of the energy each stem holds, so the two sum to 1.
pub fn soft_masks(vocals: &[f32], accompaniment: &[f32]) -> (Vec<f32>, Vec<f32>) {
    let (mut v, mut a) = (vec![0.0; vocals.len()], vec![0.0; vocals.len()]);
    for i in 0..vocals.len() {
        let (pv, pa) = (vocals[i] * vocals[i], accompaniment[i] * accompaniment[i]);
        let sum = pv + pa + EPS;
        v[i] = (pv + EPS / 2.0) / sum;
        a[i] = (pa + EPS / 2.0) / sum;
    }
    (v, a)
}

/// Overlap-add buffers of the stems and of the window gain, one chunk long plus the tail the
/// next chunk completes.
pub struct Overlap {
    vocals: [Vec<f32>; 2],
    instrumental: [Vec<f32>; 2],
    gain: Vec<f32>,
}

/// The final samples of one chunk, per channel.
pub struct Done {
    pub vocals: [Vec<f32>; 2],
    pub instrumental: [Vec<f32>; 2],
}

impl Default for Overlap {
    fn default() -> Self {
        let len = CHUNK_SAMPLES + LEAD;
        Self {
            vocals: [vec![0.0; len], vec![0.0; len]],
            instrumental: [vec![0.0; len], vec![0.0; len]],
            gain: vec![0.0; len],
        }
    }
}

impl Overlap {
    /// The chunk's final samples, normalised by the window gain; the rest moves to the front.
    pub fn finish(&mut self) -> Done {
        let gain = &self.gain;
        let norm = |buf: &Vec<f32>| -> Vec<f32> {
            (0..CHUNK_SAMPLES)
                .map(|i| {
                    if gain[i] > 1e-3 {
                        buf[i] / gain[i]
                    } else {
                        0.0
                    }
                })
                .collect()
        };
        let done = Done {
            vocals: [norm(&self.vocals[0]), norm(&self.vocals[1])],
            instrumental: [norm(&self.instrumental[0]), norm(&self.instrumental[1])],
        };
        for buf in self
            .vocals
            .iter_mut()
            .chain(self.instrumental.iter_mut())
            .chain(std::iter::once(&mut self.gain))
        {
            buf.copy_within(CHUNK_SAMPLES.., 0);
            buf[LEAD..].fill(0.0);
        }
        done
    }
}

/// Periodic Hann window, as the network's training used.
fn hann() -> Vec<f32> {
    (0..N_FFT)
        .map(|i| (0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / N_FFT as f64).cos()) as f32)
        .collect()
}

/// Analysis and synthesis of the chunks of one track.
pub struct Stft {
    fft: Fft,
    window: Vec<f32>,
}

impl Default for Stft {
    fn default() -> Self {
        Self {
            fft: Fft::new(N_FFT),
            window: hann(),
        }
    }
}

impl Stft {
    /// Spectra of the chunk whose first frame starts at `pcm[0]`; `pcm` holds both channels,
    /// short ones are zero-extended.
    pub fn analyse(&self, pcm: [&[f32]; 2]) -> Spectra {
        let n = 2 * CHUNK_FRAMES * BINS;
        let mut s = Spectra {
            re: vec![0.0; n],
            im: vec![0.0; n],
        };
        let (mut re, mut im) = (vec![0.0f32; N_FFT], vec![0.0f32; N_FFT]);
        let at = |c: &[f32], i: usize| c.get(i).copied().unwrap_or(0.0);
        for f in 0..CHUNK_FRAMES {
            let p0 = f * HOP;
            // Both channels in one transform: the left is the real part, the right the imaginary.
            for i in 0..N_FFT {
                re[i] = self.window[i] * at(pcm[0], p0 + i);
                im[i] = self.window[i] * at(pcm[1], p0 + i);
            }
            self.fft.forward(&mut re, &mut im);
            for k in 0..BINS {
                let m = (N_FFT - k) % N_FFT;
                let (zr, zi, mr, mi) = (re[k], im[k], re[m], im[m]);
                let a = Spectra::at(0, f, k);
                s.re[a] = (zr + mr) * 0.5;
                s.im[a] = (zi - mi) * 0.5;
                let b = Spectra::at(1, f, k);
                s.re[b] = (zi + mi) * 0.5;
                s.im[b] = -(zr - mr) * 0.5;
            }
        }
        s
    }

    /// Adds the stems of the chunk into `out`: the vocals are the network's vocal mask, the
    /// instrumental its accompaniment mask, and above `MODEL_BINS` all of it is instrumental.
    pub fn synthesise(&self, s: &Spectra, vocal_mask: &[f32], acc_mask: &[f32], out: &mut Overlap) {
        let (mut re, mut im) = (vec![0.0f32; N_FFT], vec![0.0f32; N_FFT]);
        for f in 0..CHUNK_FRAMES {
            for (vocal, mask, bufs) in [
                (true, vocal_mask, &mut out.vocals),
                (false, acc_mask, &mut out.instrumental),
            ] {
                let bin = |ch: usize, k: usize| {
                    let a = Spectra::at(ch, f, k);
                    let m = if k < MODEL_BINS {
                        mask[(ch * CHUNK_FRAMES + f) * MODEL_BINS + k]
                    } else if vocal {
                        0.0
                    } else {
                        1.0
                    };
                    (s.re[a] * m, s.im[a] * m)
                };
                for k in 0..N_FFT {
                    // Bins above Nyquist are the conjugates of the ones below.
                    let (j, conj) = if k < BINS {
                        (k, false)
                    } else {
                        (N_FFT - k, true)
                    };
                    let (l, r) = (bin(0, j), bin(1, j));
                    let (lr, li) = (l.0, if conj { -l.1 } else { l.1 });
                    let (rr, ri) = (r.0, if conj { -r.1 } else { r.1 });
                    // Z = L + iR: one inverse transform gives both channels.
                    re[k] = lr - ri;
                    im[k] = li + rr;
                }
                self.fft.inverse(&mut re, &mut im);
                let p0 = f * HOP;
                for i in 0..N_FFT {
                    bufs[0][p0 + i] += self.window[i] * re[i];
                    bufs[1][p0 + i] += self.window[i] * im[i];
                }
            }
            let p0 = f * HOP;
            for i in 0..N_FFT {
                out.gain[p0 + i] += self.window[i] * self.window[i];
            }
        }
    }
}

/// Chunks a track of `samples` samples needs.
pub fn chunk_count(samples: usize) -> usize {
    (samples + LEAD).div_ceil(CHUNK_SAMPLES).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(freq: f64, n: usize, amp: f32) -> Vec<f32> {
        (0..n)
            .map(|i| {
                amp * (2.0 * std::f64::consts::PI * freq * i as f64 / f64::from(SAMPLE_RATE)).sin()
                    as f32
            })
            .collect()
    }

    #[test]
    fn the_fft_of_a_tone_peaks_at_its_bin() {
        let fft = Fft::new(64);
        let mut re: Vec<f32> = (0..64)
            .map(|i| (2.0 * std::f32::consts::PI * 5.0 * i as f32 / 64.0).cos())
            .collect();
        let mut im = vec![0.0; 64];
        fft.forward(&mut re, &mut im);
        let mag: Vec<f32> = re.iter().zip(&im).map(|(r, i)| r.hypot(*i)).collect();
        assert!((mag[5] - 32.0).abs() < 1e-3 && (mag[59] - 32.0).abs() < 1e-3);
        assert!(mag[6] < 1e-3);
        fft.inverse(&mut re, &mut im);
        assert!((re[1] - (2.0 * std::f32::consts::PI * 5.0 / 64.0).cos()).abs() < 1e-5);
    }

    /// Splits at bin `cut`: below goes to the accompaniment, above to the vocals.
    fn split_masks(mags: &[f32], cut: usize) -> (Vec<f32>, Vec<f32>) {
        let (mut v, mut a) = (mags.to_vec(), mags.to_vec());
        for i in 0..mags.len() {
            if i % MODEL_BINS < cut {
                v[i] = 0.0;
            } else {
                a[i] = 0.0;
            }
        }
        soft_masks(&v, &a)
    }

    fn rms(x: &[f32]) -> f64 {
        (x.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
    }

    #[test]
    fn two_tones_split_by_frequency_and_sum_back_to_the_mix() {
        let n = CHUNK_SAMPLES + 5000;
        let (low, high) = (tone(172.265625, n, 0.3), tone(2756.25, n, 0.3));
        let mix: Vec<f32> = low.iter().zip(&high).map(|(a, b)| a + b).collect();
        // The signal sits at `LEAD` in the padded coordinates the chunks are cut from.
        let mut padded = vec![0.0f32; LEAD];
        padded.extend(&mix);
        let stft = Stft::default();
        let mut out = Overlap::default();
        let (mut vocals, mut inst): (Vec<f32>, Vec<f32>) = (vec![], vec![]);
        for k in 0..chunk_count(n) {
            let from = (k * CHUNK_SAMPLES).min(padded.len());
            let s = stft.analyse([&padded[from..], &padded[from..]]);
            let (vm, am) = split_masks(&s.magnitudes(), 30);
            stft.synthesise(&s, &vm, &am, &mut out);
            let d = out.finish();
            vocals.extend(d.vocals[0].iter());
            inst.extend(d.instrumental[0].iter());
        }
        let (vocals, inst) = (&vocals[LEAD..LEAD + n], &inst[LEAD..LEAD + n]);
        // Skip the ends, where the window overlap is partial.
        let mid = 4096..n - 4096;
        let err_mix: Vec<f32> = mid.clone().map(|i| vocals[i] + inst[i] - mix[i]).collect();
        assert!(rms(&err_mix) < 0.01, "mix error {}", rms(&err_mix));
        let err_high: Vec<f32> = mid.clone().map(|i| vocals[i] - high[i]).collect();
        let err_low: Vec<f32> = mid.map(|i| inst[i] - low[i]).collect();
        assert!(rms(&err_high) < 0.02, "vocals error {}", rms(&err_high));
        assert!(rms(&err_low) < 0.02, "instrumental error {}", rms(&err_low));
    }

    #[test]
    fn the_right_channel_is_kept_apart() {
        let stft = Stft::default();
        let n = CHUNK_SAMPLES;
        let right = tone(2756.25, n, 0.5);
        let silent = vec![0.0f32; n];
        let s = stft.analyse([&silent, &right]);
        let m = s.magnitudes();
        let ch0: f32 = m[..CHUNK_FRAMES * MODEL_BINS].iter().sum();
        let ch1: f32 = m[CHUNK_FRAMES * MODEL_BINS..].iter().sum();
        assert!(ch0 < 1e-2 && ch1 > 100.0, "{ch0} {ch1}");
    }

    #[test]
    fn soft_masks_share_the_energy() {
        let (v, a) = soft_masks(&[3.0, 0.0], &[4.0, 0.0]);
        assert!((v[0] - 0.36).abs() < 1e-6 && (a[0] - 0.64).abs() < 1e-6);
        assert!((v[1] - 0.5).abs() < 1e-3 && (v[1] + a[1] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn the_request_names_outputs_that_are_absolute_and_distinct() {
        let kind = |extra: serde_json::Value| -> JobKind {
            let mut k = serde_json::json!({ "type": "Separate",
                "info": { "video": [], "audio": [], "subtitles": [] } });
            k.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            serde_json::from_value(k).unwrap()
        };
        assert!(validate(&kind(serde_json::json!({}))).is_err());
        assert!(validate(&kind(serde_json::json!({ "vocals": "rel.opus" }))).is_err());
        assert!(
            validate(&kind(
                serde_json::json!({ "vocals": "/a.opus", "guide": "/a.opus" })
            ))
            .is_err()
        );
        assert!(
            validate(&kind(
                serde_json::json!({ "vocals": "/a.opus", "guideVocalsGain": 2 })
            ))
            .is_err()
        );
        assert!(
            validate(&kind(
                serde_json::json!({ "vocals": "/a.opus", "guide": "/g.opus" })
            ))
            .is_ok()
        );
    }
}
