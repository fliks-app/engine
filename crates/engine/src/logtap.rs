//! The engine's `warn` and `error` log lines as `Log` events, for a host that asked for them.
use crate::runtime::Runtime;
use fliks_engine_core::api::EngineEvent;
use std::fmt::Write;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

const MAX_MESSAGE: usize = 2000;
const BACKLOG: usize = 256;

pub struct Line {
    level: Level,
    target: String,
    message: String,
}

/// Queues what the subscriber sees; a full queue drops the line rather than slow the logger.
pub struct Tap(mpsc::Sender<Line>);

pub fn channel() -> (Tap, mpsc::Receiver<Line>) {
    let (tx, rx) = mpsc::channel(BACKLOG);
    (Tap(tx), rx)
}

#[derive(Default)]
struct Message(String);

impl Visit for Message {
    fn record_debug(&mut self, f: &Field, v: &dyn std::fmt::Debug) {
        let _ = match f.name() {
            "message" => write!(self.0, "{v:?}"),
            n => write!(self.0, " {n}={v:?}"),
        };
    }
}

impl<S: Subscriber> Layer<S> for Tap {
    fn on_event(&self, e: &Event<'_>, _: Context<'_, S>) {
        let mut m = Message::default();
        e.record(&mut m);
        let mut message = m.0.trim().to_string();
        message.truncate(message.floor_char_boundary(MAX_MESSAGE));
        let meta = e.metadata();
        let _ = self.0.try_send(Line {
            level: *meta.level(),
            target: meta.target().into(),
            message,
        });
    }
}

/// Emits the queued lines as events while the host's config asks for them.
pub fn forward(rt: &Arc<Runtime>, mut rx: mpsc::Receiver<Line>) {
    let rt = rt.clone();
    tokio::spawn(async move {
        while let Some(l) = rx.recv().await {
            if rt.current_config().log_events {
                rt.events.emit(EngineEvent::Log {
                    level: l.level.as_str().to_lowercase(),
                    target: l.target,
                    message: l.message,
                });
            }
        }
    });
}
