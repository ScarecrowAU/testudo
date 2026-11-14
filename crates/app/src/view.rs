//! A tiled view of browser pages plus run/test list and inspector, driven purely
//! by events. The live view and the replay view are both a `PagesView`; only the
//! source of events (hub vs. recorded history) and the notion of "now" differ.

use std::collections::HashMap;

use egui::{Color32, RichText, Sense, Stroke};
use egui_tiles::{SimplificationOptions, Tile, TileId, Tiles, Tree, UiResponse};
use testudo_core::{RunHandle, RunId};
use testudo_protocol::{Event, StdStream, TestStatus};
use testudo_store::Store;

use crate::model::{Effect, Model, Page, PageKey, Test};

/// How long a closed page's last frame stays on screen before its pane is removed.
pub const CLOSED_PAGE_LINGER_MS: f64 = 3000.0;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum InspectorTab {
    Network,
    Console,
    Output,
}

/// Something the runs panel asks the app to do.
pub enum RunAction {
    OpenReplay(RunId),
}

pub struct PagesView {
    pub model: Model,
    tree: Tree<PageKey>,
    pub selected_page: Option<PageKey>,
    pub selected_run: Option<String>,
    pub inspector_tab: InspectorTab,
    pub keep_closed_pages: bool,
}

impl PagesView {
    pub fn new() -> Self {
        Self {
            model: Model::new(),
            tree: Tree::new_grid(egui::Id::unique("testudo-pages"), vec![]),
            selected_page: None,
            selected_run: None,
            inspector_tab: InspectorTab::Output,
            keep_closed_pages: false,
        }
    }

    pub fn apply(&mut self, run_id: &str, at: f64, event: Event) {
        if let Effect::PageOpened(key) = self.model.apply(run_id, at, event) {
            self.add_pane(key);
        }
        if self.selected_run.is_none() {
            self.selected_run = Some(run_id.to_owned());
        }
    }

    fn add_pane(&mut self, key: PageKey) {
        let tile = self.tree.tiles.insert_pane(key.clone());
        match self.tree.root.and_then(|r| self.tree.tiles.get_mut(r)) {
            Some(Tile::Container(container)) => container.add_child(tile),
            Some(Tile::Pane(_)) => {
                let old_root = self.tree.root.unwrap();
                self.tree.root = Some(self.tree.tiles.insert_grid_tile(vec![old_root, tile]));
            }
            None => self.tree.root = Some(self.tree.tiles.insert_grid_tile(vec![tile])),
        }
        if let Some(page) = self.model.pages.get_mut(&key) {
            page.tile = Some(tile);
        }
    }

    /// Remove panes of pages that closed more than the linger time before `now`.
    /// Returns true while closed pages are still lingering (caller should repaint).
    pub fn reap(&mut self, now: f64) -> bool {
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
            if let Some(page) = self.model.pages.remove(&key)
                && let Some(tile) = page.tile
            {
                self.tree.remove_recursively(tile);
            }
            if self.selected_page.as_ref() == Some(&key) {
                self.selected_page = None;
            }
        }
        self.model.pages.values().any(|p| p.closed_at.is_some())
    }

    // --- panels ------------------------------------------------------------------

    /// Run list with per-test status. `handles` enables Stop buttons (live view);
    /// `store` enables per-test history on hover and Replay buttons.
    pub fn runs_ui(
        &mut self,
        ui: &mut egui::Ui,
        mut handles: Option<&mut HashMap<RunId, RunHandle>>,
        store: Option<&Store>,
    ) -> Option<RunAction> {
        let mut action = None;
        for run in self.model.runs.iter().rev() {
            let (passed, failed, running) = run.counts();
            let status = match (&run.exit, run.run_end.as_deref()) {
                (None, _) => RichText::new("running").color(Color32::LIGHT_BLUE),
                (Some(_), Some("passed")) | (Some(Some(0)), _) => RichText::new("passed").color(Color32::LIGHT_GREEN),
                (Some(Some(code)), _) => RichText::new(format!("failed (exit {code})")).color(Color32::LIGHT_RED),
                (Some(None), _) => RichText::new("killed").color(Color32::LIGHT_RED),
            };
            let selected = self.selected_run.as_deref() == Some(&run.id);
            ui.horizontal(|ui| {
                if ui.selectable_label(selected, RichText::new(&run.label).strong()).clicked() {
                    self.selected_run = Some(run.id.clone());
                    self.inspector_tab = InspectorTab::Output;
                }
                ui.label(status);
                if run.is_running() {
                    if let Some(h) = handles.as_deref_mut().and_then(|h| h.get_mut(&run.id))
                        && ui.small_button("■ Stop").clicked()
                    {
                        h.cancel();
                    }
                } else if handles.is_some() && store.is_some() && ui.small_button("⟲ Replay").clicked() {
                    action = Some(RunAction::OpenReplay(run.id.clone()));
                }
            });
            let total = run.total_tests.map(|t| format!("/{t}")).unwrap_or_default();
            ui.weak(format!("✔ {passed}  ✘ {failed}  ⏳ {running}  ({}{total})", run.tests.len()));
            egui::CollapsingHeader::new("Tests")
                .id_salt(&run.id)
                .default_open(true)
                .show(ui, |ui| {
                    for t in &run.tests {
                        test_row(ui, t, store);
                    }
                });
            ui.separator();
        }
        action
    }

    pub fn pages_ui(&mut self, ui: &mut egui::Ui, empty_hint: &str) {
        if self.model.pages.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.weak(empty_hint);
            });
            return;
        }
        let mut behavior = PaneBehavior {
            model: &self.model,
            selected: &mut self.selected_page,
            clicked: false,
        };
        self.tree.ui(&mut behavior, ui);
        if behavior.clicked && self.inspector_tab == InspectorTab::Output {
            self.inspector_tab = InspectorTab::Network;
        }
    }

    /// `header_extra` adds widgets to the inspector's tab row (e.g. Export).
    pub fn inspector_ui(&mut self, ui: &mut egui::Ui, header_extra: impl FnOnce(&mut egui::Ui, Option<&PageKey>)) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.inspector_tab, InspectorTab::Network, "Network");
            ui.selectable_value(&mut self.inspector_tab, InspectorTab::Console, "Console");
            ui.selectable_value(&mut self.inspector_tab, InspectorTab::Output, "Output");
            ui.separator();
            match (&self.inspector_tab, &self.selected_page, &self.selected_run) {
                (InspectorTab::Output, _, Some(run)) => ui.weak(format!("run {run}")),
                (InspectorTab::Output, _, None) => ui.weak("select a run"),
                (_, Some(p), _) => ui.weak(format!("page {}", p.page_id)),
                (_, None, _) => ui.weak("click a page to inspect it"),
            };
            header_extra(ui, self.selected_page.as_ref());
        });
        ui.separator();

        let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
        match self.inspector_tab {
            InspectorTab::Output => {
                let Some(run) = self.selected_run.as_deref().and_then(|id| self.model.run(id)) else { return };
                let lines: Vec<(Color32, &str)> = run
                    .stdio
                    .iter()
                    .map(|(s, l)| {
                        let c = if *s == StdStream::Stderr { Color32::LIGHT_RED } else { ui.visuals().text_color() };
                        (c, l.as_str())
                    })
                    .chain(run.adapter_logs.iter().map(|l| (Color32::GOLD, l.as_str())))
                    .collect();
                egui::ScrollArea::both()
                    .stick_to_bottom(true)
                    .auto_shrink(false)
                    .show_rows(ui, row_h, lines.len(), |ui, range| {
                        for (c, l) in &lines[range] {
                            ui.label(RichText::new(*l).monospace().color(*c));
                        }
                    });
            }
            InspectorTab::Network => {
                let Some(page) = self.selected_page.as_ref().and_then(|k| self.model.pages.get(k)) else { return };
                egui::ScrollArea::both()
                    .stick_to_bottom(true)
                    .auto_shrink(false)
                    .show_rows(ui, row_h, page.network.len(), |ui, range| {
                        for r in &page.network[range] {
                            let status = match (&r.status, &r.error) {
                                (Some(s), _) => s.to_string(),
                                (None, Some(e)) => format!("ERR {e}"),
                                (None, None) => "…".into(),
                            };
                            let ms = r.finished.map(|f| format!("{:.0}ms", f - r.started)).unwrap_or_default();
                            let color = match r.status {
                                Some(s) if s >= 400 => Color32::LIGHT_RED,
                                _ if r.error.is_some() => Color32::LIGHT_RED,
                                _ => ui.visuals().text_color(),
                            };
                            ui.label(
                                RichText::new(format!(
                                    "{:<6} {:>5} {:>7} {:<10} {}",
                                    r.method,
                                    status,
                                    ms,
                                    r.resource_type,
                                    truncate(&r.url, 200)
                                ))
                                .monospace()
                                .color(color),
                            );
                        }
                    });
            }
            InspectorTab::Console => {
                let Some(page) = self.selected_page.as_ref().and_then(|k| self.model.pages.get(k)) else { return };
                egui::ScrollArea::both()
                    .stick_to_bottom(true)
                    .auto_shrink(false)
                    .show_rows(ui, row_h, page.console.len(), |ui, range| {
                        for c in &page.console[range] {
                            let color = match c.level.as_str() {
                                "error" => Color32::LIGHT_RED,
                                "warning" | "warn" => Color32::GOLD,
                                _ => ui.visuals().text_color(),
                            };
                            ui.label(RichText::new(format!("[{}] {}", c.level, c.text)).monospace().color(color));
                        }
                    });
            }
        }
    }
}

fn test_row(ui: &mut egui::Ui, t: &Test, store: Option<&Store>) {
    let (icon, color) = status_icon(t.status);
    let mut text = format!("{icon} {}", t.title);
    if let Some(d) = t.duration_ms {
        text.push_str(&format!("  {d:.0}ms"));
    }
    let resp = ui.label(RichText::new(text).color(color));
    if let Some(step) = &t.current_step {
        ui.weak(format!("    ↳ {step}"));
    }
    // Only runs (and queries) while hovered.
    resp.on_hover_ui(|ui| {
        ui.label(t.title_path.join(" › "));
        if let Some(f) = &t.file {
            ui.weak(format!("{f}:{}", t.line.unwrap_or(0)));
        }
        if let Some(e) = &t.error {
            ui.separator();
            ui.label(RichText::new(e).monospace().color(Color32::LIGHT_RED));
        }
        if let (Some(store), Some(file)) = (store, &t.file)
            && let Ok(history) = store.test_history(file, &t.title_path, 20)
            && !history.is_empty()
        {
            ui.separator();
            let strip: String = history
                .iter()
                .rev()
                .map(|h| match h.status.as_deref() {
                    Some("passed") => '✔',
                    Some("failed" | "timedOut") => '✘',
                    Some("interrupted") => '⏹',
                    Some("skipped") => '⊘',
                    _ => '·',
                })
                .collect();
            let flaky = strip.contains('✔') && strip.contains('✘');
            ui.label(format!("last {} runs (oldest → newest): {strip}", history.len()));
            if flaky {
                ui.colored_label(Color32::GOLD, "flaky: both passes and failures in recent history");
            }
        }
    });
}

struct PaneBehavior<'a> {
    model: &'a Model,
    selected: &'a mut Option<PageKey>,
    clicked: bool,
}

impl egui_tiles::Behavior<PageKey> for PaneBehavior<'_> {
    fn pane_ui(&mut self, ui: &mut egui::Ui, _tile_id: TileId, key: &mut PageKey) -> UiResponse {
        let Some(page) = self.model.pages.get(key) else { return UiResponse::None };
        let test = self.model.test_for_page(page);
        let (_, status_color) = status_icon(test.and_then(|t| t.status));
        let is_selected = self.selected.as_ref() == Some(key);

        let frame = egui::Frame::new()
            .inner_margin(4.0)
            .stroke(Stroke::new(if is_selected { 2.0 } else { 1.0 }, status_color));
        let mut drag = false;
        let resp = frame
            .show(ui, |ui| {
                // Everything above the frame is one line each and truncated: with
                // dozens of parallel workers panes get small, and the page image
                // is what matters.
                let title = test.map(|t| t.title.as_str()).unwrap_or("(outside a test)");
                let mut meta = page.browser.clone().unwrap_or_default();
                if page.closed_at.is_some() {
                    meta.push_str(" · closed");
                }
                let header = ui
                    .add(
                        egui::Label::new(RichText::new(title).strong().color(status_color))
                            .truncate()
                            .sense(Sense::click_and_drag()),
                    )
                    .on_hover_text(format!("{title}\n{meta}\n{}", page.url));
                drag = header.drag_started();
                if ui.available_height() > 180.0 {
                    ui.add(egui::Label::new(RichText::new(&page.url).weak().small()).truncate());
                }
                draw_frame(ui, page);
            })
            .response
            .interact(Sense::click());

        if resp.clicked() {
            *self.selected = Some(key.clone());
            self.clicked = true;
        }
        if drag { UiResponse::DragStarted } else { UiResponse::None }
    }

    fn tab_title_for_pane(&mut self, key: &PageKey) -> egui::WidgetText {
        self.model
            .pages
            .get(key)
            .and_then(|p| self.model.test_for_page(p))
            .map(|t| t.title.clone())
            .unwrap_or_else(|| key.page_id.clone())
            .into()
    }

    fn simplification_options(&self) -> SimplificationOptions {
        SimplificationOptions {
            // Keep the root grid around even when the last page closes.
            prune_empty_containers: false,
            prune_single_child_containers: false,
            ..Default::default()
        }
    }

    fn gap_width(&self, _style: &egui::Style) -> f32 {
        4.0
    }

    fn is_tab_closable(&self, _tiles: &Tiles<PageKey>, _tile_id: TileId) -> bool {
        false
    }
}

fn draw_frame(ui: &mut egui::Ui, page: &Page) {
    let avail = ui.available_size();
    let Some(tex) = &page.texture else {
        ui.allocate_ui(avail, |ui| ui.centered_and_justified(|ui| ui.spinner()));
        return;
    };
    let [w, h] = tex.size().map(|v| v as f32);
    let scale = (avail.x / w).min(avail.y / h).max(0.01);
    let tint = if page.closed_at.is_some() { Color32::from_gray(120) } else { Color32::WHITE };
    ui.vertical_centered(|ui| {
        ui.add(egui::Image::new(tex).fit_to_exact_size(egui::vec2(w * scale, h * scale)).tint(tint));
    });
}

pub fn status_icon(status: Option<TestStatus>) -> (&'static str, Color32) {
    match status {
        None => ("⏳", Color32::LIGHT_BLUE),
        Some(TestStatus::Passed) => ("✔", Color32::LIGHT_GREEN),
        Some(TestStatus::Failed | TestStatus::TimedOut) => ("✘", Color32::LIGHT_RED),
        Some(TestStatus::Interrupted) => ("⏹", Color32::GOLD),
        Some(TestStatus::Skipped) => ("⊘", Color32::GRAY),
        Some(TestStatus::Unknown) => ("?", Color32::GRAY),
    }
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}
