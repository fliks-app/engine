//! The pure half of the background jobs: argv, output parsers and result assembly. Spec: `docs/spec/jobs-pure.md`.
pub mod fingerprint;
pub mod scan;
pub mod subtitles;

fn v(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| (*s).to_string()).collect()
}
