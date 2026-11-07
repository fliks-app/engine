//! The pure half of the background jobs: argv, output parsers and result assembly.
//! Specs: `docs/spec/jobs.md` (framework, sprite), `docs/spec/jobs-pure.md`.
pub mod cover;
pub mod cues;
pub mod fingerprint;
pub mod merge;
pub mod ocr;
pub mod optimize;
pub mod scan;
pub mod sprite;
pub mod srt;
pub mod subtitles;
pub mod sync;
