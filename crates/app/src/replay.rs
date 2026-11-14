//! Replay of a recorded run: re-applies stored events up to a scrub time into a
//! fresh `PagesView` and shows each page's recorded frame at that time.
//!
//! Seeking forward applies only new events; seeking backward rebuilds the view,
//! which is cheap next to frame decoding.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};

use anyhow::Result;
use egui::{Color32, RichText, Sense, Stroke, pos2, vec2};
use testudo_protocol::{Event, TestStatus};
use testudo_store::export::{ExportOptions, export_mp4};
use testudo_store::frames::FrameIndex;
use testudo_store::{RunRow, Store};

use crate::bridge::decode_jpeg;
use crate::model::PageKey;
use crate::view::{PagesView, status_icon};

const SPEEDS: [f32; 6] = [0.25, 0.5, 1.0, 2.0, 4.0, 8.0];

struct TestSpan {
    /// Timeline row; tests that overlap in time get different lanes.
    lane: usize,
    start: f64,
    end: f64,
    status: Option<TestStatus>,
    title: String,
}

pub struct Replay {
    pub run: RunRow,
    events: Vec<(f64, Event)>,
    cursor: usize,
    pub start: f64,
    pub end: f64,
    /// Current scrub time, absolute ms since epoch.
    t: f64,
    playing: bool,
    speed: f32,
    pub view: PagesView,
    spans: Vec<TestSpan>,
    lanes: usize,
    /// Lazily opened per page; `None` once we know a page has no recording.
    frames: HashMap<String, Option<FrameIndex>>,
    shown: HashMap<String, usize>,
    pub exporter: Exporter,
}

/// MP4 export of one page's recording, run on a background thread.
#[derive(Default)]
pub struct Exporter {
    job: Option<(String, Receiver<Result<PathBuf, String>>)>,
    status: Option<String>,
}

impl Replay {
    pub fn load(store: &Store, run_id: &str) -> Result<Self> {
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

        // Greedy interval packing: each test takes the first lane that is free
        // when it starts. Worker indexes are no good here: Playwright assigns a
        // new one whenever a worker restarts, so a serial run would fan out.
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

    pub fn seek(&mut self, t: f64, ctx: &egui::Context, store: &Store) {
        let t = t.clamp(self.start, self.end);
        if t < self.t || self.cursor == 0 {
            let keep_closed = self.view.keep_closed_pages;
            let selected = self.view.selected_page.take();
            let tab = self.view.inspector_tab;
            let focus = self.view.focus.take();
            self.view = PagesView::new();
            self.view.keep_closed_pages = keep_closed;
            self.view.inspector_tab = tab;
            self.view.selected_page = selected;
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
        self.view.reap(t);
        if let Some(sel) = &self.view.selected_page
            && !self.view.model.pages.contains_key(sel)
        {
            self.view.selected_page = None;
        }
        self.update_frames(ctx, store);
    }

    fn update_frames(&mut self, ctx: &egui::Context, store: &Store) {
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
                // Force replacement even when scrubbing backwards.
                if let Some(p) = self.view.model.pages.get_mut(&key) {
                    p.last_frame_ts = 0.0;
                }
                self.view.model.apply_frame(ctx, &key, at, image);
                self.shown.insert(key.page_id.clone(), i);
            }
        }
    }

    /// Open a page's focus view with the whole run's events, at the replay time.
    pub fn open_focus(&mut self, key: PageKey) {
        let mut full = crate::model::Model::new();
        for (at, ev) in &self.events {
            full.apply(&self.run.id, *at, ev.clone());
        }
        self.view.open_focus(crate::focus::PageFocus::recorded(key, full, self.t));
    }

    /// Advance playback. Call once per UI frame.
    pub fn tick(&mut self, ctx: &egui::Context, store: &Store) {
        if self.cursor == 0 {
            self.seek(self.start, ctx, store);
        }
        if self.playing {
            let dt = ctx.input(|i| i.stable_dt).min(0.1) as f64 * 1000.0 * self.speed as f64;
            let next = self.t + dt;
            if next >= self.end {
                self.playing = false;
            }
            self.seek(next, ctx, store);
            ctx.request_repaint();
        }
        self.exporter.poll(ctx);
    }

    pub fn timeline_ui(&mut self, ui: &mut egui::Ui, store: &Store) {
        let ctx = ui.ctx().clone();
        ui.horizontal(|ui| {
            let label = if self.playing { "⏸" } else { "▶" };
            if ui.button(label).clicked() {
                if !self.playing && self.t >= self.end {
                    self.seek(self.start, &ctx, store);
                }
                self.playing = !self.playing;
            }
            if ui.button("⏮").on_hover_text("Start").clicked() {
                self.seek(self.start, &ctx, store);
            }
            egui::ComboBox::from_id_salt("speed")
                .width(60.0)
                .selected_text(format!("{}×", self.speed))
                .show_ui(ui, |ui| {
                    for s in SPEEDS {
                        ui.selectable_value(&mut self.speed, s, format!("{s}×"));
                    }
                });
            ui.monospace(format!(
                "{} / {}",
                fmt_secs(self.t - self.start),
                fmt_secs(self.end - self.start)
            ));
            ui.checkbox(&mut self.view.keep_closed_pages, "Keep closed pages");
        });

        // Scrub bar with one lane of test spans per worker underneath.
        // Lanes thin out instead of growing past ~80px (Vitest runs pack many
        // short parallel tests).
        let lane_h = (80.0 / self.lanes as f32).clamp(2.0, 10.0);
        let width = ui.available_width();
        let height = 14.0 + lane_h * self.lanes as f32;
        let (rect, resp) = ui.allocate_exact_size(vec2(width, height), Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        let span = self.end - self.start;
        let x_of = |t: f64| rect.left() + ((t - self.start) / span) as f32 * rect.width();
        painter.rect_filled(rect, 3.0, ui.visuals().extreme_bg_color);
        let mut hovered_span = None;
        for s in &self.spans {
            let top = rect.top() + 14.0 + lane_h * s.lane as f32;
            let r = egui::Rect::from_min_max(
                pos2(x_of(s.start), top + 1.0),
                pos2(x_of(s.end).max(x_of(s.start) + 2.0), top + (lane_h - 1.0).max(1.0)),
            );
            let (_, color) = status_icon(s.status);
            painter.rect_filled(r, 2.0, color.gamma_multiply(0.6));
            if resp.hover_pos().is_some_and(|p| r.contains(p)) {
                hovered_span = Some(s);
            }
        }
        let x = x_of(self.t);
        painter.line_segment([pos2(x, rect.top()), pos2(x, rect.bottom())], Stroke::new(2.0, Color32::WHITE));

        if let Some(s) = hovered_span {
            resp.clone().on_hover_text(format!(
                "{} {}  ({})",
                status_icon(s.status).0,
                s.title,
                fmt_secs(s.end - s.start)
            ));
        }
        if (resp.dragged() || resp.clicked())
            && let Some(p) = resp.interact_pointer_pos()
        {
            let frac = ((p.x - rect.left()) / rect.width()).clamp(0.0, 1.0) as f64;
            self.playing = false;
            self.seek(self.start + frac * span, &ctx, store);
        }
    }
}

impl Exporter {
    pub fn poll(&mut self, ctx: &egui::Context) {
        let Some((name, rx)) = &self.job else { return };
        match rx.try_recv() {
            Ok(Ok(path)) => {
                self.status = Some(format!("exported {}", path.display()));
                self.job = None;
            }
            Ok(Err(e)) => {
                self.status = Some(format!("export of {name} failed: {e}"));
                self.job = None;
            }
            Err(_) => ctx.request_repaint_after(std::time::Duration::from_millis(250)),
        }
    }

    /// Export controls for the inspector's tab row.
    pub fn ui(&mut self, ui: &mut egui::Ui, page: Option<&PageKey>, store: &Store) {
        if let Some(page) = page {
            let frames = store.frame_path(&page.run_id, &page.page_id);
            if !frames.exists() {
                ui.weak("(no recording)");
            } else if self.job.is_some() {
                ui.spinner();
                ui.weak("exporting…");
            } else if ui.button("⬇ Export MP4").clicked() {
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
        if ui.small_button("📂").on_hover_text("Open exports folder").clicked() {
            let _ = std::fs::create_dir_all(store.exports_dir());
            let _ = std::process::Command::new("xdg-open").arg(store.exports_dir()).spawn();
        }
        if let Some(s) = &self.status {
            ui.add(egui::Label::new(RichText::new(s).small()).truncate());
        }
    }
}

fn fmt_secs(ms: f64) -> String {
    let s = (ms / 1000.0).max(0.0);
    format!("{:02}:{:06.3}", (s / 60.0) as u64, s % 60.0)
}
