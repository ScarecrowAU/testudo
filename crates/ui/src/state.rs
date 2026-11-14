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
use testudo_store::{PriorPage, RunRow, Store};

use crate::model::{Effect, Model, Page, PageKey};
use crate::perf::ui_decode;

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
    /// Failing tests whose full error is expanded in the run list.
    pub expanded_tests: std::collections::HashSet<String>,
    /// Runs shown as just their header and counts in the run list.
    pub collapsed_runs: std::collections::HashSet<String>,
    /// Output tab: stream filter, and whether it follows new lines.
    pub output_stream: StreamFilter,
    pub output_follow: bool,
    /// Lines shown at the last render, to tell new output from a user scroll.
    pub output_seen: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StreamFilter {
    All,
    Stdout,
    Stderr,
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
            expanded_tests: Default::default(),
            collapsed_runs: Default::default(),
            output_stream: StreamFilter::All,
            output_follow: true,
            output_seen: 0,
        }
    }

    pub fn apply(&mut self, run_id: &str, at: f64, event: Event) {
        // A new run collapses the finished ones in the run list. Runs still
        // going stay open: "run all" starts several suites together.
        if matches!(event, Event::RunStarted { .. }) && !self.model.runs.iter().any(|r| r.id == run_id) {
            let finished = self.model.runs.iter().filter(|r| !r.is_running()).map(|r| r.id.clone());
            self.collapsed_runs.extend(finished);
        }
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

    /// A new run is starting: pages of runs that already finished leave the
    /// grid (they stay in history and in the focus view's comparison). The
    /// focused page stays so an open focus view isn't pulled away.
    pub fn drop_finished_run_pages(&mut self, retired: &mut Retired) {
        let finished: std::collections::HashSet<&str> = self
            .model
            .runs
            .iter()
            .filter(|r| !r.is_running())
            .map(|r| r.id.as_str())
            .collect();
        let focused = self.focus.as_ref().filter(|f| f.recorded.is_none()).map(|f| &f.key);
        let gone: Vec<PageKey> = self
            .model
            .pages
            .keys()
            .filter(|k| finished.contains(k.run_id.as_str()) && Some(*k) != focused)
            .cloned()
            .collect();
        for key in gone {
            if let Some(mut page) = self.model.pages.remove(&key) {
                retired.extend(page.frame.take());
            }
            if self.selected_page.as_ref() == Some(&key) {
                self.selected_page = None;
            }
        }
    }

    /// Forget every run and page, keeping view settings. Returns the images
    /// it held, to be retired.
    pub fn clear(&mut self) -> Retired {
        let images = self.take_images();
        let mut fresh = PagesView::new();
        fresh.inspector_tab = self.inspector_tab;
        fresh.keep_closed_pages = self.keep_closed_pages;
        fresh.hide_repeated_stderr = self.hide_repeated_stderr;
        fresh.output_stream = self.output_stream;
        fresh.output_follow = self.output_follow;
        *self = fresh;
        images
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

/// Tabs of the focus view's details panel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FocusTab {
    Steps,
    Network,
    Console,
    Output,
}

/// Network table filter chips.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NetFilter {
    All,
    Document,
    Fetch,
    Script,
    Style,
    Image,
    Other,
    Errors,
}

impl NetFilter {
    pub const ALL: [NetFilter; 8] = [
        NetFilter::All,
        NetFilter::Document,
        NetFilter::Fetch,
        NetFilter::Script,
        NetFilter::Style,
        NetFilter::Image,
        NetFilter::Other,
        NetFilter::Errors,
    ];

    pub fn label(self) -> &'static str {
        match self {
            NetFilter::All => "All",
            NetFilter::Document => "Doc",
            NetFilter::Fetch => "Fetch/XHR",
            NetFilter::Script => "JS",
            NetFilter::Style => "CSS",
            NetFilter::Image => "Img",
            NetFilter::Other => "Other",
            NetFilter::Errors => "Errors",
        }
    }

    pub fn matches(self, resource_type: &str, failed: bool) -> bool {
        match self {
            NetFilter::All => true,
            NetFilter::Errors => failed,
            NetFilter::Document => resource_type == "document",
            NetFilter::Fetch => matches!(resource_type, "fetch" | "xhr"),
            NetFilter::Script => resource_type == "script",
            NetFilter::Style => resource_type == "stylesheet",
            NetFilter::Image => matches!(resource_type, "image" | "media" | "font"),
            NetFilter::Other => !matches!(
                resource_type,
                "document" | "fetch" | "xhr" | "script" | "stylesheet" | "image" | "media" | "font"
            ),
        }
    }
}

/// Console filter chips.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConsoleFilter {
    All,
    Errors,
    Warnings,
}

/// Playback speeds offered by both players.
pub const SPEEDS: [f32; 6] = [0.25, 0.5, 0.75, 1.0, 1.5, 2.0];
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
    loader: crate::decode::FrameLoader,
    pub image: Option<Arc<RenderImage>>,
    /// Scroll the synced lists to the scrub time on the next draw.
    pub sync_lists: bool,
    pub wheel_px: f32,
    pub tab: FocusTab,
    pub net_filter: NetFilter,
    pub console_filter: ConsoleFilter,
    /// What the mouse is over on the timeline (step / request / navigation).
    pub hover_label: Option<String>,
    /// An earlier recording of the same test shown beside this page, played
    /// in step with it; arrows move through the test's history.
    pub compare: Option<Compare>,
    /// Line compared recordings up by test step (else by time since open).
    pub compare_by_step: bool,
}

/// How far back the comparison's history goes, in runs.
pub const COMPARE_HISTORY: usize = 50;

/// Earlier recordings of the focused test, newest first, with one loaded.
/// At most this many earlier runs are shown at once (with this run: six,
/// an even grid).
pub const MAX_COMPARED: usize = 5;

pub struct Compare {
    pub candidates: Vec<PriorPage>,
    /// First candidate shown (0 = the most recent earlier run).
    pub index: usize,
    /// How many earlier runs are shown (1..=MAX_COMPARED).
    pub count: usize,
    /// The shown ones, `candidates[index..index + count]`, loaded.
    pub lanes: Vec<CompareLane>,
}

impl Compare {
    /// Load the lanes for the current window, keeping ones already loaded;
    /// returns the images of lanes no longer shown.
    fn refresh(&mut self, store: &Store) -> Retired {
        self.count = self.count.clamp(1, MAX_COMPARED);
        let last_start = self.candidates.len().saturating_sub(self.count);
        self.index = self.index.min(last_start);
        let mut old = std::mem::take(&mut self.lanes);
        let wanted: Vec<PriorPage> = self
            .candidates
            .iter()
            .skip(self.index)
            .take(self.count)
            .cloned()
            .collect();
        self.lanes = wanted
            .into_iter()
            .map(|p| {
                match old
                    .iter()
                    .position(|l| l.prior.run_id == p.run_id && l.prior.page_id == p.page_id)
                {
                    Some(i) => old.swap_remove(i),
                    None => CompareLane::new(p, store),
                }
            })
            .collect();
        old.into_iter().filter_map(|mut l| l.image.take()).collect()
    }
}

pub struct CompareLane {
    pub prior: PriorPage,
    /// The earlier run's steps for this test: (start time, title).
    steps: Vec<(f64, String)>,
    /// Step alignment found no matching step at the current position, so
    /// this lane is shown by time.
    pub by_time_fallback: bool,
    frames: Option<FrameIndex>,
    loader: crate::decode::FrameLoader,
    pub image: Option<Arc<RenderImage>>,
}

impl CompareLane {
    fn new(prior: PriorPage, store: &Store) -> Self {
        let frames = FrameIndex::open(&store.frame_path(&prior.run_id, &prior.page_id)).ok();
        let mut model = Model::new();
        for (at, ev) in store.run_events(&prior.run_id).unwrap_or_default() {
            model.apply(&prior.run_id, at, ev);
        }
        let steps = model
            .runs
            .first()
            .and_then(|r| r.test(&prior.test_id))
            .map(|t| t.steps.iter().map(|s| (s.started, s.title.clone())).collect())
            .unwrap_or_default();
        Self {
            prior,
            steps,
            by_time_fallback: false,
            frames,
            loader: Default::default(),
            image: None,
        }
    }

    /// Length of this recording: page lifetime, or its last frame if later.
    pub fn length(&self) -> f64 {
        let last = self
            .frames
            .as_ref()
            .and_then(|f| f.entries.last())
            .map_or(0.0, |e| e.at - self.prior.opened_at);
        self.prior.closed_at.map_or(0.0, |c| c - self.prior.opened_at).max(last)
    }
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
            loader: Default::default(),
            image: None,
            sync_lists: true,
            wheel_px: 0.0,
            tab: FocusTab::Steps,
            net_filter: NetFilter::All,
            console_filter: ConsoleFilter::All,
            hover_label: None,
            compare: None,
            compare_by_step: true,
        }
    }

    pub fn take_images(&mut self) -> Retired {
        let mut out: Retired = self.image.take().into_iter().collect();
        if let Some(m) = &mut self.recorded {
            out.extend(m.take_frames());
        }
        out.extend(self.close_compare());
        out
    }

    /// Show the most recent earlier recording of `test` (file + title path)
    /// beside this page; `step_compare` moves through older ones.
    #[tracing::instrument(level = "info", name = "compare_open", skip(self, title_path, store), fields(runs = tracing::field::Empty))]
    pub fn open_compare(&mut self, file: &str, title_path: &[String], store: &Store) {
        let candidates = store
            .prior_pages(file, title_path, &self.key.run_id, COMPARE_HISTORY)
            .inspect_err(|e| tracing::warn!("comparison query failed: {e:#}"))
            .unwrap_or_default();
        tracing::Span::current().record("runs", candidates.len());
        let mut c = Compare {
            candidates,
            index: 0,
            count: 1,
            lanes: Vec::new(),
        };
        let _ = c.refresh(store);
        self.compare = Some(c);
    }

    /// Move the shown window to older (`delta` > 0) or newer (< 0) runs.
    pub fn step_compare(&mut self, delta: isize, store: &Store) -> Retired {
        let Some(c) = &mut self.compare else { return Vec::new() };
        let index = (c.index as isize + delta).max(0) as usize;
        if index == c.index {
            return Vec::new();
        }
        c.index = index;
        c.refresh(store)
    }

    /// Show one more (`delta` > 0) or one fewer earlier run.
    pub fn resize_compare(&mut self, delta: isize, store: &Store) -> Retired {
        let Some(c) = &mut self.compare else { return Vec::new() };
        let count = (c.count as isize + delta).clamp(1, MAX_COMPARED as isize) as usize;
        if count == c.count {
            return Vec::new();
        }
        c.count = count;
        c.refresh(store)
    }

    pub fn close_compare(&mut self) -> Retired {
        self.compare
            .take()
            .into_iter()
            .flat_map(|c| c.lanes)
            .filter_map(|mut l| l.image.take())
            .collect()
    }

    /// Show each compared recording at the point matching the scrub position:
    /// the same step (see `align_by_step`) or the same time since page open.
    /// `steps` are this page's test steps. Before its first frame: its first
    /// frame.
    pub fn update_compare(&mut self, page: &Page, steps: &[(f64, String)], retired: &mut Retired) -> bool {
        let t = self.t.min(self.bounds(page).1);
        let by_step = self.compare_by_step;
        let mut decoding = false;
        for lane in self.compare.iter_mut().flat_map(|c| c.lanes.iter_mut()) {
            let stepped = by_step
                .then(|| crate::logic::align_by_step(steps, &lane.steps, t, page.opened_at, lane.prior.opened_at))
                .flatten();
            lane.by_time_fallback = by_step && stepped.is_none();
            let at = stepped.unwrap_or(lane.prior.opened_at + (t - page.opened_at));
            let Some(f) = lane.frames.as_mut().filter(|f| !f.entries.is_empty()) else {
                continue;
            };
            let i = f.index_at(at).unwrap_or(0);
            decoding |= lane.loader.update(i, |i| f.read(i).ok(), &mut lane.image, retired);
        }
        decoding
    }

    /// (start, end) of the page's timeline; a running page's end is now.
    pub fn bounds(&self, page: &Page) -> (f64, f64) {
        let start = page.opened_at;
        let end = page.closed_at.unwrap_or_else(testudo_core::now_ms);
        let last_frame = self
            .frames
            .as_ref()
            .and_then(|f| f.entries.last())
            .map_or(start, |e| e.at);
        // A finished page's timeline also covers longer compared recordings.
        let compared = match (page.closed_at, &self.compare) {
            (Some(_), Some(c)) => start + c.lanes.iter().map(CompareLane::length).fold(0.0, f64::max),
            _ => start,
        };
        (start, end.max(last_frame).max(compared).max(start + 1.0))
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
        let stale = self
            .indexed_at
            .is_none_or(|t| t.elapsed().as_millis() >= REINDEX_EVERY_MS);
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

    /// Bring the recorded frame for the scrub time on screen (decoded off
    /// the UI thread). Returns true while a decode is in flight.
    pub fn update_image(&mut self, page: &Page, store: Option<&Store>, retired: &mut Retired) -> bool {
        if self.follow_live || !self.ensure_index(page, store) {
            return false;
        }
        let f = self.frames.as_mut().unwrap();
        let Some(i) = f.index_at(self.t) else { return false };
        self.loader.update(i, |i| f.read(i).ok(), &mut self.image, retired)
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
            // Never play the stretch before the page opened (a History focus
            // can start at the run's time, before this page existed).
            let (start, _) = self.bounds(page);
            self.t = self.t.max(start) + dt_ms * self.speed as f64;
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
    #[tracing::instrument(level = "info", name = "replay_load", skip(store), fields(events = tracing::field::Empty))]
    pub fn load(store: &Store, run_id: &str) -> anyhow::Result<Self> {
        let run = store
            .run(run_id)?
            .ok_or_else(|| anyhow::anyhow!("run {run_id} not found"))?;
        let events = store.run_events(run_id)?;
        tracing::Span::current().record("events", events.len());
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
                expanded_tests,
                collapsed_runs,
                ..
            } = old;
            retired.extend(model.take_frames());
            self.view.selected_page = selected_page;
            self.view.inspector_tab = inspector_tab;
            self.view.keep_closed_pages = keep_closed_pages;
            self.view.hide_repeated_stderr = hide_repeated_stderr;
            self.view.focus = focus;
            self.view.expanded_tests = expanded_tests;
            self.view.collapsed_runs = collapsed_runs;
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
            if let Some(image) = index.read(i).ok().and_then(|j| ui_decode(&j)) {
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
        let out = store
            .exports_dir()
            .join(format!("{}-{}.mp4", page.run_id, page.page_id));
        let (tx, rx) = channel();
        let span = tracing::info_span!("export_mp4", run.id = %page.run_id, page.id = %page.page_id);
        std::thread::spawn(move || {
            let _span = span.entered();
            let r = export_mp4(&frames, &out, &ExportOptions::default())
                .map(|_| out)
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(r);
        });
        self.job = Some((page.page_id.clone(), rx));
        self.status = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page_events(view: &mut PagesView, run: &str, at: f64) {
        let started = Event::RunStarted {
            label: run.into(),
            runner: "playwright".into(),
            cwd: "/x".into(),
            args: vec![],
        };
        view.apply(run, at, started);
        let opened = Event::PageOpened {
            page_id: "p".into(),
            test_id: None,
            worker_index: None,
            browser: None,
            url: None,
        };
        view.apply(run, at, opened);
        view.apply(run, at + 1.0, Event::PageClosed { page_id: "p".into() });
    }

    #[test]
    fn new_run_drops_pages_of_finished_runs_only() {
        let mut view = PagesView::new();
        page_events(&mut view, "old", 0.0);
        view.apply("old", 2.0, Event::ProcessExited { code: Some(0) });
        page_events(&mut view, "busy", 3.0); // still running
        let mut retired = Vec::new();
        view.drop_finished_run_pages(&mut retired);
        let runs: Vec<&str> = view.model.pages.keys().map(|k| k.run_id.as_str()).collect();
        assert_eq!(runs, ["busy"]);
        assert_eq!(view.model.runs.len(), 2, "runs stay in the run list");
    }

    #[test]
    fn clear_forgets_runs_and_pages_but_keeps_settings() {
        let mut view = PagesView::new();
        view.keep_closed_pages = false;
        view.hide_repeated_stderr = false;
        page_events(&mut view, "r1", 0.0);
        view.collapsed_runs.insert("r1".into());
        let _ = view.clear();
        assert!(view.model.runs.is_empty() && view.model.pages.is_empty());
        assert!(view.collapsed_runs.is_empty() && view.selected_run.is_none());
        assert!(!view.keep_closed_pages && !view.hide_repeated_stderr, "settings kept");
    }

    #[test]
    fn new_run_collapses_finished_runs_only() {
        let mut view = PagesView::new();
        page_events(&mut view, "old", 0.0);
        view.apply("old", 2.0, Event::ProcessExited { code: Some(0) });
        page_events(&mut view, "busy", 3.0); // still running
        assert!(view.collapsed_runs.contains("old"));
        page_events(&mut view, "new", 5.0);
        assert!(view.collapsed_runs.contains("old"));
        assert!(!view.collapsed_runs.contains("busy"), "running runs stay open");
        assert!(!view.collapsed_runs.contains("new"));
        // Later events of an existing run don't collapse anything else.
        view.apply("busy", 6.0, Event::ProcessExited { code: Some(0) });
        view.apply("new", 7.0, Event::RunBegin { total_tests: None });
        assert!(!view.collapsed_runs.contains("busy"));
    }
}
