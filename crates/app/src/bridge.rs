//! Bridges the async hub into the immediate-mode UI thread.
//!
//! JPEG decoding happens off the UI thread with at most one decode in flight per
//! page; frames arriving while a page is decoding replace that page's pending frame,
//! so a slow UI drops stale frames instead of queueing them.

use std::collections::HashMap;
use std::sync::mpsc as std_mpsc;

use testudo_core::Hub;
use testudo_protocol::{Event, FrameHeader, Message};
use tokio::sync::mpsc;

use crate::model::PageKey;

pub enum UiMsg {
    Event { run_id: String, event: Event },
    Frame { key: PageKey, ts: f64, image: egui::ColorImage },
}

struct Slot {
    busy: bool,
    pending: Option<(FrameHeader, bytes::Bytes)>,
}

pub fn spawn(rt: &tokio::runtime::Handle, hub: &Hub, ctx: egui::Context) -> std_mpsc::Receiver<UiMsg> {
    let (ui_tx, ui_rx) = std_mpsc::channel();
    let mut hub_rx = hub.subscribe();

    rt.spawn(async move {
        let (done_tx, mut done_rx) = mpsc::unbounded_channel::<PageKey>();
        let mut slots: HashMap<PageKey, Slot> = HashMap::new();

        let start_decode = |key: PageKey, header: FrameHeader, jpeg: bytes::Bytes| {
            let ui_tx = ui_tx.clone();
            let done_tx = done_tx.clone();
            let ctx = ctx.clone();
            tokio::task::spawn_blocking(move || {
                if let Some(image) = decode_jpeg(&jpeg) {
                    let _ = ui_tx.send(UiMsg::Frame {
                        key: key.clone(),
                        ts: header.ts,
                        image,
                    });
                    ctx.request_repaint();
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
                            if ui_tx.send(UiMsg::Event { run_id: msg.run_id, event }).is_err() {
                                break;
                            }
                            ctx.request_repaint();
                        }
                        Message::Frame { header, jpeg } => {
                            let key = PageKey { run_id: msg.run_id, page_id: header.page_id.clone() };
                            let slot = slots.entry(key.clone()).or_insert(Slot { busy: false, pending: None });
                            if slot.busy {
                                slot.pending = Some((header, jpeg));
                            } else {
                                slot.busy = true;
                                start_decode(key, header, jpeg);
                            }
                        }
                    }
                }
                Some(key) = done_rx.recv() => {
                    if let Some(slot) = slots.get_mut(&key) {
                        match slot.pending.take() {
                            Some((header, jpeg)) => start_decode(key, header, jpeg),
                            None => { slots.remove(&key); }
                        }
                    }
                }
            }
        }
    });

    ui_rx
}

fn decode_jpeg(jpeg: &[u8]) -> Option<egui::ColorImage> {
    let img = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg)
        .ok()?
        .to_rgb8();
    let size = [img.width() as usize, img.height() as usize];
    Some(egui::ColorImage::from_rgb(size, img.as_raw()))
}
