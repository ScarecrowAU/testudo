//! Frame decoding off the UI thread.
//!
//! Decoding a recorded JPEG (and converting it to gpui's BGRA) costs about a
//! millisecond in release builds, but much more in dev builds and for large
//! frames; done on the UI thread it is paid on every frame of playback, twice
//! when comparing. Players ask a `FrameLoader` for the frame they want; it is
//! decoded by a small shared pool and shown when ready, while the previous
//! frame stays on screen. A frame already in flight is finished (and shown)
//! before the newest request starts, so a player that outruns the decoder
//! still moves, showing the latest frame it can, instead of stalling.

use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex, OnceLock};

use gpui_kit::RenderImage;

use crate::state::Retired;

type Job = (Vec<u8>, Sender<Option<Arc<RenderImage>>>);

/// Decoder threads shared by every player.
const WORKERS: usize = 2;

fn pool() -> &'static Mutex<Sender<Job>> {
    static POOL: OnceLock<Mutex<Sender<Job>>> = OnceLock::new();
    POOL.get_or_init(|| {
        let (tx, rx) = channel::<Job>();
        let rx = Arc::new(Mutex::new(rx));
        for n in 0..WORKERS {
            let rx = rx.clone();
            let _ = std::thread::Builder::new()
                .name(format!("testudo-decode-{n}"))
                .spawn(move || {
                    loop {
                        let job = rx.lock().ok().and_then(|r| r.recv().ok());
                        let Some((jpeg, reply)) = job else { return };
                        let _ = reply.send(crate::bridge::decode_jpeg(&jpeg));
                    }
                });
        }
        Mutex::new(tx)
    })
}

/// One stream of frames (a player): which frame is shown, which is decoding.
#[derive(Default)]
pub struct FrameLoader {
    shown: Option<usize>,
    pending: Option<(usize, Receiver<Option<Arc<RenderImage>>>)>,
}

impl FrameLoader {
    /// Bring frame `want` on screen (into `image`), reading its JPEG with
    /// `read` when a decode has to start. Returns true while a decode is in
    /// flight, so the caller keeps frames coming until it lands.
    pub fn update(
        &mut self,
        want: usize,
        read: impl FnOnce(usize) -> Option<Vec<u8>>,
        image: &mut Option<Arc<RenderImage>>,
        retired: &mut Retired,
    ) -> bool {
        if let Some((at, rx)) = &self.pending {
            match rx.try_recv() {
                Ok(decoded) => {
                    if let Some(img) = decoded {
                        retired.extend(image.replace(img));
                        self.shown = Some(*at);
                        crate::perf::count_decode();
                    }
                    self.pending = None;
                }
                Err(TryRecvError::Empty) => return true,
                Err(TryRecvError::Disconnected) => self.pending = None,
            }
        }
        if self.shown == Some(want) {
            return false;
        }
        let Some(jpeg) = read(want) else { return false };
        let (tx, rx) = channel();
        if pool().lock().map(|p| p.send((jpeg, tx))).is_ok_and(|r| r.is_ok()) {
            self.pending = Some((want, rx));
            true
        } else {
            false
        }
    }
}
