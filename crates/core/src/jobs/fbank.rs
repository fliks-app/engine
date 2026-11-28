//! The SeamlessM4T feature extractor (Kaldi-style fbank) of the w2v-BERT CTC model, as
//! `SeamlessM4TFeatureExtractor.__call__` computes it. Spec: `docs/spec/karaoke.md` section 4.
use super::separate::Fft;
use std::sync::OnceLock;

const FRAME: usize = 400;
const HOP: usize = 160;
const FFT: usize = 512;
const BINS: usize = FFT / 2 + 1;
pub const MELS: usize = 80;
/// Features of one network input frame: two fbank frames stacked.
pub const STACKED: usize = 2 * MELS;
const RATE: f64 = 16_000.0;
const PREEMPHASIS: f32 = 0.97;
const FLOOR: f32 = f32::EPSILON;
/// The feature extractor's `padding_value`, which fills the frame an odd count is padded with.
const PAD: f32 = 1.0;

fn mel(hz: f64) -> f64 {
    1127.0 * (1.0 + hz / 700.0).ln()
}

struct Tables {
    /// Povey window: Hann (symmetric) to the power 0.85.
    window: [f32; FRAME],
    /// Triangular filters in mel space, `[mel][bin]`.
    filters: Vec<[f32; BINS]>,
    fft: Fft,
}

fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| {
        let mut window = [0.0; FRAME];
        for (i, w) in window.iter_mut().enumerate() {
            let hann =
                0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / (FRAME - 1) as f64).cos();
            *w = hann.powf(0.85) as f32;
        }
        let (lo, hi) = (mel(20.0), mel(RATE / 2.0));
        let centre = |k: usize| lo + (hi - lo) * k as f64 / (MELS + 1) as f64;
        let bin_mel = |b: usize| mel(RATE / FFT as f64 * b as f64);
        let filters = (0..MELS)
            .map(|m| {
                let (l, c, r) = (centre(m), centre(m + 1), centre(m + 2));
                let mut f = [0.0; BINS];
                for (b, v) in f.iter_mut().enumerate() {
                    let x = bin_mel(b);
                    *v = ((x - l) / (c - l)).min((r - x) / (r - c)).max(0.0) as f32;
                }
                f
            })
            .collect();
        Tables {
            window,
            filters,
            fft: Fft::new(FFT),
        }
    })
}

/// The input for 16 kHz mono `pcm` (at least 560 samples): `[frames, STACKED]`, log-mel frames
/// normalised per band, two stacked; an odd count is padded with `PAD`.
pub fn input_features(pcm: &[f32]) -> Vec<f32> {
    let t = tables();
    let n = if pcm.len() < FRAME {
        0
    } else {
        1 + (pcm.len() - FRAME) / HOP
    };
    let mut feats = vec![0.0f32; n * MELS];
    let (mut re, mut im) = (vec![0.0f32; FFT], vec![0.0f32; FFT]);
    let mut power = [0.0f32; BINS];
    for f in 0..n {
        let x = &pcm[f * HOP..f * HOP + FRAME];
        let mean = x.iter().map(|v| (v * 32768.0) as f64).sum::<f64>() / FRAME as f64;
        let s: Vec<f32> = x.iter().map(|v| (v * 32768.0) - mean as f32).collect();
        re.fill(0.0);
        im.fill(0.0);
        re[0] = s[0] * (1.0 - PREEMPHASIS) * t.window[0];
        for i in 1..FRAME {
            re[i] = (s[i] - PREEMPHASIS * s[i - 1]) * t.window[i];
        }
        t.fft.forward(&mut re, &mut im);
        for (b, p) in power.iter_mut().enumerate() {
            *p = re[b] * re[b] + im[b] * im[b];
        }
        for (m, filter) in t.filters.iter().enumerate() {
            let e: f32 = filter.iter().zip(&power).map(|(a, b)| a * b).sum();
            feats[f * MELS + m] = e.max(FLOOR).ln();
        }
    }
    for m in 0..MELS {
        let col = || (0..n).map(|f| feats[f * MELS + m] as f64);
        let mean = col().sum::<f64>() / n as f64;
        let var = col().map(|v| (v - mean) * (v - mean)).sum::<f64>() / (n as f64 - 1.0);
        let inv = 1.0 / (var + 1e-7).sqrt();
        for f in 0..n {
            feats[f * MELS + m] = ((feats[f * MELS + m] as f64 - mean) * inv) as f32;
        }
    }
    if n % 2 == 1 {
        feats.resize((n + 1) * MELS, PAD);
    }
    feats
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The deterministic test signal of `tools/ctc-export/fbank-golden.py`.
    fn signal(n: usize) -> Vec<f32> {
        let mut state = 12345u32;
        (0..n)
            .map(|i| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = (state >> 8) as f32 / 16_777_216.0 - 0.5;
                let amp = (1 + (i / 8000) % 4) as f32 / 8.0;
                let tone = if (i / 40) % 2 == 0 { 0.25 } else { -0.25 };
                noise * amp + tone * amp
            })
            .collect()
    }

    fn golden(name: &str, bytes: &[u8], n: usize) {
        let want: Vec<f32> = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes(*c))
            .collect();
        let got = input_features(&signal(n));
        assert_eq!(got.len(), want.len(), "{name}");
        let worst = got
            .iter()
            .zip(&want)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f32::max);
        eprintln!("{name}: max deviation {worst:e}");
        assert!(worst < 2e-4, "{name}: {worst}");
    }

    #[test]
    fn the_features_match_the_python_extractor() {
        // 32 000 samples give 198 fbank frames, 32 160 give 199 (odd: one padded frame).
        golden("even", include_bytes!("../../tests/fbank-even.f32"), 32_000);
        golden("odd", include_bytes!("../../tests/fbank-odd.f32"), 32_160);
    }
}
