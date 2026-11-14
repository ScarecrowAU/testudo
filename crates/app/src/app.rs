use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use egui::{Color32, RichText};
use testudo_core::{Orchestrator, RunHandle, RunId, RunSpec, RunnerKind, now_ms};
use testudo_protocol::Event;
use testudo_store::{RunRow, Store};

use crate::bridge::UiMsg;
use crate::replay::Replay;
use crate::view::{PagesView, RunAction};

const HISTORY_LIMIT: usize = 200;

#[derive(Clone, Copy, PartialEq, Eq)]
enum KindChoice {
    Playwright,
    Vitest,
    Command,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Live,
    History,
}

pub struct TestudoApp {
    orchestrator: Orchestrator,
    rx: Receiver<UiMsg>,
    mode: Mode,

    live: PagesView,
    handles: HashMap<RunId, RunHandle>,

    store: Option<Store>,
    history: Vec<RunRow>,
    refresh_history_at: Option<Instant>,
    replay: Option<Replay>,
    error: Option<String>,

    // new-run form
    kind: KindChoice,
    project_dir: String,
    program: String,
    args: String,
}

impl TestudoApp {
    pub fn new(orchestrator: Orchestrator, rx: Receiver<UiMsg>, default_dir: PathBuf, store: Option<Store>) -> Self {
        let mut app = Self {
            orchestrator,
            rx,
            mode: Mode::Live,
            live: PagesView::new(),
            handles: HashMap::new(),
            store,
            history: Vec::new(),
            refresh_history_at: None,
            replay: None,
            error: None,
            kind: KindChoice::Playwright,
            project_dir: default_dir.display().to_string(),
            program: String::new(),
            args: String::new(),
        };
        app.refresh_history();
        app
    }

    /// Pre-fill the runner args field (CLI `-- args`).
    pub fn set_args(&mut self, args: &[String]) {
        self.args = args.join(" ");
    }

    /// Start a run immediately with the default project dir (`--run` flag).
    pub fn autorun(&mut self, kind: &str) {
        self.kind = match kind {
            "vitest" => KindChoice::Vitest,
            _ => KindChoice::Playwright,
        };
        self.start_run();
    }

    /// Open the history view on a run (`--replay` flag, Replay buttons).
    pub fn open_replay(&mut self, run_id: &str) {
        let Some(store) = &self.store else {
            self.error = Some("history unavailable: store failed to open".into());
            return;
        };
        match Replay::load(store, run_id) {
            Ok(r) => {
                self.replay = Some(r);
                self.mode = Mode::History;
                self.error = None;
            }
            Err(e) => self.error = Some(format!("{e:#}")),
        }
    }

    /// Seek the open replay to `offset_ms` after the run started.
    pub fn seek_replay(&mut self, ctx: &egui::Context, offset_ms: f64) {
        if let (Some(replay), Some(store)) = (&mut self.replay, &self.store) {
            let t = replay.start + offset_ms;
            replay.seek(t, ctx, store);
        }
    }

    fn refresh_history(&mut self) {
        self.refresh_history_at = None;
        if let Some(store) = &self.store {
            match store.list_runs(HISTORY_LIMIT) {
                Ok(rows) => self.history = rows,
                Err(e) => self.error = Some(format!("reading history: {e:#}")),
            }
        }
    }

    fn drain_messages(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                UiMsg::Event { run_id, at, event } => {
                    if let Event::ProcessExited { .. } = &event {
                        self.handles.remove(&run_id);
                        // Give the recorder a moment to commit the final status.
                        self.refresh_history_at = Some(Instant::now() + Duration::from_millis(500));
                    }
                    self.live.apply(&run_id, at, event);
                }
                UiMsg::Frame { key, at, image } => self.live.model.apply_frame(ctx, &key, at, image),
            }
        }
        match self.refresh_history_at {
            Some(t) if Instant::now() >= t => self.refresh_history(),
            Some(_) => ctx.request_repaint_after(Duration::from_millis(250)),
            None => {}
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
            cwd.canonicalize()
                .ok()
                .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
                .unwrap_or_default()
        );
        let spec = RunSpec {
            label,
            kind,
            cwd,
            args: self.args.split_whitespace().map(str::to_owned).collect(),
        };
        match self.orchestrator.start(spec) {
            Ok(handle) => {
                self.live.selected_run = Some(handle.id.clone());
                self.handles.insert(handle.id.clone(), handle);
                self.mode = Mode::Live;
                self.error = None;
            }
            Err(e) => self.error = Some(format!("{e:#}")),
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Testudo");
            ui.separator();
            ui.selectable_value(&mut self.mode, Mode::Live, "● Live");
            ui.selectable_value(&mut self.mode, Mode::History, "🕘 History");
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
            if self.mode == Mode::Live {
                ui.separator();
                ui.checkbox(&mut self.live.keep_closed_pages, "Keep closed pages");
            }
        });
        if let Some(err) = &self.error {
            ui.colored_label(Color32::LIGHT_RED, err);
        }
    }

    fn live_ui(&mut self, ui: &mut egui::Ui) {
        if self.live.reap(now_ms()) {
            ui.ctx().request_repaint_after(Duration::from_millis(500));
        }
        let mut action = None;
        egui::Panel::left("live-runs")
            .resizable(true)
            .default_size(320.0)
            .show(ui, |ui| {
                ui.heading("Runs");
                if self.live.model.runs.is_empty() {
                    ui.weak("No runs yet. Pick a runner and project above.");
                }
                egui::ScrollArea::vertical().show(ui, |ui| {
                    action = self.live.runs_ui(ui, Some(&mut self.handles), self.store.as_ref());
                });
            });
        egui::Panel::bottom("live-inspector")
            .resizable(true)
            .default_size(220.0)
            .show(ui, |ui| self.live.inspector_ui(ui, |_, _| {}));
        egui::CentralPanel::default().show(ui, |ui| {
            self.live.pages_ui(ui, "Live browser pages appear here as suites open them.");
        });
        if let Some(RunAction::OpenReplay(id)) = action {
            self.open_replay(&id);
        }
    }

    fn history_ui(&mut self, ui: &mut egui::Ui) {
        let Some(store) = &self.store else {
            ui.centered_and_justified(|ui| ui.colored_label(Color32::LIGHT_RED, "History store unavailable."));
            return;
        };
        if let Some(replay) = &mut self.replay {
            replay.tick(ui.ctx(), store);
        }

        let mut open = None;
        let mut refresh = false;
        egui::Panel::left("history")
            .resizable(true)
            .default_size(340.0)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("History");
                    if ui.small_button("⟳").on_hover_text("Refresh").clicked() {
                        refresh = true;
                    }
                });
                ui.weak(store.dir().display().to_string());
                ui.separator();
                let list_height = if self.replay.is_some() { ui.available_height() * 0.4 } else { f32::INFINITY };
                egui::ScrollArea::vertical()
                    .id_salt("history-list")
                    .max_height(list_height)
                    .show(ui, |ui| {
                        if self.history.is_empty() {
                            ui.weak("No recorded runs yet.");
                        }
                        for r in &self.history {
                            let selected = self.replay.as_ref().is_some_and(|rp| rp.run.id == r.id);
                            let color = match r.status.as_str() {
                                "passed" => Color32::LIGHT_GREEN,
                                "running" => Color32::LIGHT_BLUE,
                                _ => Color32::LIGHT_RED,
                            };
                            let resp = ui.selectable_label(
                                selected,
                                RichText::new(format!(
                                    "{}  {}  ✔{} ✘{}  {}",
                                    fmt_time(r.started_at),
                                    r.status,
                                    r.passed,
                                    r.failed,
                                    r.label
                                ))
                                .color(color),
                            );
                            if resp.clicked() {
                                open = Some(r.id.clone());
                            }
                            resp.on_hover_text(format!("{}\n{}", r.id, r.cwd));
                        }
                    });
                if let Some(replay) = &mut self.replay {
                    ui.separator();
                    egui::ScrollArea::vertical().id_salt("replay-tests").show(ui, |ui| {
                        replay.view.runs_ui(ui, None, Some(store));
                    });
                }
            });

        if let Some(replay) = &mut self.replay {
            egui::Panel::bottom("history-inspector")
                .resizable(true)
                .default_size(220.0)
                .show(ui, |ui| {
                    let mut selected_page = None;
                    replay.view.inspector_ui(ui, |_, page| selected_page = page.cloned());
                    ui.horizontal(|ui| {
                        replay.export_button(ui, selected_page.as_ref(), store);
                        replay.status_ui(ui);
                    });
                });
            egui::CentralPanel::default().show(ui, |ui| {
                replay.timeline_ui(ui, store);
                ui.separator();
                replay.view.pages_ui(ui, "No browser pages open at this point in the run.");
            });
        } else {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.centered_and_justified(|ui| ui.weak("Select a run to replay it."));
            });
        }

        if refresh {
            self.refresh_history();
        }
        if let Some(id) = open {
            self.open_replay(&id);
        }
    }
}

impl eframe::App for TestudoApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.drain_messages(&ctx);
        egui::Panel::top("top").show(ui, |ui| self.top_bar(ui));
        match self.mode {
            Mode::Live => self.live_ui(ui),
            Mode::History => self.history_ui(ui),
        }
    }
}

/// Local wall-clock `MM-DD HH:MM:SS` without a date-time dependency.
fn fmt_time(ms: f64) -> String {
    let t = (ms / 1000.0) as i64 + local_offset_secs();
    let (days, rem) = (t.div_euclid(86_400), t.rem_euclid(86_400));
    // Civil-from-days (Howard Hinnant), month/day only.
    let doe = (days + 719_468).rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{m:02}-{d:02} {:02}:{:02}:{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

#[cfg(unix)]
fn local_offset_secs() -> i64 {
    // SAFETY: localtime_r writes only into the provided tm.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        tm.tm_gmtoff
    }
}

#[cfg(not(unix))]
fn local_offset_secs() -> i64 {
    0
}
