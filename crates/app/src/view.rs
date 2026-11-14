//! A tiled view of browser pages plus run/test list and inspector, driven purely
//! by events. The live view and the replay view are both a `PagesView`; only the
//! source of events (hub vs. recorded history) and the notion of "now" differ.

use std::collections::HashMap;

use egui::{Color32, RichText, Sense, Stroke};
use egui_tiles::{Container, Grid, SimplificationOptions, Tile, TileId, Tiles, Tree, UiResponse};
use testudo_core::{RunHandle, RunId};
use testudo_protocol::{Event, StdStream, TestStatus};
use testudo_store::Store;

use crate::focus::PageFocus;
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
    output_filter: String,
    hide_repeated_stderr: bool,
    /// Single-page focus (live watch or recording scrubber), replacing the grid.
    pub focus: Option<PageFocus>,
    /// A pane was clicked; the owner opens focus with the right data source.
    pub focus_request: Option<PageKey>,
}

impl PagesView {
    pub fn new() -> Self {
        Self {
            model: Model::new(),
            tree: Tree::new_grid(egui::Id::unique("testudo-pages"), vec![]),
            selected_page: None,
            selected_run: None,
            inspector_tab: InspectorTab::Output,
            keep_closed_pages: true,
            output_filter: String::new(),
            hide_repeated_stderr: true,
            focus: None,
            focus_request: None,
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
        if let Some(page) = self.model.pages.get_mut(&key) {
            page.tile = Some(tile);
        }
        // Placement is decided by `arrange`.
    }

    /// Pages in display order: running first, then failed, then complete; by
    /// name within each group (case-insensitive), page id as a stable tiebreak.
    pub fn ordered_pages(&self) -> Vec<&Page> {
        let mut pages: Vec<&Page> = self.model.pages.values().collect();
        pages.sort_by_cached_key(|p| {
            let state = pane_state(p, self.model.test_for_page(p));
            (state, pane_name(&self.model, p).to_lowercase(), p.key.page_id.clone())
        });
        pages
    }

    /// Keep the root grid's children in `ordered_pages` order. Rebuilds the grid
    /// only when the order actually changed (a page opened, closed, or its test
    /// finished), so steady frames cost one comparison.
    fn arrange(&mut self) {
        let wanted: Vec<TileId> = self.ordered_pages().iter().filter_map(|p| p.tile).collect();
        let current = self
            .tree
            .root
            .and_then(|r| self.tree.tiles.get_container(r))
            .map(|c| c.children_vec());
        if current.as_ref() == Some(&wanted) {
            return;
        }
        let grid = Tile::Container(Container::Grid(Grid::new(wanted)));
        match self.tree.root {
            Some(root) if matches!(self.tree.tiles.get(root), Some(Tile::Container(_))) => {
                self.tree.tiles.insert(root, grid)
            }
            _ => self.tree.root = Some(self.tree.tiles.insert_new(grid)),
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

    /// Focus a page of this view's own (live) model.
    pub fn open_live_focus(&mut self, key: PageKey) {
        if let Some(page) = self.model.pages.get(&key) {
            self.focus = Some(PageFocus::live(page));
            self.selected_page = Some(key);
            self.inspector_tab = InspectorTab::Network;
        }
    }

    pub fn open_focus(&mut self, focus: PageFocus) {
        self.selected_page = Some(focus.key.clone());
        self.inspector_tab = InspectorTab::Network;
        self.focus = Some(focus);
    }

    pub fn pages_ui(&mut self, ui: &mut egui::Ui, empty_hint: &str, store: Option<&Store>) {
        if let Some(focus) = &mut self.focus {
            focus.ui(ui, &self.model, store);
            if focus.close {
                self.focus = None;
            }
            return;
        }
        if self.model.pages.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.weak(empty_hint);
            });
            return;
        }
        self.arrange();
        let mut behavior = PaneBehavior {
            model: &self.model,
            selected: &mut self.selected_page,
            clicked: false,
            open: None,
        };
        self.tree.ui(&mut behavior, ui);
        if let Some(key) = behavior.open.take() {
            self.focus_request = Some(key);
        }
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
                (_, Some(p), _) if self.focus.is_some() => ui.weak(format!(
                    "page {} · synced to {:.2}s",
                    p.page_id,
                    self.focus.as_ref().map_or(0.0, |f| {
                        let opened = self.model.pages.get(p).map_or(f.time(), |pg| pg.opened_at);
                        (f.time() - opened) / 1000.0
                    })
                )),
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

                // Runners and test environments write plenty of non-errors to stderr
                // (jsdom "Not implemented", Node experimental warnings, progress), so
                // colour by content, and optionally fold stderr lines that repeat a
                // lot into a summary instead of letting them bury the test output.
                let mut repeats: HashMap<&str, usize> = HashMap::new();
                for (s, l) in &run.stdio {
                    if *s == StdStream::Stderr {
                        *repeats.entry(l.as_str()).or_default() += 1;
                    }
                }
                let noisy = |l: &str| repeats.get(l).is_some_and(|n| *n > REPEAT_THRESHOLD);
                let filter = self.output_filter.to_lowercase();
                let text = ui.visuals().text_color();
                let lines: Vec<(Color32, &str)> = run
                    .stdio
                    .iter()
                    .filter(|(s, l)| !(self.hide_repeated_stderr && *s == StdStream::Stderr && noisy(l)))
                    .map(|(s, l)| {
                        let c = if looks_like_failure(l) {
                            Color32::LIGHT_RED
                        } else if *s == StdStream::Stderr {
                            text.gamma_multiply(0.75)
                        } else {
                            text
                        };
                        (c, l.as_str())
                    })
                    .chain(run.adapter_logs.iter().map(|l| (Color32::GOLD, l.as_str())))
                    .filter(|(_, l)| filter.is_empty() || l.to_lowercase().contains(&filter))
                    .collect();

                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.output_filter)
                            .hint_text("filter output")
                            .desired_width(220.0),
                    );
                    ui.checkbox(&mut self.hide_repeated_stderr, "Hide repeated stderr");
                    if self.hide_repeated_stderr {
                        let mut folded: Vec<(&str, usize)> =
                            repeats.iter().filter(|(_, n)| **n > REPEAT_THRESHOLD).map(|(l, n)| (*l, *n)).collect();
                        folded.sort_by_key(|f| std::cmp::Reverse(f.1));
                        if !folded.is_empty() {
                            let total: usize = folded.iter().map(|(_, n)| n).sum();
                            let detail = folded
                                .iter()
                                .map(|(l, n)| format!("{n} × {l}"))
                                .collect::<Vec<_>>()
                                .join("\n");
                            ui.weak(format!("{total} repeated stderr lines hidden"))
                                .on_hover_text(detail);
                        }
                    }
                });
                egui::ScrollArea::both()
                    .stick_to_bottom(true)
                    .auto_shrink(false)
                    .show_rows(ui, row_h, lines.len(), |ui, range| {
                        for (c, l) in &lines[range] {
                            ui.label(RichText::new(*l).monospace().color(*c));
                        }
                    });
            }
            InspectorTab::Network if self.focus.is_some() => {
                self.focus.as_mut().unwrap().network_ui(ui, &self.model);
            }
            InspectorTab::Console if self.focus.is_some() => {
                self.focus.as_mut().unwrap().console_ui(ui, &self.model);
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

/// Stderr lines repeated more than this many times in a run count as noise.
const REPEAT_THRESHOLD: usize = 3;

/// Heuristic for lines worth highlighting, whichever stream they came on.
fn looks_like_failure(line: &str) -> bool {
    let l = line.to_lowercase();
    let summary_ok = l.contains(" 0 failed") || l.contains("(0 failed") || l.contains("0 errors");
    !summary_ok
        && (l.contains("error")
            || l.contains("failed")
            || l.contains("panicked")
            || l.contains("exception")
            || l.contains('✘')
            || l.trim_start().starts_with('×')
            || l.contains("timed out")
            || l.contains("timeout of"))
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
    open: Option<PageKey>,
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
                ui.add(egui::Label::new(RichText::new(title).strong().color(status_color)).truncate())
                    .on_hover_text(format!("{title}\n{meta}\n{}", page.url));
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
            self.open = Some(key.clone());
        }
        resp.on_hover_text("click to open: watch live, or scrub the recording");
        // No drag-to-rearrange: panes are ordered automatically.
        UiResponse::None
    }

    fn tab_title_for_pane(&mut self, key: &PageKey) -> egui::WidgetText {
        self.model
            .pages
            .get(key)
            .map(|p| pane_name(self.model, p).to_owned())
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

/// Pane ordering groups.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PaneState {
    /// Page still open.
    Running,
    /// Page closed and its test failed or timed out.
    Failed,
    /// Page closed otherwise (passed, skipped, interrupted, or outside a test).
    Complete,
}

pub fn pane_state(page: &Page, test: Option<&Test>) -> PaneState {
    if page.closed_at.is_none() {
        return PaneState::Running;
    }
    match test.and_then(|t| t.status) {
        Some(TestStatus::Failed | TestStatus::TimedOut) => PaneState::Failed,
        _ => PaneState::Complete,
    }
}

/// What a pane is called: its test's title, else the page id.
pub fn pane_name<'a>(model: &'a Model, page: &'a Page) -> &'a str {
    model
        .test_for_page(page)
        .map(|t| t.title.as_str())
        .unwrap_or(page.key.page_id.as_str())
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

#[cfg(test)]
mod tests {
    use super::{PagesView, looks_like_failure};
    use testudo_protocol::{Event, TestStatus};

    #[test]
    fn panes_order_by_state_then_name() {
        let mut v = PagesView::new();
        let begin = |id: &str, title: &str| Event::TestBegin {
            test_id: id.into(),
            title: title.into(),
            title_path: vec![],
            file: None,
            line: None,
            project: None,
            worker_index: None,
        };
        let open = |page: &str, test: &str| Event::PageOpened {
            page_id: page.into(),
            test_id: Some(test.into()),
            worker_index: None,
            browser: None,
            url: None,
        };
        let end = |id: &str, status| Event::TestEnd {
            test_id: id.into(),
            status,
            duration_ms: None,
            error: None,
        };
        let close = |page: &str| Event::PageClosed { page_id: page.into() };
        let events = [
            begin("t1", "zeta passes"),
            begin("t2", "alpha fails"),
            begin("t3", "Beta running"),
            begin("t4", "alpha passes"),
            begin("t5", "delta running"),
            begin("t6", "beta times out"),
            open("p1", "t1"),
            open("p2", "t2"),
            open("p3", "t3"),
            open("p4", "t4"),
            open("p5", "t5"),
            open("p6", "t6"),
            end("t1", TestStatus::Passed),
            close("p1"),
            end("t2", TestStatus::Failed),
            close("p2"),
            end("t4", TestStatus::Passed),
            close("p4"),
            end("t6", TestStatus::TimedOut),
            close("p6"),
        ];
        for (i, e) in events.into_iter().enumerate() {
            v.apply("r1", i as f64, e);
        }
        let order: Vec<&str> = v.ordered_pages().iter().map(|p| p.key.page_id.as_str()).collect();
        // running: Beta, delta | failed: alpha fails, beta times out | complete: alpha passes, zeta
        assert_eq!(order, ["p3", "p5", "p2", "p6", "p4", "p1"]);
    }

    #[test]
    fn failure_heuristic() {
        assert!(!looks_like_failure("Not implemented: Window's scrollTo() method"));
        assert!(!looks_like_failure("(node:1) ExperimentalWarning: localStorage is not available"));
        assert!(!looks_like_failure("      Tests  632 passed (632)"));
        assert!(!looks_like_failure("  11 passed, 0 failed"));
        assert!(looks_like_failure("  1 failed"));
        assert!(looks_like_failure("Error: page.waitForURL: Test timeout of 30000ms exceeded."));
        assert!(looks_like_failure(" × fails on purpose 117ms"));
        assert!(looks_like_failure("AssertionError: expected 2 to be 3"));
    }
}
