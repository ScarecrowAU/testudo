use std::sync::{Arc, Mutex};

use testudo_protocol::Message;
use tokio::sync::mpsc;

pub type RunId = String;

#[derive(Debug, Clone)]
pub struct HubMessage {
    pub run_id: RunId,
    /// Receive time in ms since the Unix epoch. The single clock every consumer
    /// (live view, recorder, replay) orders events and frames by.
    pub at: f64,
    pub message: Message,
}

pub fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

/// Fan-out of every message from every run to every subscriber.
///
/// Unbounded per subscriber so lifecycle events are never dropped. Subscribers that
/// care about frame backpressure (the GUI) should drain promptly and drop stale
/// frames themselves.
#[derive(Clone, Default)]
pub struct Hub {
    subscribers: Arc<Mutex<Vec<mpsc::UnboundedSender<HubMessage>>>>,
}

impl Hub {
    pub fn subscribe(&self) -> mpsc::UnboundedReceiver<HubMessage> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.subscribers.lock().unwrap().push(tx);
        rx
    }

    /// Drop all subscriptions so subscribers see end-of-stream after draining what
    /// is already queued. Used at shutdown so the recorder can flush and exit.
    pub fn close(&self) {
        self.subscribers.lock().unwrap().clear();
    }

    pub fn publish(&self, run_id: &str, message: Message) {
        let at = now_ms();
        let mut subs = self.subscribers.lock().unwrap();
        subs.retain(|tx| {
            tx.send(HubMessage {
                run_id: run_id.to_owned(),
                at,
                message: message.clone(),
            })
            .is_ok()
        });
    }
}
