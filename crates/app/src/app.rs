use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use egui::{Color32, RichText, Sense, Stroke};
use egui_tiles::{SimplificationOptions, Tile, TileId, Tiles, Tree, UiResponse};
use testudo_core::{Orchestrator, RunSpec, RunnerKind};
use testudo_protocol::{StdStream, TestStatus};

use crate::bridge::UiMsg;
use crate::model::{Effect, Model, Page, PageKey};

/// How long a closed page's last frame stays on screen before its pane is removed.
const CLOSED_PAGE_LINGER: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, PartialEq, Eq)]
enum KindChoice {
    Playwright,
    Vitest,
    Command,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InspectorTab {
    Network,
    Console,
    Output,
}

pub struct TestudoApp {
    orchestrator: Orchestrator,
    rx: Receiver<UiMsg>,
    model: Model,
    tree: Tree<PageKey>,

    // new-run form
    kind: KindChoice,
    project_dir: String,
    program: String,
    args: String,
    start_error: Option<String>,

    keep_closed_pages: bool,
    selected_page: Option<PageKey>,
    selected_run: Option<String>,
    inspector_tab: InspectorTab,
}

impl TestudoApp {
    pub fn new(orchestrator: Orchestrator, rx: Receiver<UiMsg>, default_dir: PathBuf) -> Self {
        Self {
            orchestrator,
            rx,
            model: Model::new(),
            tree: Tree::new_grid(egui::Id::unique("testudo-pages"), vec![]),
            kind: KindChoice::Playwright,
            project_dir: default_dir.display().to_string(),
            program: String::new(),
            args: String::new(),
            start_error: None,
            keep_closed_pages: false,
            selected_page: None,
            selected_run: None,
            inspector_tab: InspectorTab::Output,
        }
    }

    /// Start a run immediately with the default project dir (`--run` flag).
    pub fn autorun(&mut self, kind: &str) {
        self.kind = match kind {
            "vitest" => KindChoice::Vitest,
            _ => KindChoice::Playwright,
        };
        self.start_run();
    }

    fn drain_messages(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                UiMsg::Event { run_id, event } => {
                    if let Effect::PageOpened(key) = self.model.apply(&run_id, event) {
                        self.add_pane(key);
                    }
                }
                UiMsg::Frame { key, ts, image } => self.model.apply_frame(ctx, &key, ts, image),
            }
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

    fn reap_closed_pages(&mut self, ctx: &egui::Context) {
        if self.keep_closed_pages {
            return;
        }
        let expired: Vec<PageKey> = self
            .model
            .pages
            .values()
            .filter(|p| p.closed_at.is_some_and(|t| t.elapsed() >= CLOSED_PAGE_LINGER))
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
        if self.model.pages.values().any(|p| p.closed_at.is_some()) {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }

    fn start_run(&mut self) {
        let kind = match self.kind {
            KindChoice::Playwright => RunnerKind::Playwright,
            KindChoice::Vitest => RunnerKind::Vitest,
            KindChoice::Command => RunnerKind::Command {
                program: self.program.trim().to_owned(),
            },
        };
        let cwd = PathBuf::from(self.project_dir.trim());
        let label = format!(
            "{} · {}",
            kind.name(),
            cwd.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
        );
        let spec = RunSpec {
            label: label.clone(),
            kind,
            cwd,
            args: self.args.split_whitespace().map(str::to_owned).collect(),
        };
        match self.orchestrator.start(spec) {
            Ok(handle) => {
                self.selected_run = Some(handle.id.clone());
                self.model.add_run(handle, label);
                self.start_error = None;
            }
            Err(e) => self.start_error = Some(format!("{e:#}")),
        }
    }

    // --- panels ----------------------------------------------------------------

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Testudo");
            ui.separator();
            egui::ComboBox::from_id_salt("kind")
                .selected_text(match self.kind {
                    KindChoice::Playwright => "Playwright",
                    KindChoice::Vitest => "Vitest / Storybook",
                    KindChoice::Command => "Command",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.kind, KindChoice::Playwright, "Playwright");
                    ui.selectable_value(&mut self.kind, KindChoice::Vitest, "Vitest / Storybook");
                    ui.selectable_value(&mut self.kind, KindChoice::Command, "Command");
                });
            if self.kind == KindChoice::Command {
                ui.add(egui::TextEdit::singleline(&mut self.program).hint_text("program").desired_width(120.0));
            }
            ui.add(egui::TextEdit::singleline(&mut self.project_dir).hint_text("project dir").desired_width(320.0));
            ui.add(egui::TextEdit::singleline(&mut self.args).hint_text("args").desired_width(200.0));
            if ui.button("▶ Run").clicked() {
                self.start_run();
            }
            ui.separator();
            ui.checkbox(&mut self.keep_closed_pages, "Keep closed pages");
        });
        if let Some(err) = &self.start_error {
            ui.colored_label(Color32::LIGHT_RED, err);
        }
    }

    fn runs_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Runs");
        if self.model.runs.is_empty() {
            ui.weak("No runs yet. Pick a runner and project above.");
        }
        egui::ScrollArea::vertical().show(ui, |ui| {
            for run in self.model.runs.iter_mut().rev() {
                let (passed, failed, running) = run.counts();
                let status = match (&run.exit, run.run_end.as_deref()) {
                    (None, _) => RichText::new("running").color(Color32::LIGHT_BLUE),
                    (Some(_), Some("passed")) => RichText::new("passed").color(Color32::LIGHT_GREEN),
                    (Some(Some(0)), _) => RichText::new("passed").color(Color32::LIGHT_GREEN),
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
                    if run.is_running()
                        && let Some(h) = run.handle.as_mut()
                        && ui.small_button("■ Stop").clicked()
                    {
                        h.cancel();
                    }
                });
                let total = run.total_tests.map(|t| format!("/{t}")).unwrap_or_default();
                ui.weak(format!("✔ {passed}  ✘ {failed}  ⏳ {running}  ({}{total})", run.tests.len()));
                egui::CollapsingHeader::new("Tests")
                    .id_salt(&run.id)
                    .default_open(true)
                    .show(ui, |ui| {
                        for t in &run.tests {
                            let (icon, color) = status_icon(t.status);
                            let mut text = format!("{icon} {}", t.title);
                            if let Some(d) = t.duration_ms {
                                text.push_str(&format!("  {:.0}ms", d));
                            }
                            let resp = ui.label(RichText::new(text).color(color));
                            if let Some(step) = &t.current_step {
                                ui.weak(format!("    ↳ {step}"));
                            }
                            let hover = [
                                Some(t.title_path.join(" › ")),
                                t.file.as_ref().map(|f| format!("{f}:{}", t.line.unwrap_or(0))),
                                t.error.clone(),
                            ]
                            .into_iter()
                            .flatten()
                            .collect::<Vec<_>>()
                            .join("\n\n");
                            resp.on_hover_text(hover);
                        }
                    });
                ui.separator();
            }
        });
    }

    fn inspector(&mut self, ui: &mut egui::Ui) {
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

impl eframe::App for TestudoApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.drain_messages(&ctx);
        self.reap_closed_pages(&ctx);

        egui::Panel::top("top").show(ui, |ui| self.top_bar(ui));
        egui::Panel::left("runs")
            .resizable(true)
            .default_size(320.0)
            .show(ui, |ui| self.runs_panel(ui));
        egui::Panel::bottom("inspector")
            .resizable(true)
            .default_size(220.0)
            .show(ui, |ui| self.inspector(ui));
        egui::CentralPanel::default().show(ui, |ui| {
            if self.model.pages.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.weak("Live browser pages appear here as suites open them.");
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
        });
    }
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
                let header = ui
                    .horizontal(|ui| {
                        let title = test.map(|t| t.title.as_str()).unwrap_or("(outside a test)");
                        ui.label(RichText::new(title).strong().color(status_color));
                        ui.weak(page.browser.as_deref().unwrap_or(""));
                        if page.closed_at.is_some() {
                            ui.weak("closed");
                        }
                    })
                    .response
                    .interact(Sense::click_and_drag());
                drag = header.drag_started();
                ui.weak(truncate(&page.url, 120));
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

fn status_icon(status: Option<TestStatus>) -> (&'static str, Color32) {
    match status {
        None => ("⏳", Color32::LIGHT_BLUE),
        Some(TestStatus::Passed) => ("✔", Color32::LIGHT_GREEN),
        Some(TestStatus::Failed | TestStatus::TimedOut | TestStatus::Interrupted) => ("✘", Color32::LIGHT_RED),
        Some(TestStatus::Skipped) => ("⊘", Color32::GRAY),
        Some(TestStatus::Unknown) => ("?", Color32::GRAY),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}
