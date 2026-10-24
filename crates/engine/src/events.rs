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
        EngineEvent::LiveAttempt { .. } => "LiveAttempt",
        EngineEvent::LiveReleased { .. } => "LiveReleased",
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

    /// A subscriber that lagged is cut: the host's reconnect resyncs it, where skipping would not.
    pub fn sse(&self) -> Sse<impl Stream<Item = Result<Event, Infallible>> + use<>> {
        let stream = futures_util::stream::unfold(self.subscribe(), |mut rx| async move {
            let e = rx.recv().await.ok()?;
            let ev = Event::default()
                .event(name(&e))
                .data(serde_json::to_string(&e).unwrap_or_default());
            Some((Ok(ev), rx))
        });
        Sse::new(stream).keep_alive(KeepAlive::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;
    use std::time::Duration;

    fn ev(i: usize) -> EngineEvent {
        EngineEvent::BgJobProgress {
            job_id: i.to_string(),
            pct: 1.0,
        }
    }

    #[tokio::test]
    async fn a_lagged_subscriber_stream_ends() {
        let events = Events::default();
        let res = events.sse().into_response();
        for i in 0..2000 {
            events.emit(ev(i));
        }
        let body = axum::body::to_bytes(res.into_body(), usize::MAX);
        let out = tokio::time::timeout(Duration::from_secs(5), body).await;
        assert!(out.is_ok(), "the stream must end on lag, not stay open");
    }
}
