//! `GET /v1/events`: the engine's events as SSE (`docs/spec/runtime.md` section 7).
use axum::response::sse::{Event, KeepAlive, Sse};
use fliks_engine_core::api::EngineEvent;
use futures_util::Stream;
use std::convert::Infallible;
use tokio::sync::broadcast;

pub struct Events(broadcast::Sender<EngineEvent>);

impl Default for Events {
    fn default() -> Self {
        Self(broadcast::channel(1024).0)
    }
}

fn name(e: &EngineEvent) -> &'static str {
    match e {
        EngineEvent::SessionOpened { .. } => "SessionOpened",
        EngineEvent::SessionEnded { .. } => "SessionEnded",
        EngineEvent::PipelineChanged { .. } => "PipelineChanged",
        EngineEvent::SegmentServed { .. } => "SegmentServed",
        EngineEvent::JobStarted { .. } => "JobStarted",
        EngineEvent::JobEnded { .. } => "JobEnded",
        EngineEvent::HwFallback { .. } => "HwFallback",
        EngineEvent::BgJobProgress { .. } => "BgJobProgress",
        EngineEvent::BgJobDone { .. } => "BgJobDone",
    }
}

impl Events {
    pub fn emit(&self, e: EngineEvent) {
        // No subscriber is not an error.
        let _ = self.0.send(e);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
        self.0.subscribe()
    }

    /// A lagging subscriber skips what it missed; the host resyncs with `GET /v1/sessions`.
    pub fn sse(&self) -> Sse<impl Stream<Item = Result<Event, Infallible>> + use<>> {
        let stream = futures_util::stream::unfold(self.subscribe(), |mut rx| async move {
            loop {
                match rx.recv().await {
                    Ok(e) => {
                        let ev = Event::default()
                            .event(name(&e))
                            .data(serde_json::to_string(&e).unwrap_or_default());
                        return Some((Ok(ev), rx));
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        });
        Sse::new(stream).keep_alive(KeepAlive::default())
    }
}
