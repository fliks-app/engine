//! Pure session logic: keys, cache profile hash, seek/quality-switch matrix, throttle, segment
//! responses, lifetimes and cache eviction. Time is an argument (`i64` ms), files are a
//! `SegmentSet`. Spec: `docs/spec/session.md`.
mod cache;
mod key;
mod lifetime;
mod plan;
mod profile;
mod resume;
mod segments;
mod throttle;

pub use cache::*;
pub use key::*;
pub use lifetime::*;
pub use plan::*;
pub use profile::*;
pub use resume::*;
pub use segments::*;
pub use throttle::*;
