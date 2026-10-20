//! The pure half of the background jobs: argv, output parsers and result assembly.
//! Specs: `docs/spec/jobs.md` (framework, sprite), `docs/spec/jobs-pure.md`.
pub mod cues;
pub mod fingerprint;
pub mod ocr;
pub mod scan;
pub mod sprite;
pub mod srt;
pub mod subtitles;
pub mod sync;

fn v(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| (*s).to_string()).collect()
}
