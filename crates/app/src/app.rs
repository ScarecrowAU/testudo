use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use egui::{Color32, RichText};
use testudo_core::discover::{Suite, discover};
use testudo_core::{Orchestrator, RunHandle, RunId, RunSpec, now_ms};
use testudo_protocol::Event;
use testudo_store::{RunRow, Store};

use crate::bridge::UiMsg;
use crate::replay::Replay;
use crate::view::{PagesView, RunAction};

const HISTORY_LIMIT: usize = 200;

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

    // project + discovered suites
    rt: tokio::runtime::Handle,
    /// Native folder picker in flight; yields the chosen dir (None = cancelled).
    folder_pick: Option<Receiver<Option<PathBuf>>>,
    want_folder_pick: bool,
    project_dir: String,
    suites: Vec<Suite>,
    scanned: Option<PathBuf>,
    args: String,
}

impl TestudoApp {
    pub fn new(
        orchestrator: Orchestrator,
        rx: Receiver<UiMsg>,
        rt: tokio::runtime::Handle,
        default_dir: PathBuf,
        store: Option<Store>,
    ) -> Self {
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
            rt,
            folder_pick: None,
            want_folder_pick: false,
            project_dir: default_dir.display().to_string(),
            suites: Vec::new(),
            scanned: None,
            args: String::new(),
        };
        app.refresh_history();
        app.scan();
        // Test hook (debug builds only): open the folder picker at startup, for
        // exercising the dialog without synthesising input.
        #[cfg(debug_assertions)]
        if std::env::var_os("TESTUDO_DEBUG_OPEN_PICKER").is_some() {
            app.want_folder_pick = true;
        }
        app
    }

    /// Pre-fill the runner args field (CLI `-- args`).
    pub fn set_args(&mut self, args: &[String]) {
        self.args = args.join(" ");
    }

    /// Start the discovered suites matching `filter` (`all`, `playwright`,
    /// `vitest`, or part of a suite name) (`--run` flag).
    pub fn autorun(&mut self, filter: &str) {
        let chosen: Vec<Suite> = self
            .suites
            .iter()
            .filter(|s| s.runnable() && s.matches(filter))
            .cloned()
            .collect();
        if chosen.is_empty() {
            self.error = Some(format!("--run {filter}: no runnable suite matches"));
        }
        for s in &chosen {
            self.start_suite(s);
        }
    }

    /// Open the platform's folder picker (KDE/GNOME via the XDG desktop portal,
    /// Finder on macOS, the shell dialog on Windows), attached to our window.
    /// The dialog is created here on the UI thread (required on macOS) and
    /// awaited on the runtime so the UI keeps painting.
    fn open_folder_picker(&mut self, frame: &eframe::Frame, ctx: &egui::Context) {
        let mut dialog = rfd::AsyncFileDialog::new()
            .set_title("Choose a project directory")
            .set_parent(frame);
        let current = PathBuf::from(expand_tilde(self.project_dir.trim()));
        if current.is_dir() {
            dialog = dialog.set_directory(&current);
        }
        let pick = dialog.pick_folder();
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = ctx.clone();
        self.rt.spawn(async move {
            let chosen = pick.await.map(|h| h.path().to_owned());
            let _ = tx.send(chosen);
            ctx.request_repaint();
        });
        self.folder_pick = Some(rx);
    }

    fn poll_folder_picker(&mut self) {
        let Some(rx) = &self.folder_pick else { return };
        match rx.try_recv() {
            Ok(Some(dir)) => {
                self.folder_pick = None;
                self.project_dir = dir.display().to_string();
                self.scan();
                self.mode = Mode::Live;
            }
            Ok(None) | Err(std::sync::mpsc::TryRecvError::Disconnected) => self.folder_pick = None,
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
    }

    fn scan(&mut self) {
        let dir = PathBuf::from(expand_tilde(self.project_dir.trim()));
        if !dir.is_dir() {
            self.suites.clear();
            self.scanned = None;
            self.error = Some(format!("{} is not a directory", dir.display()));
            return;
        }
        self.suites = discover(&dir);
        self.scanned = Some(dir);
        self.error = None;
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

    /// Open the focus view on a page of the open replay (`--focus`).
    pub fn focus_replay_page(&mut self, page_id: &str) {
        if let Some(replay) = &mut self.replay {
            let key = crate::model::PageKey {
                run_id: replay.run.id.clone(),
                page_id: page_id.to_owned(),
            };
            replay.open_focus(key);
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
                    #[cfg(debug_assertions)]
                    let opened = match &event {
                        Event::PageOpened { page_id, .. } => Some(page_id.clone()),
                        _ => None,
                    };
                    self.live.apply(&run_id, at, event);
                    // Test hook (debug builds only): focus the first page that opens.
                    #[cfg(debug_assertions)]
                    if let Some(page_id) = opened
                        && self.live.focus.is_none()
                        && std::env::var_os("TESTUDO_DEBUG_FOCUS_FIRST").is_some()
                    {
                        self.live.open_live_focus(crate::model::PageKey { run_id: run_id.clone(), page_id });
                    }
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

    fn start_suite(&mut self, suite: &Suite) {
        let args = self.args.split_whitespace().map(str::to_owned).collect();
        let Some(spec) = RunSpec::for_suite(suite, args) else { return };
        match self.orchestrator.start(spec) {
            Ok(handle) => {
                self.live.selected_run = Some(handle.id.clone());
                self.handles.insert(handle.id.clone(), handle);
                self.mode = Mode::Live;
                self.error = None;
            }
            Err(e) => self.error = Some(format!("{}: {e:#}", suite.name)),
        }
    }

    fn suites_ui(&mut self, ui: &mut egui::Ui) {
        let runnable = self.suites.iter().filter(|s| s.runnable()).count();
        let mut start: Vec<Suite> = Vec::new();
        egui::CollapsingHeader::new(RichText::new(format!("Suites ({runnable})")).strong())
            .id_salt("suites")
            .default_open(true)
            .show(ui, |ui| {
                if self.scanned.is_some() && self.suites.is_empty() {
                    ui.weak("No Playwright or Vitest suites found here.");
                }
                for s in &self.suites {
                    ui.horizontal(|ui| {
                        let run = ui.add_enabled(s.runnable(), egui::Button::new("▶").small());
                        if run.clicked() {
                            start.push(s.clone());
                        }
                        let mut text = RichText::new(&s.name);
                        if !s.runnable() {
                            text = text.weak();
                        }
                        let label = ui.add(egui::Label::new(text).truncate());
                        if let Some(v) = s.tool.as_ref().and_then(|t| t.version.as_deref()) {
                            ui.weak(v);
                        }
                        if !s.warnings.is_empty() {
                            ui.colored_label(Color32::GOLD, "⚠");
                        }
                        let mut hover = vec![s.id.clone()];
                        hover.extend(s.problems.iter().map(|p| format!("✘ {p}")));
                        hover.extend(s.warnings.iter().map(|w| format!("⚠ {w}")));
                        label.on_hover_text(hover.join("\n"));
                    });
                }
                if runnable > 1 && ui.button(format!("▶ Run all {runnable}")).clicked() {
                    start.extend(self.suites.iter().filter(|s| s.runnable()).cloned());
                }
            });
        for s in &start {
            self.start_suite(s);
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Testudo");
            ui.separator();
            ui.selectable_value(&mut self.mode, Mode::Live, "● Live");
            ui.selectable_value(&mut self.mode, Mode::History, "🕘 History");
            ui.separator();
            let picking = self.folder_pick.is_some();
            if ui
                .add_enabled(!picking, egui::Button::new("📂"))
                .on_hover_text("Choose a project directory")
                .clicked()
            {
                self.want_folder_pick = true;
            }
            if picking {
                ui.spinner();
            }
            let dir = ui.add(
                egui::TextEdit::singleline(&mut self.project_dir)
                    .hint_text("project directory")
                    .desired_width(380.0),
            );
            let entered = dir.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if entered || ui.button("⟳ Scan").on_hover_text("Find test suites in this directory").clicked() {
                self.scan();
                self.mode = Mode::Live;
            }
            ui.add(
                egui::TextEdit::singleline(&mut self.args)
                    .hint_text("extra runner args")
                    .desired_width(200.0),
            );
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
                self.suites_ui(ui);
                ui.separator();
                ui.heading("Runs");
                if self.live.model.runs.is_empty() {
                    ui.weak("No runs yet. Press ▶ on a suite.");
                }
                egui::ScrollArea::vertical().show(ui, |ui| {
                    action = self.live.runs_ui(ui, Some(&mut self.handles), self.store.as_ref());
                });
            });
        let inspector_max = (ui.available_height() * 0.6).max(120.0);
        egui::Panel::bottom("live-inspector")
            .resizable(true)
            .default_size(220.0)
            // Long runner output must not push the timeline and pages off screen.
            .size_range(80.0..=inspector_max)
            .show(ui, |ui| self.live.inspector_ui(ui, |_, _| {}));
        egui::CentralPanel::default().show(ui, |ui| {
            self.live
                .pages_ui(ui, "Live browser pages appear here as suites open them.", self.store.as_ref());
        });
        if let Some(key) = self.live.focus_request.take() {
            self.live.open_live_focus(key);
        }
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
            let inspector_max = (ui.available_height() * 0.6).max(120.0);
            egui::Panel::bottom("history-inspector")
                .resizable(true)
                .default_size(220.0)
            // Long runner output must not push the timeline and pages off screen.
            .size_range(80.0..=inspector_max)
                .show(ui, |ui| {
                    // Export lives in the tab row: anything drawn after the inspector
                    // would overflow the panel (its lists fill the remaining height).
                    let Replay { view, exporter, .. } = replay;
                    view.inspector_ui(ui, |ui, page| {
                        ui.separator();
                        exporter.ui(ui, page, store);
                    });
                });
            egui::CentralPanel::default().show(ui, |ui| {
                replay.timeline_ui(ui, store);
                ui.separator();
                replay.view.pages_ui(ui, "No browser pages open at this point in the run.", Some(store));
                if let Some(key) = replay.view.focus_request.take() {
                    replay.open_focus(key);
                }
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
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.drain_messages(&ctx);
        self.poll_folder_picker();
        egui::Panel::top("top").show(ui, |ui| self.top_bar(ui));
        if std::mem::take(&mut self.want_folder_pick) {
            self.open_folder_picker(frame, &ctx);
        }
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

fn expand_tilde(p: &str) -> String {
    match (p.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => p.to_owned(),
    }
}
