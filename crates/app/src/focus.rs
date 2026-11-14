//! Single-page focus view: watch a running page live in a large view, or scrub
//! through a page's recording, with its network and console rows synced to the
//! scrub time.
//!
//! One clock throughout: frames, requests, console messages and navigations are
//! all stamped with the hub's receive time, so "what had happened by t" is a
//! plain comparison.
//!
//! - Running page: follows live (the bridge's latest frame). Scrolling back or
//!   stepping switches to the on-disk recording made so far; LIVE returns.
//! - Finished page / History: scrub the recording. Wheel = coarse scrub,
//!   Shift+wheel or ←/→ = one frame, Space = play/pause, Esc = back to grid.

use std::time::Instant;

use egui::{Align2, Color32, FontId, RichText, Sense, Stroke, TextureHandle, pos2, vec2};
use testudo_store::Store;
use testudo_store::frames::FrameIndex;

use crate::bridge::decode_jpeg;
use crate::model::{Model, Page, PageKey};
use crate::view::{pane_name, status_icon, truncate};

const SPEEDS: [f32; 5] = [0.25, 0.5, 1.0, 2.0, 4.0];
/// One wheel notch (~50px) moves this fraction of the page's duration.
const WHEEL_FRACTION_PER_NOTCH: f64 = 0.02;
/// Shift+wheel: pixels of scroll per frame step.
const WHEEL_PX_PER_FRAME: f32 = 25.0;
/// While a page is still recording, re-read its frame index at most this often.
const REINDEX_EVERY_MS: u128 = 400;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowState {
    /// Not started yet at the scrub time.
    Pending,
    /// Started, not finished.
    InFlight,
    Done,
}

/// Where a request (or any started/finished span) stands at time `t`.
pub fn row_state(started: f64, finished: Option<f64>, t: f64) -> RowState {
    if started > t {
        RowState::Pending
    } else if finished.is_none_or(|f| f > t) {
        RowState::InFlight
    } else {
        RowState::Done
    }
}

pub struct PageFocus {
    pub key: PageKey,
    /// History: the whole run's model, so the page's full timeline is available
    /// whatever the replay time. Live: None, the live model is used.
    recorded: Option<Box<Model>>,
    follow_live: bool,
    t: f64,
    playing: bool,
    speed: f32,
    frames: Option<FrameIndex>,
    indexed_at: Option<Instant>,
    shown: Option<usize>,
    texture: Option<TextureHandle>,
    wheel_px: f32,
    /// Scroll the synced lists to the scrub time on the next draw.
    sync_lists: bool,
    pub close: bool,
}

impl PageFocus {
    /// Focus a page of the live model: follow it if still running, otherwise
    /// open its recording at the last frame.
    pub fn live(page: &Page) -> Self {
        let running = page.closed_at.is_none();
        Self::new(page.key.clone(), None, running, page.closed_at.unwrap_or(f64::MAX))
    }

    /// Focus a page of a recorded run at time `t`.
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
            texture: None,
            wheel_px: 0.0,
            sync_lists: true,
            close: false,
        }
    }

    pub fn time(&self) -> f64 {
        self.t
    }

    /// (start, end) of the page's timeline; a running page's end is now.
    fn bounds(&self, page: &Page) -> (f64, f64) {
        let start = page.opened_at;
        let end = page.closed_at.unwrap_or_else(testudo_core::now_ms);
        let last_frame = self.frames.as_ref().and_then(|f| f.entries.last()).map_or(start, |e| e.at);
        (start, end.max(last_frame).max(start + 1.0))
    }

    fn seek(&mut self, t: f64, page: &Page) {
        let (start, end) = self.bounds(page);
        self.t = t.clamp(start, end);
        self.follow_live = false;
        self.sync_lists = true;
    }

    /// Make sure the frame index exists and, for a page still recording, covers
    /// the scrub time. Returns false when there is no recording to read.
    fn ensure_index(&mut self, page: &Page, store: Option<&Store>) -> bool {
        let Some(store) = store else { return false };
        let stale = self.indexed_at.is_none_or(|t| t.elapsed().as_millis() >= REINDEX_EVERY_MS);
        let needs = match &self.frames {
            None => self.indexed_at.is_none() || (page.closed_at.is_none() && stale),
            Some(f) => {
                stale && (page.closed_at.is_none() || f.entries.last().is_none_or(|e| e.at < page.closed_at.unwrap()))
            }
        };
        if needs {
            let path = store.frame_path(&page.key.run_id, &page.key.page_id);
            self.frames = path.exists().then(|| FrameIndex::open(&path).ok()).flatten();
            self.indexed_at = Some(Instant::now());
        }
        self.frames.as_ref().is_some_and(|f| !f.entries.is_empty())
    }

    fn step_frames(&mut self, n: i64, page: &Page, store: Option<&Store>) {
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
    fn update_texture(&mut self, ctx: &egui::Context, page: &Page, store: Option<&Store>) {
        if !self.ensure_index(page, store) {
            return;
        }
        let f = self.frames.as_mut().unwrap();
        let Some(i) = f.index_at(self.t) else { return };
        if self.shown == Some(i) {
            return;
        }
        if let Some(image) = f.read(i).ok().and_then(|j| decode_jpeg(&j)) {
            match &mut self.texture {
                Some(t) => t.set(image, egui::TextureOptions::LINEAR),
                None => self.texture = Some(ctx.load_texture("focus-frame", image, egui::TextureOptions::LINEAR)),
            }
            self.shown = Some(i);
        }
    }

    /// The big view: header, scrub bar, frame. `live` is the live view's model.
    pub fn ui(&mut self, ui: &mut egui::Ui, live: &Model, store: Option<&Store>) {
        let recorded = self.recorded.take();
        let model = recorded.as_deref().unwrap_or(live);
        match model.pages.get(&self.key) {
            Some(page) => self.ui_page(ui, model, page, store),
            None => self.close = true, // page dropped from the live view
        }
        self.recorded = recorded;
    }

    fn ui_page(&mut self, ui: &mut egui::Ui, model: &Model, page: &Page, store: Option<&Store>) {
        let ctx = ui.ctx().clone();
        let running = page.closed_at.is_none() && self.recorded.is_none();
        if !running && self.follow_live {
            // The page closed while we were following it: become a recording.
            self.follow_live = false;
            self.t = page.closed_at.unwrap_or(self.t);
        }
        let (start, end) = self.bounds(page);
        if self.follow_live {
            self.t = end;
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        self.keyboard(ui, page, store);
        if self.playing {
            let dt = ctx.input(|i| i.stable_dt).min(0.1) as f64 * 1000.0 * self.speed as f64;
            self.t += dt;
            self.sync_lists = true;
            if self.t >= end {
                self.t = end;
                self.playing = false;
                self.follow_live = running;
            }
            ctx.request_repaint();
        }
        if self.t == f64::MAX {
            self.t = end;
        }

        // --- header ---
        let test = model.test_for_page(page);
        let (_, color) = status_icon(test.and_then(|t| t.status));
        ui.horizontal(|ui| {
            if ui.button("← All pages").on_hover_text("Esc").clicked() {
                self.close = true;
            }
            ui.label(RichText::new(pane_name(model, page)).strong().color(color));
            ui.weak(page.browser.as_deref().unwrap_or(""));
            ui.separator();
            let url = page
                .navigations
                .iter()
                .rev()
                .find(|(at, _)| *at <= self.t)
                .map_or(page.url.as_str(), |(_, u)| u.as_str());
            ui.add(egui::Label::new(RichText::new(url).weak()).truncate());
        });
        ui.horizontal(|ui| {
            if ui.button("⏮").on_hover_text("Start").clicked() {
                self.seek(start, page);
            }
            if ui.button("◀").on_hover_text("Previous frame (←, Shift+scroll)").clicked() {
                self.step_frames(-1, page, store);
            }
            let play = if self.playing { "⏸" } else { "▶" };
            if ui.button(play).on_hover_text("Space").clicked() {
                if !self.playing && self.t >= end - 1.0 {
                    self.seek(start, page);
                }
                self.playing = !self.playing;
                self.follow_live = false;
            }
            if ui.button("▶|").on_hover_text("Next frame (→, Shift+scroll)").clicked() {
                self.step_frames(1, page, store);
            }
            egui::ComboBox::from_id_salt("focus-speed")
                .width(56.0)
                .selected_text(format!("{}×", self.speed))
                .show_ui(ui, |ui| {
                    for s in SPEEDS {
                        ui.selectable_value(&mut self.speed, s, format!("{s}×"));
                    }
                });
            ui.monospace(format!("{:7.2}s / {:.2}s", (self.t - start) / 1000.0, (end - start) / 1000.0));
            if let Some(f) = &self.frames
                && !self.follow_live
                && let Some(i) = f.index_at(self.t)
            {
                ui.weak(format!("frame {}/{}", i + 1, f.entries.len()));
            }
            if running {
                let live = RichText::new("● LIVE").color(if self.follow_live { Color32::LIGHT_RED } else { Color32::GRAY });
                if ui.selectable_label(self.follow_live, live).on_hover_text("Follow the page in real time").clicked() {
                    self.follow_live = true;
                    self.playing = false;
                    self.sync_lists = true;
                }
            }
        });

        self.scrub_bar(ui, page, start, end);

        // --- frame ---
        if !self.follow_live {
            self.update_texture(&ctx, page, store);
        }
        let have_recording = self.frames.as_ref().is_some_and(|f| !f.entries.is_empty());
        let tex = if self.follow_live || !have_recording { page.texture.as_ref() } else { self.texture.as_ref() };
        let avail = ui.available_size();
        let (rect, resp) = ui.allocate_exact_size(avail, Sense::click());
        match tex {
            Some(tex) => {
                let [w, h] = tex.size().map(|v| v as f32);
                let scale = (rect.width() / w).min(rect.height() / h);
                let img = egui::Rect::from_center_size(rect.center(), vec2(w * scale, h * scale));
                ui.painter().image(
                    tex.id(),
                    img,
                    egui::Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
            None => {
                ui.painter().text(
                    rect.center(),
                    Align2::CENTER_CENTER,
                    "No frames recorded for this page",
                    FontId::proportional(14.0),
                    ui.visuals().weak_text_color(),
                );
            }
        }
        if !have_recording && !self.follow_live && store.is_some() && !running {
            ui.painter().text(
                rect.left_bottom() + vec2(6.0, -6.0),
                Align2::LEFT_BOTTOM,
                "recording unavailable (disabled or removed by retention); showing last live frame",
                FontId::proportional(11.0),
                ui.visuals().weak_text_color(),
            );
        }
        if resp.hovered() {
            self.wheel(ui, page, store, start, end);
        }
    }

    fn wheel(&mut self, ui: &egui::Ui, page: &Page, store: Option<&Store>, start: f64, end: f64) {
        let (delta, shift) = ui.input(|i| (i.smooth_scroll_delta, i.modifiers.shift));
        // Shift often turns vertical wheel motion into horizontal: take both.
        let d = delta.y + delta.x;
        if d == 0.0 {
            return;
        }
        self.playing = false;
        if shift {
            self.wheel_px += d;
            let steps = (self.wheel_px / WHEEL_PX_PER_FRAME).trunc();
            if steps != 0.0 {
                self.wheel_px -= steps * WHEEL_PX_PER_FRAME;
                // Wheel up / content moving down = back in time.
                self.step_frames(-(steps as i64), page, store);
            }
        } else {
            let dt = -(d as f64 / 50.0) * WHEEL_FRACTION_PER_NOTCH * (end - start);
            let t = if self.follow_live { end } else { self.t };
            self.seek(t + dt, page);
        }
    }

    fn keyboard(&mut self, ui: &egui::Ui, page: &Page, store: Option<&Store>) {
        // Leave keys alone while a text field has focus.
        if ui.ctx().memory(|m| m.focused().is_some()) {
            return;
        }
        let (left, right, space, esc) = ui.input(|i| {
            (
                i.key_pressed(egui::Key::ArrowLeft),
                i.key_pressed(egui::Key::ArrowRight),
                i.key_pressed(egui::Key::Space),
                i.key_pressed(egui::Key::Escape),
            )
        });
        if left {
            self.playing = false;
            self.step_frames(-1, page, store);
        }
        if right {
            self.playing = false;
            self.step_frames(1, page, store);
        }
        if space {
            self.playing = !self.playing;
            self.follow_live = false;
        }
        if esc {
            self.close = true;
        }
    }

    fn scrub_bar(&mut self, ui: &mut egui::Ui, page: &Page, start: f64, end: f64) {
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 3.0, ui.visuals().extreme_bg_color);
        let span = end - start;
        let x_of = |t: f64| rect.left() + ((t - start) / span).clamp(0.0, 1.0) as f32 * rect.width();
        let weak = ui.visuals().weak_text_color();
        // Requests: short ticks along the bottom.
        for r in &page.network {
            let x = x_of(r.started);
            let c = if r.error.is_some() || r.status.is_some_and(|s| s >= 400) { Color32::LIGHT_RED } else { weak };
            painter.line_segment([pos2(x, rect.bottom() - 7.0), pos2(x, rect.bottom() - 2.0)], Stroke::new(1.0, c));
        }
        // Navigations: tall blue.
        for (at, _) in &page.navigations {
            let x = x_of(*at);
            painter.line_segment([pos2(x, rect.top() + 2.0), pos2(x, rect.bottom() - 2.0)], Stroke::new(1.5, Color32::LIGHT_BLUE));
        }
        // Console errors: red dots on top.
        for c in page.console.iter().filter(|c| c.level == "error") {
            painter.circle_filled(pos2(x_of(c.at), rect.top() + 5.0), 2.5, Color32::LIGHT_RED);
        }
        let x = x_of(self.t);
        painter.line_segment([pos2(x, rect.top()), pos2(x, rect.bottom())], Stroke::new(2.0, Color32::WHITE));
        if (resp.dragged() || resp.clicked())
            && let Some(p) = resp.interact_pointer_pos()
        {
            self.playing = false;
            let frac = ((p.x - rect.left()) / rect.width()).clamp(0.0, 1.0) as f64;
            self.seek(start + frac * span, page);
        }
        resp.on_hover_text("drag to scrub · blue: navigation · red: errors · ticks: requests");
    }

    // --- synced inspector lists ------------------------------------------------

    pub fn network_ui(&mut self, ui: &mut egui::Ui, live: &Model) {
        let recorded = self.recorded.take();
        let model = recorded.as_deref().unwrap_or(live);
        if let Some(page) = model.pages.get(&self.key) {
            self.network_rows(ui, page);
        }
        self.recorded = recorded;
    }

    fn network_rows(&mut self, ui: &mut egui::Ui, page: &Page) {
        let row_h = ui.text_style_height(&egui::TextStyle::Monospace) + ui.spacing().item_spacing.y;
        let t = self.t;
        let text = ui.visuals().text_color();
        let current = page.network.iter().rposition(|r| r.started <= t);
        let mut area = egui::ScrollArea::both().auto_shrink(false).id_salt("focus-net");
        if self.sync_lists && let Some(i) = current {
            area = area.vertical_scroll_offset((i as f32 * row_h - ui.available_height() / 2.0).max(0.0));
        }
        let mut seek_to = None;
        area.show_rows(ui, row_h, page.network.len(), |ui, range| {
            for r in &page.network[range] {
                let state = row_state(r.started, r.finished, t);
                let status = match (state, r.status, &r.error) {
                    (RowState::Pending, ..) => "".to_owned(),
                    (RowState::InFlight, ..) => "…".to_owned(),
                    (_, Some(s), _) => s.to_string(),
                    (_, None, Some(e)) => format!("ERR {e}"),
                    _ => "?".to_owned(),
                };
                let ms = match (state, r.finished) {
                    (RowState::Done, Some(f)) => format!("{:.0}ms", f - r.started),
                    _ => String::new(),
                };
                let failed = r.error.is_some() || r.status.is_some_and(|s| s >= 400);
                let color = match state {
                    RowState::Pending => text.gamma_multiply(0.3),
                    RowState::InFlight => Color32::LIGHT_BLUE,
                    RowState::Done if failed => Color32::LIGHT_RED,
                    RowState::Done => text,
                };
                let line = format!(
                    "{:>8.3}s {:<6} {:>5} {:>7} {:<10} {}",
                    (r.started - page.opened_at) / 1000.0,
                    r.method,
                    status,
                    ms,
                    r.resource_type,
                    truncate(&r.url, 200)
                );
                let resp = ui.add(egui::Label::new(RichText::new(line).monospace().color(color)).sense(Sense::click()));
                if resp.on_hover_text("click to jump here").clicked() {
                    seek_to = Some(r.started);
                }
            }
        });
        self.sync_lists = false;
        if let Some(at) = seek_to {
            self.seek(at, page);
        }
    }

    pub fn console_ui(&mut self, ui: &mut egui::Ui, live: &Model) {
        let recorded = self.recorded.take();
        let model = recorded.as_deref().unwrap_or(live);
        if let Some(page) = model.pages.get(&self.key) {
            let row_h = ui.text_style_height(&egui::TextStyle::Monospace) + ui.spacing().item_spacing.y;
            let t = self.t;
            let text = ui.visuals().text_color();
            let mut seek_to = None;
            egui::ScrollArea::both().auto_shrink(false).id_salt("focus-console").show_rows(
                ui,
                row_h,
                page.console.len(),
                |ui, range| {
                    for c in &page.console[range] {
                        let base = match c.level.as_str() {
                            "error" => Color32::LIGHT_RED,
                            "warning" | "warn" => Color32::GOLD,
                            _ => text,
                        };
                        let color = if c.at > t { base.gamma_multiply(0.3) } else { base };
                        let line = format!("{:>8.3}s [{}] {}", (c.at - page.opened_at) / 1000.0, c.level, c.text);
                        let resp =
                            ui.add(egui::Label::new(RichText::new(line).monospace().color(color)).sense(Sense::click()));
                        if resp.on_hover_text("click to jump here").clicked() {
                            seek_to = Some(c.at);
                        }
                    }
                },
            );
            if let Some(at) = seek_to {
                self.seek(at, page);
            }
        }
        self.recorded = recorded;
    }
}

#[cfg(test)]
mod tests {
    use super::{RowState, row_state};

    #[test]
    fn rows_relative_to_scrub_time() {
        assert_eq!(row_state(10.0, Some(20.0), 5.0), RowState::Pending);
        assert_eq!(row_state(10.0, Some(20.0), 10.0), RowState::InFlight);
        assert_eq!(row_state(10.0, Some(20.0), 15.0), RowState::InFlight);
        assert_eq!(row_state(10.0, Some(20.0), 20.0), RowState::Done);
        assert_eq!(row_state(10.0, None, 99.0), RowState::InFlight, "no response yet");
    }
}
