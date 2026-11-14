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
    /// One timeline lane per runner worker, so parallel tests don't overlap.
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
    export: Option<(String, Receiver<Result<PathBuf, String>>)>,
    pub status: Option<String>,
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
        let mut lane_of: HashMap<i32, usize> = HashMap::new();
        for (at, ev) in &events {
            match ev {
                Event::TestBegin {
                    test_id,
                    title,
                    worker_index,
                    ..
                } => {
                    open.insert(test_id, spans.len());
                    let next = lane_of.len();
                    let lane = *lane_of.entry(worker_index.unwrap_or(0)).or_insert(next);
                    spans.push(TestSpan {
                        lane,
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
            lanes: lane_of.len().max(1),
            frames: HashMap::new(),
            shown: HashMap::new(),
            export: None,
            status: None,
        })
    }

    pub fn seek(&mut self, t: f64, ctx: &egui::Context, store: &Store) {
        let t = t.clamp(self.start, self.end);
        if t < self.t || self.cursor == 0 {
            let keep_closed = self.view.keep_closed_pages;
            let selected = self.view.selected_page.take();
            let tab = self.view.inspector_tab;
            self.view = PagesView::new();
            self.view.keep_closed_pages = keep_closed;
            self.view.inspector_tab = tab;
            self.view.selected_page = selected;
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
        if let Some((name, rx)) = &self.export {
            match rx.try_recv() {
                Ok(Ok(path)) => {
                    self.status = Some(format!("exported {}", path.display()));
                    self.export = None;
                }
                Ok(Err(e)) => {
                    self.status = Some(format!("export of {name} failed: {e}"));
                    self.export = None;
                }
                Err(_) => ctx.request_repaint_after(std::time::Duration::from_millis(250)),
            }
        }
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
        const LANE_H: f32 = 10.0;
        let width = ui.available_width();
        let height = 14.0 + LANE_H * self.lanes as f32;
        let (rect, resp) = ui.allocate_exact_size(vec2(width, height), Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        let span = self.end - self.start;
        let x_of = |t: f64| rect.left() + ((t - self.start) / span) as f32 * rect.width();
        painter.rect_filled(rect, 3.0, ui.visuals().extreme_bg_color);
        let mut hovered_span = None;
        for s in &self.spans {
            let top = rect.top() + 14.0 + LANE_H * s.lane as f32;
            let r = egui::Rect::from_min_max(
                pos2(x_of(s.start), top + 1.0),
                pos2(x_of(s.end).max(x_of(s.start) + 2.0), top + LANE_H - 1.0),
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

    /// Export button for the inspector header.
    pub fn export_button(&mut self, ui: &mut egui::Ui, page: Option<&PageKey>, store: &Store) {
        let Some(page) = page else { return };
        let frames = store.frame_path(&page.run_id, &page.page_id);
        if !frames.exists() {
            ui.weak("(no recording)");
            return;
        }
        if self.export.is_some() {
            ui.spinner();
            ui.weak("exporting…");
            return;
        }
        if ui.button("⬇ Export MP4").clicked() {
            let out = store.exports_dir().join(format!("{}-{}.mp4", page.run_id, page.page_id));
            let (tx, rx) = channel();
            std::thread::spawn(move || {
                let r = export_mp4(&frames, &out, &ExportOptions::default())
                    .map(|_| out)
                    .map_err(|e| format!("{e:#}"));
                let _ = tx.send(r);
            });
            self.export = Some((page.page_id.clone(), rx));
            self.status = None;
        }
        if ui.small_button("📂").on_hover_text("Open exports folder").clicked() {
            let _ = std::fs::create_dir_all(store.exports_dir());
            let _ = std::process::Command::new("xdg-open").arg(store.exports_dir()).spawn();
        }
    }

    pub fn status_ui(&self, ui: &mut egui::Ui) {
        if let Some(s) = &self.status {
            ui.label(RichText::new(s).small());
        }
    }
}

fn fmt_secs(ms: f64) -> String {
    let s = (ms / 1000.0).max(0.0);
    format!("{:02}:{:06.3}", (s / 60.0) as u64, s % 60.0)
}
