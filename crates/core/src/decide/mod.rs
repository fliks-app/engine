//! The playback decision: DirectPlay, DirectStream (remux) or Transcode, a pure port of the
//! host's `StreamBuilderService.evaluate()`. Spec: `docs/spec/decide.md`.
mod caps;
mod codec;
mod dolby_vision;
mod tonemap;
