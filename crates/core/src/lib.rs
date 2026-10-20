//! Pure core: wire types and decision logic. No tokio, fs or process.
pub mod api;
pub mod args;
pub mod decide;
pub mod hls;
pub mod hw;
pub mod intro;
pub mod jobs;
pub mod jsnum;
pub mod ladder;
pub mod live;
pub mod mp4;
pub mod mpegts;
pub mod probe;
pub mod session;

/// Owned argv or flag list from literals.
pub(crate) fn strs(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| (*s).to_string()).collect()
}
