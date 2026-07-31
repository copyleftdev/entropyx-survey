//! SSE emitter: adapts the pipeline's `Progress` trait onto an axum
//! event channel.
//!
//! Cancellation is inferred from send failure. When the browser closes
//! the EventSource, axum drops the receiver, every subsequent send errors,
//! and the pipeline sees `cancelled()` at its next checkpoint and unwinds.
//! Without this a closed tab would leave a twelve-minute betweenness
//! computation running.

use crate::pipeline::{Phase, Progress};
use axum::response::sse::Event as SseEvent;
use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc::UnboundedSender;

pub struct Emitter {
    tx: UnboundedSender<Result<SseEvent, Infallible>>,
    dead: AtomicBool,
}

impl Emitter {
    pub fn new(tx: UnboundedSender<Result<SseEvent, Infallible>>) -> Self {
        Self {
            tx,
            dead: AtomicBool::new(false),
        }
    }

    pub fn send(&self, name: &str, data: serde_json::Value) {
        if self.dead.load(Ordering::Relaxed) {
            return;
        }
        let ev = SseEvent::default().event(name).data(data.to_string());
        if self.tx.send(Ok(ev)).is_err() {
            self.dead.store(true, Ordering::Relaxed);
        }
    }

    pub fn meta(&self, v: serde_json::Value) {
        self.send("meta", v);
    }

    pub fn cached(&self) {
        self.send("cached", serde_json::json!({ "cached": true }));
    }

    pub fn error(&self, message: &str) {
        self.send("error", serde_json::json!({ "message": message }));
    }

    pub fn done(&self, v: serde_json::Value) {
        self.send("done", v);
    }
}

impl Progress for Emitter {
    fn phase_start(&self, phase: Phase) {
        self.send(
            "phase",
            serde_json::json!({
                "id": phase.id(),
                "label": phase.label(),
                "weight": phase.weight(),
                "status": "start",
            }),
        );
    }

    fn phase_done(&self, phase: Phase, elapsed_ms: u128, detail: serde_json::Value) {
        self.send(
            "phase",
            serde_json::json!({
                "id": phase.id(),
                "label": phase.label(),
                "weight": phase.weight(),
                "status": "done",
                "elapsed_ms": elapsed_ms as u64,
                "detail": detail,
            }),
        );
    }

    fn tick(&self, phase: Phase, done: usize, total: usize) {
        self.send(
            "tick",
            serde_json::json!({ "id": phase.id(), "done": done, "total": total }),
        );
    }

    fn cancelled(&self) -> bool {
        self.dead.load(Ordering::Relaxed)
    }
}
