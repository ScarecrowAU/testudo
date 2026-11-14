use std::sync::{Arc, Mutex};

use testudo_protocol::Message;
use tokio::sync::mpsc;

pub type RunId = String;

#[derive(Debug, Clone)]
pub struct HubMessage {
    pub run_id: RunId,
    pub message: Message,
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

    pub fn publish(&self, run_id: &str, message: Message) {
        let mut subs = self.subscribers.lock().unwrap();
        subs.retain(|tx| {
            tx.send(HubMessage {
                run_id: run_id.to_owned(),
                message: message.clone(),
            })
            .is_ok()
        });
    }
}
