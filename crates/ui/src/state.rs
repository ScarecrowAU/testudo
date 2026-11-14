//! View state and behaviour, independent of rendering.
//!
//! GPU memory rule: gpui keeps every `RenderImage` it has drawn in its atlas
//! until `window.drop_image` is called. Anything here that replaces or drops a
//! frame pushes the old one onto a `Retired` list; the root view frees them
//! at the start of each render.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::Instant;

use gpui_kit::RenderImage;
use testudo_protocol::{Event, TestStatus};
use testudo_store::export::{ExportOptions, export_mp4};
use testudo_store::frames::FrameIndex;
use testudo_store::{RunRow, Store};

use crate::bridge::decode_jpeg;
use crate::model::{Effect, Model, Page, PageKey};

pub type Retired = Vec<Arc<RenderImage>>;

/// How long a closed page lingers when "keep closed pages" is off.
pub const CLOSED_PAGE_LINGER_MS: f64 = 3000.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InspectorTab {
    Network,
    Console,
    Output,
}

/// A page grid with run list and inspector, driven purely by events. The live
/// view and the replay view are both a `PagesView`.
pub struct PagesView {
    pub model: Model,
    pub selected_page: Option<PageKey>,
    pub selected_run: Option<String>,
    pub inspector_tab: InspectorTab,
    pub keep_closed_pages: bool,
    pub hide_repeated_stderr: bool,
    pub focus: Option<PageFocus>,
}

impl PagesView {
    pub fn new() -> Self {
        Self {
            model: Model::new(),
            selected_page: None,
            selected_run: None,
            inspector_tab: InspectorTab::Output,
            keep_closed_pages: true,
            hide_repeated_stderr: true,
            focus: None,
        }
    }

    pub fn apply(&mut self, run_id: &str, at: f64, event: Event) {
        let _ = matches!(self.model.apply(run_id, at, event), Effect::PageOpened);
        if self.selected_run.is_none() {
            self.selected_run = Some(run_id.to_owned());
        }
    }

    /// Drop pages closed longer than the linger time (only when not keeping
    /// closed pages). Returns true while closed pages are still lingering.
    pub fn reap(&mut self, now: f64, retired: &mut Retired) -> bool {
        if self.keep_closed_pages {
            return false;
        }
        let expired: Vec<PageKey> = self
            .model
            .pages
            .values()
            .filter(|p| p.closed_at.is_some_and(|t| now - t >= CLOSED_PAGE_LINGER_MS))
            .map(|p| p.key.clone())
            .collect();
        for key in expired {
            if let Some(mut page) = self.model.pages.remove(&key) {
                retired.extend(page.frame.take());
            }
            if self.selected_page.as_ref() == Some(&key) {
                self.selected_page = None;
            }
        }
        self.model.pages.values().any(|p| p.closed_at.is_some())
    }

    pub fn open_focus(&mut self, focus: PageFocus) {
        self.selected_page = Some(focus.key.clone());
        self.inspector_tab = InspectorTab::Network;
        self.focus = Some(focus);
    }

    /// Take every image this view holds, before replacing or dropping it.
    pub fn take_images(&mut self) -> Retired {
        let mut out = self.model.take_frames();
        if let Some(f) = &mut self.focus {
            out.extend(f.take_images());
        }
        out
    }
}

// --- page focus ---------------------------------------------------------------

const SPEEDS: [f32; 5] = [0.25, 0.5, 1.0, 2.0, 4.0];
/// While a page is still recording, re-read its frame index at most this often.
const REINDEX_EVERY_MS: u128 = 400;

/// Single-page focus: follow a running page live, or scrub its recording. One
/// clock throughout (hub receive time) for frames, requests and console.
pub struct PageFocus {
    pub key: PageKey,
    /// History: the whole run's model, so the page's full timeline is available
    /// whatever the replay time. Live: None, the live model is used.
    pub recorded: Option<Box<Model>>,
    pub follow_live: bool,
    pub t: f64,
    pub playing: bool,
    pub speed: f32,
    frames: Option<FrameIndex>,
    indexed_at: Option<Instant>,
    shown: Option<usize>,
    pub image: Option<Arc<RenderImage>>,
    /// Scroll the synced lists to the scrub time on the next draw.
    pub sync_lists: bool,
    pub wheel_px: f32,
}

impl PageFocus {
    pub fn live(page: &Page) -> Self {
        let running = page.closed_at.is_none();
        Self::new(page.key.clone(), None, running, page.closed_at.unwrap_or(f64::MAX))
    }

    pub fn recorded(key: PageKey, model: Model, t: f64) -> Self {
        Self::new(key, Some(Box::new(model)), false, t)
    }

    fn new(key: PageKey, recorded: Option<Box<Model>>, follow_live: bool, t: f64) -> Self {
        Self {
            key,
            recorded,
            follow_live,
            t,
            playing: false,
            speed: 1.0,
            frames: None,
            indexed_at: None,
            shown: None,
            image: None,
            sync_lists: true,
            wheel_px: 0.0,
        }
    }

    pub fn take_images(&mut self) -> Retired {
        let mut out: Retired = self.image.take().into_iter().collect();
        if let Some(m) = &mut self.recorded {
            out.extend(m.take_frames());
        }
        out
    }

    pub fn cycle_speed(&mut self) {
        let i = SPEEDS.iter().position(|s| *s == self.speed).unwrap_or(2);
        self.speed = SPEEDS[(i + 1) % SPEEDS.len()];
    }

    /// (start, end) of the page's timeline; a running page's end is now.
    pub fn bounds(&self, page: &Page) -> (f64, f64) {
        let start = page.opened_at;
        let end = page.closed_at.unwrap_or_else(testudo_core::now_ms);
        let last_frame = self.frames.as_ref().and_then(|f| f.entries.last()).map_or(start, |e| e.at);
        (start, end.max(last_frame).max(start + 1.0))
    }

    pub fn seek(&mut self, t: f64, page: &Page) {
        let (start, end) = self.bounds(page);
        self.t = t.clamp(start, end);
        self.follow_live = false;
        self.sync_lists = true;
    }

    /// Make sure the frame index exists and, for a page still recording, covers
    /// the scrub time. Returns false when there is no recording to read.
    pub fn ensure_index(&mut self, page: &Page, store: Option<&Store>) -> bool {
        let Some(store) = store else { return false };
        let stale = self.indexed_at.is_none_or(|t| t.elapsed().as_millis() >= REINDEX_EVERY_MS);
        let needs = match &self.frames {
            None => self.indexed_at.is_none() || (page.closed_at.is_none() && stale),
            Some(f) => {
                stale && (page.closed_at.is_none() || f.entries.last().is_none_or(|e| Some(e.at) < page.closed_at))
            }
        };
        if needs {
            let path = store.frame_path(&page.key.run_id, &page.key.page_id);
            self.frames = path.exists().then(|| FrameIndex::open(&path).ok()).flatten();
            self.indexed_at = Some(Instant::now());
        }
        self.frames.as_ref().is_some_and(|f| !f.entries.is_empty())
    }

    pub fn has_recording(&self) -> bool {
        self.frames.as_ref().is_some_and(|f| !f.entries.is_empty())
    }

    /// (index, total) of the frame at the scrub time.
    pub fn frame_position(&self) -> Option<(usize, usize)> {
        let f = self.frames.as_ref()?;
        Some((f.index_at(self.t)?, f.entries.len()))
    }

    pub fn step_frames(&mut self, n: i64, page: &Page, store: Option<&Store>) {
        if !self.ensure_index(page, store) {
            return;
        }
        let f = self.frames.as_ref().unwrap();
        let cur = f.index_at(self.t).unwrap_or(0) as i64;
        let next = (cur + n).clamp(0, f.entries.len() as i64 - 1) as usize;
        let at = f.entries[next].at;
        self.seek(at, page);
    }

    /// Decode the recorded frame for the scrub time if it changed.
    pub fn update_image(&mut self, page: &Page, store: Option<&Store>, retired: &mut Retired) {
        if self.follow_live || !self.ensure_index(page, store) {
            return;
        }
        let f = self.frames.as_mut().unwrap();
        let Some(i) = f.index_at(self.t) else { return };
        if self.shown == Some(i) {
            return;
        }
        if let Some(image) = f.read(i).ok().and_then(|j| decode_jpeg(&j)) {
            retired.extend(self.image.replace(image));
            self.shown = Some(i);
        }
    }

    /// Advance playback/follow state for this frame. Returns true if the view
    /// should keep animating.
    pub fn tick(&mut self, page: &Page, running: bool, dt_ms: f64) -> bool {
        if !running && self.follow_live {
            // Closed while followed: become a recording at its last moment.
            self.follow_live = false;
            self.t = page.closed_at.unwrap_or(self.t);
        }
        let (_, end) = self.bounds(page);
        if self.t == f64::MAX {
            self.t = end;
        }
        if self.follow_live {
            self.t = end;
            return true;
        }
        if self.playing {
            self.t += dt_ms * self.speed as f64;
            self.sync_lists = true;
            if self.t >= end {
                self.t = end;
                self.playing = false;
                self.follow_live = running;
            }
            return true;
        }
        false
    }
}

// --- replay -------------------------------------------------------------------

pub struct TestSpan {
    /// Timeline row; tests that overlap in time get different lanes.
    pub lane: usize,
    pub start: f64,
    pub end: f64,
    pub status: Option<TestStatus>,
    pub title: String,
}

/// Replay of a recorded run: re-applies stored events up to a scrub time into
/// a fresh `PagesView` and shows each page's recorded frame at that time.
pub struct Replay {
    pub run: RunRow,
    pub events: Vec<(f64, Event)>,
    cursor: usize,
    pub start: f64,
    pub end: f64,
    pub t: f64,
    pub playing: bool,
    pub speed: f32,
    pub view: PagesView,
    pub spans: Vec<TestSpan>,
    pub lanes: usize,
    frames: HashMap<String, Option<FrameIndex>>,
    shown: HashMap<String, usize>,
    pub exporter: Exporter,
}

impl Replay {
    pub fn load(store: &Store, run_id: &str) -> anyhow::Result<Self> {
        let run = store.run(run_id)?.ok_or_else(|| anyhow::anyhow!("run {run_id} not found"))?;
        let events = store.run_events(run_id)?;
        let start = run.started_at;
        let end = run
            .ended_at
            .or_else(|| events.last().map(|(at, _)| *at))
            .unwrap_or(start)
            .max(start + 1.0);

        let mut spans: Vec<TestSpan> = Vec::new();
        let mut open: HashMap<&str, usize> = HashMap::new();
        for (at, ev) in &events {
            match ev {
                Event::TestBegin { test_id, title, .. } => {
                    open.insert(test_id, spans.len());
                    spans.push(TestSpan {
                        lane: 0,
                        start: *at,
                        end,
                        status: None,
                        title: title.clone(),
                    });
                }
                Event::TestEnd { test_id, status, .. } => {
                    if let Some(&i) = open.get(test_id.as_str()) {
                        spans[i].end = *at;
                        spans[i].status = Some(*status);
                    }
                }
                _ => {}
            }
        }
        // Greedy interval packing: each test takes the first free lane.
        let mut lane_free_at: Vec<f64> = Vec::new();
        for s in &mut spans {
            s.lane = match lane_free_at.iter().position(|free| *free <= s.start) {
                Some(i) => i,
                None => {
                    lane_free_at.push(0.0);
                    lane_free_at.len() - 1
                }
            };
            lane_free_at[s.lane] = s.end;
        }
        let lanes = lane_free_at.len().max(1);

        Ok(Self {
            run,
            events,
            cursor: 0,
            start,
            end,
            t: start,
            playing: false,
            speed: 1.0,
            view: PagesView::new(),
            spans,
            lanes,
            frames: HashMap::new(),
            shown: HashMap::new(),
            exporter: Exporter::default(),
        })
    }

    pub fn seek(&mut self, t: f64, store: &Store, retired: &mut Retired) {
        let t = t.clamp(self.start, self.end);
        if t < self.t || self.cursor == 0 {
            let old = std::mem::replace(&mut self.view, PagesView::new());
            let PagesView {
                mut model,
                selected_page,
                inspector_tab,
                keep_closed_pages,
                hide_repeated_stderr,
                focus,
                ..
            } = old;
            retired.extend(model.take_frames());
            self.view.selected_page = selected_page;
            self.view.inspector_tab = inspector_tab;
            self.view.keep_closed_pages = keep_closed_pages;
            self.view.hide_repeated_stderr = hide_repeated_stderr;
            self.view.focus = focus;
            self.cursor = 0;
            self.shown.clear();
        }
        while let Some((at, ev)) = self.events.get(self.cursor) {
            if *at > t {
                break;
            }
            self.view.apply(&self.run.id, *at, ev.clone());
            self.cursor += 1;
        }
        self.t = t;
        self.view.reap(t, retired);
        self.update_frames(store, retired);
    }

    fn update_frames(&mut self, store: &Store, retired: &mut Retired) {
        let keys: Vec<PageKey> = self.view.model.pages.keys().cloned().collect();
        for key in keys {
            let index = self.frames.entry(key.page_id.clone()).or_insert_with(|| {
                let path = store.frame_path(&key.run_id, &key.page_id);
                path.exists().then(|| FrameIndex::open(&path).ok()).flatten()
            });
            let Some(index) = index else { continue };
            let Some(i) = index.index_at(self.t) else { continue };
            if self.shown.get(&key.page_id) == Some(&i) {
                continue;
            }
            let at = index.entries[i].at;
            if let Some(image) = index.read(i).ok().and_then(|j| decode_jpeg(&j)) {
                if let Some(p) = self.view.model.pages.get_mut(&key) {
                    p.last_frame_ts = 0.0; // allow stepping backwards
                }
                retired.extend(self.view.model.apply_frame(&key, at, image));
                self.shown.insert(key.page_id.clone(), i);
            }
        }
    }

    /// Advance playback. Returns true while playing.
    pub fn tick(&mut self, dt_ms: f64, store: &Store, retired: &mut Retired) -> bool {
        if self.cursor == 0 {
            self.seek(self.start, store, retired);
        }
        if !self.playing {
            return false;
        }
        let next = self.t + dt_ms * self.speed as f64;
        if next >= self.end {
            self.playing = false;
        }
        self.seek(next, store, retired);
        true
    }

    pub fn cycle_speed(&mut self) {
        let speeds = [0.25, 0.5, 1.0, 2.0, 4.0, 8.0];
        let i = speeds.iter().position(|s| *s == self.speed).unwrap_or(2);
        self.speed = speeds[(i + 1) % speeds.len()];
    }

    /// A focus view over the whole run's events, at the replay time.
    pub fn open_focus(&mut self, key: PageKey) {
        let mut full = Model::new();
        for (at, ev) in &self.events {
            full.apply(&self.run.id, *at, ev.clone());
        }
        self.view.open_focus(PageFocus::recorded(key, full, self.t));
    }
}

/// MP4 export of one page's recording, run on a background thread.
#[derive(Default)]
pub struct Exporter {
    job: Option<(String, Receiver<Result<PathBuf, String>>)>,
    pub status: Option<String>,
}

impl Exporter {
    pub fn busy(&self) -> bool {
        self.job.is_some()
    }

    /// Returns true while an export is still running.
    pub fn poll(&mut self) -> bool {
        let Some((name, rx)) = &self.job else { return false };
        match rx.try_recv() {
            Ok(Ok(path)) => {
                self.status = Some(format!("exported {}", path.display()));
                self.job = None;
                false
            }
            Ok(Err(e)) => {
                self.status = Some(format!("export of {name} failed: {e}"));
                self.job = None;
                false
            }
            Err(_) => true,
        }
    }

    pub fn start(&mut self, page: &PageKey, store: &Store) {
        let frames = store.frame_path(&page.run_id, &page.page_id);
        let out = store.exports_dir().join(format!("{}-{}.mp4", page.run_id, page.page_id));
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let r = export_mp4(&frames, &out, &ExportOptions::default())
                .map(|_| out)
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(r);
        });
        self.job = Some((page.page_id.clone(), rx));
        self.status = None;
    }
}
