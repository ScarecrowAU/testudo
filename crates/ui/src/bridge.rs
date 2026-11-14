//! Bridges the async hub into the gpui UI.
//!
//! JPEG decoding happens off the UI thread with at most one decode in flight per
//! page; frames arriving while a page is decoding replace that page's pending
//! frame, so a slow UI drops stale frames instead of queueing them. Decoded
//! frames are already in gpui's BGRA layout.

use std::collections::HashMap;
use std::sync::Arc;

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use gpui_kit::RenderImage;
use testudo_core::Hub;
use testudo_protocol::{Event, Message};
use tokio::sync::mpsc;

use crate::model::PageKey;

pub enum UiMsg {
    Event {
        run_id: String,
        at: f64,
        event: Event,
    },
    Frame {
        key: PageKey,
        at: f64,
        image: Arc<RenderImage>,
    },
}

struct Slot {
    busy: bool,
    pending: Option<(f64, bytes::Bytes)>,
}

pub fn spawn(rt: &tokio::runtime::Handle, hub: &Hub) -> UnboundedReceiver<UiMsg> {
    let (ui_tx, ui_rx) = unbounded();
    let mut hub_rx = hub.subscribe();

    rt.spawn(async move {
        let (done_tx, mut done_rx) = mpsc::unbounded_channel::<PageKey>();
        let mut slots: HashMap<PageKey, Slot> = HashMap::new();

        let start_decode = |key: PageKey, at: f64, jpeg: bytes::Bytes, ui_tx: UnboundedSender<UiMsg>| {
            let done_tx = done_tx.clone();
            tokio::task::spawn_blocking(move || {
                if let Some(image) = decode_jpeg(&jpeg) {
                    let _ = ui_tx.unbounded_send(UiMsg::Frame {
                        key: key.clone(),
                        at,
                        image,
                    });
                }
                let _ = done_tx.send(key);
            });
        };

        loop {
            tokio::select! {
                msg = hub_rx.recv() => {
                    let Some(msg) = msg else { break };
                    match msg.message {
                        Message::Event(event) => {
                            if ui_tx.unbounded_send(UiMsg::Event { run_id: msg.run_id, at: msg.at, event }).is_err() {
                                break;
                            }
                        }
                        Message::Frame { header, jpeg } => {
                            let key = PageKey { run_id: msg.run_id, page_id: header.page_id.clone() };
                            let slot = slots.entry(key.clone()).or_insert(Slot { busy: false, pending: None });
                            if slot.busy {
                                slot.pending = Some((msg.at, jpeg));
                            } else {
                                slot.busy = true;
                                start_decode(key, msg.at, jpeg, ui_tx.clone());
                            }
                        }
                    }
                }
                Some(key) = done_rx.recv() => {
                    if let Some(slot) = slots.get_mut(&key) {
                        match slot.pending.take() {
                            Some((at, jpeg)) => start_decode(key, at, jpeg, ui_tx.clone()),
                            None => { slots.remove(&key); }
                        }
                    }
                }
            }
        }
    });

    ui_rx
}

/// Decode a screencast JPEG into a gpui image (BGRA).
pub fn decode_jpeg(jpeg: &[u8]) -> Option<Arc<RenderImage>> {
    let mut img = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg)
        .ok()?
        .to_rgba8();
    for px in img.pixels_mut() {
        px.0.swap(0, 2); // RGBA -> BGRA
    }
    Some(Arc::new(RenderImage::new(smallvec::smallvec![image::Frame::new(img)])))
}
