//! The root view. Rendering only: state and behaviour live in `state.rs`.

use std::cell::Cell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::channel::mpsc::UnboundedReceiver;
use gpui_kit::component::button::{Button, ButtonRounded, ButtonVariants};
use gpui_kit::component::Icon;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::WindowExt;
use gpui_kit::component::{ActiveTheme, Disableable, Selectable, Sizable, h_flex, h_resizable, resizable_panel, v_flex, v_resizable};
use gpui_kit::prelude::*;
use gpui_kit::*;
use testudo_core::discover::{Suite, discover};
use testudo_core::settings::{DEFAULT_FRAME_MAX, FRAME_SIZES, Settings};
use testudo_core::{Orchestrator, RunHandle, RunId, RunSpec, now_ms};
use testudo_protocol::Event;
use testudo_store::{RunRow, Store};

use crate::bridge::UiMsg;
use crate::logic::{rerun_args, test_order, 
    REPEAT_THRESHOLD, RowState, Tone, expand_tilde, fmt_ago, fmt_dur, fmt_secs, fmt_time, status_label, summary_line,
    url_path, looks_like_failure, ordered_pages, pane_name,
    row_state, status_icon, truncate,
};
use crate::model::{Model, Page, PageKey, Source};
use crate::state::{InspectorTab, PageFocus, PagesView, Replay, Retired, StreamFilter};

mod focus;
mod settings;

const HISTORY_LIMIT: usize = 200;
/// How many recent runs the History summary covers.
const SUMMARY_RUNS: usize = 50;
/// Runs shown in the History sparkline.
const SPARK_RUNS: usize = 24;
/// One wheel notch (~50px) moves this fraction of the page's duration.
const WHEEL_FRACTION_PER_NOTCH: f64 = 0.02;
/// Shift+wheel: pixels of scroll per frame step.
const WHEEL_PX_PER_FRAME: f32 = 25.0;

actions!(testudo, [PrevFrame, NextFrame, TogglePlay, CloseFocus]);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("left", PrevFrame, Some("PageFocus")),
        KeyBinding::new("right", NextFrame, Some("PageFocus")),
        KeyBinding::new("space", TogglePlay, Some("PageFocus")),
        KeyBinding::new("escape", CloseFocus, Some("PageFocus")),
    ]);
}

/// Everything main hands the window at startup.
#[derive(Default)]
pub struct Startup {
    pub project_dir: Option<PathBuf>,
    pub autorun: Option<String>,
    pub replay: Option<String>,
    pub replay_at: Option<f64>,
    pub focus_page: Option<String>,
    pub runner_args: Vec<String>,
    pub orchestrator: Option<Orchestrator>,
    pub rt: Option<tokio::runtime::Handle>,
    pub store: Option<Rc<Store>>,
    pub ui_rx: Option<UnboundedReceiver<UiMsg>>,
    pub settings: Settings,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Live,
    History,
}

/// Which `PagesView` a widget belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Which {
    Live,
    Replay,
}

pub struct TestudoApp {
    orchestrator: Orchestrator,
    /// User settings (settings menu), saved on change.
    settings: Settings,
    store: Option<Rc<Store>>,
    mode: Mode,

    live: PagesView,
    handles: HashMap<RunId, RunHandle>,
    /// A run started by "Re-run test": its first page opens in the focus view.
    follow_run: Option<RunId>,
    /// Runs from a project we switched away from: still shutting down, their
    /// late events and frames are ignored (the recorder still keeps them).
    dropped_runs: std::collections::HashSet<RunId>,
    history: Vec<RunRow>,
    /// Tests that both passed and failed in recent runs.
    flaky: Vec<testudo_store::FlakyTest>,
    replay: Option<Replay>,
    error: Option<String>,

    project_input: Entity<InputState>,
    args_input: Entity<InputState>,
    filter_input: Entity<InputState>,
    net_search: Entity<InputState>,
    suites: Vec<Suite>,
    scanned: Option<PathBuf>,
    picking: bool,

    /// Frames replaced since the last render; freed with `drop_image` there.
    retired: Retired,
    last_render: Instant,
    perf: crate::perf::Perf,
    /// Scroll position of the run list (live sidebar / History tests).
    run_scroll: UniformListScrollHandle,
    #[cfg(debug_assertions)]
    started_at: Instant,
    focus_handle: FocusHandle,
    focus_scrub: Rc<Cell<Bounds<Pixels>>>,
    /// Size of the comparison area at the last layout, and whether it was
    /// laid out stacked: decides side by side vs stacked (see focus.rs).
    compare_area: Rc<Cell<(Bounds<Pixels>, bool)>>,
    timeline_scrub: Rc<Cell<Bounds<Pixels>>>,
    /// Test span under the mouse on the replay timeline.
    timeline_hover: Option<usize>,
    net_scroll: UniformListScrollHandle,
    console_scroll: UniformListScrollHandle,
    steps_scroll: UniformListScrollHandle,
    output_scroll: UniformListScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl TestudoApp {
    pub fn new(mut s: Startup, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let dir = s.project_dir.clone().unwrap_or_default();
        let project_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("project directory")
                .default_value(dir.display().to_string())
        });
        let args_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("extra runner args")
                .default_value(s.runner_args.join(" "))
        });
        let filter_input = cx.new(|cx| InputState::new(window, cx).placeholder("filter output"));
        let net_search = cx.new(|cx| InputState::new(window, cx).placeholder("search URLs"));
        let subscriptions = vec![
            cx.subscribe_in(&project_input, window, |this, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::PressEnter { .. }) {
                    this.scan(cx);
                    this.mode = Mode::Live;
                }
            }),
            cx.subscribe_in(&filter_input, window, |_, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::Change) {
                    cx.notify();
                }
            }),
            cx.subscribe_in(&net_search, window, |_, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::Change) {
                    cx.notify();
                }
            }),
        ];

        // Pump hub messages into the view in batches.
        if let Some(mut rx) = s.ui_rx.take() {
            cx.spawn(async move |this, cx| {
                while let Some(first) = rx.next().await {
                    let mut batch = vec![first];
                    while batch.len() < 4096 {
                        match rx.try_recv() {
                            Ok(m) => batch.push(m),
                            Err(_) => break,
                        }
                    }
                    let alive = this.update(cx, |app, cx| {
                        for m in batch {
                            app.on_msg(m, cx);
                        }
                        cx.notify();
                    });
                    if alive.is_err() {
                        break;
                    }
                }
            })
            .detach();
        }

        let mut app = Self {
            orchestrator: s.orchestrator.take().expect("orchestrator"),
            settings: s.settings.clone(),
            store: s.store.take(),
            mode: Mode::Live,
            live: PagesView::new(),
            handles: HashMap::new(),
            dropped_runs: Default::default(),
            follow_run: None,
            history: Vec::new(),
            flaky: Vec::new(),
            replay: None,
            error: None,
            project_input,
            args_input,
            filter_input,
            net_search,
            suites: Vec::new(),
            scanned: None,
            picking: false,
            retired: Vec::new(),
            last_render: Instant::now(),
            perf: Default::default(),
            run_scroll: UniformListScrollHandle::new(),
            #[cfg(debug_assertions)]
            started_at: Instant::now(),
            focus_handle: cx.focus_handle(),
            focus_scrub: Rc::new(Cell::new(Bounds::default())),
            compare_area: Rc::new(Cell::new((Bounds::default(), false))),
            timeline_scrub: Rc::new(Cell::new(Bounds::default())),
            timeline_hover: None,
            net_scroll: UniformListScrollHandle::new(),
            console_scroll: UniformListScrollHandle::new(),
            steps_scroll: UniformListScrollHandle::new(),
            output_scroll: UniformListScrollHandle::new(),
            _subscriptions: subscriptions,
        };
        let _ = s.rt.take();
        app.refresh_history();
        app.scan(cx);
        if let Some(filter) = s.autorun.as_deref() {
            app.autorun(filter, cx);
        }
        if let Some(run) = s.replay.as_deref() {
            app.open_replay(run);
            if let (Some(secs), Some(replay), Some(store)) = (s.replay_at, app.replay.as_mut(), app.store.clone()) {
                let t = replay.start + secs * 1000.0;
                replay.seek(t, &store, &mut app.retired);
            }
            // Test hook (debug builds only): open every failing test's error.
            #[cfg(debug_assertions)]
            if std::env::var_os("TESTUDO_DEBUG_EXPAND_ERRORS").is_some()
                && let Some(replay) = app.replay.as_mut()
            {
                let v = &mut replay.view;
                let failing: Vec<String> =
                    v.model.runs.iter().flat_map(|r| &r.tests).filter(|t| t.error.is_some()).map(|t| t.id.clone()).collect();
                v.expanded_tests.extend(failing);
            }
            if let (Some(page), Some(replay)) = (s.focus_page.as_deref(), app.replay.as_mut()) {
                replay.open_focus(PageKey {
                    run_id: replay.run.id.clone(),
                    page_id: page.to_owned(),
                });
                // Test hook (debug builds only): open the focus on a given tab.
                #[cfg(debug_assertions)]
                if let (Ok(tab), Some(f)) = (std::env::var("TESTUDO_DEBUG_FOCUS_TAB"), replay.view.focus.as_mut()) {
                    use crate::state::FocusTab;
                    f.tab = match tab.as_str() {
                        "network" => FocusTab::Network,
                        "console" => FocusTab::Console,
                        "output" => FocusTab::Output,
                        _ => FocusTab::Steps,
                    };
                }
                // Test hook (debug builds only): open the comparison.
                #[cfg(debug_assertions)]
                if std::env::var_os("TESTUDO_DEBUG_COMPARE").is_some()
                    && let (Some(store), Some(f)) = (app.store.clone(), replay.view.focus.as_mut())
                    && let Some(test) = f.recorded.as_ref().and_then(|m| {
                        let page = m.pages.get(&f.key)?;
                        m.test_for_page(page).and_then(|t| Some((t.file.clone()?, t.title_path.clone())))
                    })
                {
                    f.open_compare(&test.0, &test.1, &store);
                    f.compare_by_step = std::env::var("TESTUDO_DEBUG_COMPARE").as_deref() != Ok("time");
                    let back: isize = std::env::var("TESTUDO_DEBUG_COMPARE_STEP").ok().and_then(|n| n.parse().ok()).unwrap_or(0);
                    if back != 0 {
                        let old = f.step_compare(back, &store);
                        app.retired.extend(old);
                    }
                }
                window.focus(&app.focus_handle, cx);
            }
            // Test hook (debug builds only): play the focused recording at 0.25×.
            #[cfg(debug_assertions)]
            if s.focus_page.is_some()
                && std::env::var_os("TESTUDO_DEBUG_PLAY").is_some()
                && let Some(f) = app.replay.as_mut().and_then(|r| r.view.focus.as_mut())
            {
                f.speed = 0.25;
                f.playing = true;
            }
            // Test hook (debug builds only): press "Re-run test" in that focus.
            #[cfg(debug_assertions)]
            if s.focus_page.is_some() && std::env::var_os("TESTUDO_DEBUG_RERUN").is_some() {
                app.rerun_test(Which::Replay, cx);
            }
        }
        #[cfg(debug_assertions)]
        if std::env::var_os("TESTUDO_DEBUG_OPEN_PICKER").is_some() {
            app.pick_folder(window, cx);
        }
        #[cfg(debug_assertions)]
        if std::env::var_os("TESTUDO_DEBUG_OPEN_SETTINGS").is_some() {
            let app_handle = cx.entity().downgrade();
            window.defer(cx, move |window, cx| {
                let _ = app_handle.update(cx, |app, cx| app.open_settings(window, cx));
            });
        }
        app
    }

    // --- data flow -------------------------------------------------------------

    fn on_msg(&mut self, msg: UiMsg, cx: &mut Context<Self>) {
        let run = match &msg {
            UiMsg::Event { run_id, .. } => run_id.as_str(),
            UiMsg::Frame { key, .. } => key.run_id.as_str(),
        };
        if self.dropped_runs.contains(run) {
            if let UiMsg::Event { event: Event::ProcessExited { .. }, .. } = &msg {
                self.dropped_runs.remove(run);
                self.refresh_history();
            }
            return;
        }
        match msg {
            UiMsg::Event { run_id, at, event } => {
                if let Event::ProcessExited { .. } = &event {
                    self.handles.remove(&run_id);
                    // Give the recorder a moment to commit the final status.
                    cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(Duration::from_millis(500)).await;
                        let _ = this.update(cx, |app, cx| {
                            app.refresh_history();
                            cx.notify();
                        });
                    })
                    .detach();
                }
                #[cfg(debug_assertions)]
                let opened = match &event {
                    Event::PageOpened { page_id, .. } => Some(page_id.clone()),
                    _ => None,
                };
                if matches!(event, Event::RunStarted { .. }) {
                    self.live.drop_finished_run_pages(&mut self.retired);
                }
                let follow = match &event {
                    Event::PageOpened { page_id, .. } if self.follow_run.as_deref() == Some(run_id.as_str()) => {
                        Some(page_id.clone())
                    }
                    _ => None,
                };
                self.live.apply(&run_id, at, event);
                if let Some(page_id) = follow {
                    self.follow_run = None;
                    self.open_live_focus(PageKey { run_id: run_id.clone(), page_id });
                }
                // Test hook (debug builds only): focus the first page that opens.
                #[cfg(debug_assertions)]
                if let Some(page_id) = opened
                    && self.live.focus.is_none()
                    && std::env::var_os("TESTUDO_DEBUG_FOCUS_FIRST").is_some()
                {
                    self.open_live_focus(PageKey { run_id, page_id });
                }
            }
            UiMsg::Frame { key, at, image } => {
                let old = self.live.model.apply_frame(&key, at, image);
                self.retired.extend(old);
            }
        }
    }

    fn refresh_history(&mut self) {
        if let Some(store) = &self.store {
            match store.list_runs(HISTORY_LIMIT) {
                Ok(rows) => self.history = rows,
                Err(e) => self.error = Some(format!("reading history: {e:#}")),
            }
            self.flaky = store.flaky_tests(SUMMARY_RUNS).unwrap_or_default();
        }
    }

    fn scan(&mut self, cx: &mut Context<Self>) {
        let raw = self.project_input.read(cx).value().to_string();
        let dir = PathBuf::from(expand_tilde(raw.trim()));
        if !dir.is_dir() {
            self.suites.clear();
            self.scanned = None;
            self.error = Some(format!("{} is not a directory", dir.display()));
            return;
        }
        let same = |a: &PathBuf, b: &PathBuf| a.canonicalize().ok().zip(b.canonicalize().ok()).is_some_and(|(a, b)| a == b);
        if self.scanned.as_ref().is_some_and(|old| !same(old, &dir)) {
            self.leave_project();
        }
        self.suites = discover(&dir);
        self.scanned = Some(dir);
        self.error = None;
        cx.notify();
    }

    /// Switching projects: stop its runs and clear the live view (runs, pages,
    /// focus). Settings are kept; History still has everything.
    #[tracing::instrument(level = "info", skip_all, fields(stopped = self.handles.len()))]
    fn leave_project(&mut self) {
        for (id, handle) in self.handles.drain() {
            handle.cancel();
            self.dropped_runs.insert(id);
        }
        // Finished runs may still have stragglers (late adapter messages).
        self.dropped_runs.extend(self.live.model.runs.iter().map(|r| r.id.clone()));
        let images = self.live.clear();
        self.retired.extend(images);
    }

    fn autorun(&mut self, filter: &str, cx: &mut Context<Self>) {
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
            self.start_suite(s, cx);
        }
    }

    #[tracing::instrument(level = "info", skip_all, fields(suite = %suite.name))]
    fn start_suite(&mut self, suite: &Suite, cx: &mut Context<Self>) {
        let args = self
            .args_input
            .read(cx)
            .value()
            .split_whitespace()
            .map(str::to_owned)
            .collect();
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
        cx.notify();
    }

    /// The suite and arguments to run just the focused page's test again, or
    /// why that isn't possible. Looks for the suite among the scanned ones,
    /// then in the run's own directory (a History run from another project).
    fn rerun_target(&self, w: Which, cx: &App) -> Result<(Suite, Vec<String>), String> {
        let view = self.view(w).ok_or("no view")?;
        let focus = view.focus.as_ref().ok_or("no page open")?;
        let model = focus.recorded.as_deref().unwrap_or(&view.model);
        let page = model.pages.get(&focus.key).ok_or("page not found")?;
        let test = model.test_for_page(page).ok_or("this page isn't tied to a test")?;
        let run = model.run(&page.key.run_id).ok_or("run not found")?;
        let (Some(file), Some(line)) = (&test.file, test.line) else {
            return Err("the runner didn't report this test's location".into());
        };
        let mut args = rerun_args(&run.runner, &run.cwd, file, line, test.project.as_deref())
            .ok_or_else(|| format!("re-running single tests isn't supported for {}", run.runner))?;
        let same_dir = |a: &std::path::Path| {
            a.canonicalize().ok().zip(std::path::Path::new(&run.cwd).canonicalize().ok()).is_some_and(|(a, b)| a == b)
        };
        let matches = |s: &Suite| s.kind.runner().is_some_and(|k| k.name() == run.runner) && same_dir(&s.dir);
        let suite = self
            .suites
            .iter()
            .find(|s| matches(s))
            .cloned()
            .or_else(|| discover(std::path::Path::new(&run.cwd)).into_iter().find(|s| matches(s)))
            .ok_or_else(|| format!("no {} suite found in {}", run.runner, run.cwd))?;
        let mut user: Vec<String> =
            self.args_input.read(cx).value().split_whitespace().map(str::to_owned).collect();
        user.append(&mut args);
        Ok((suite, user))
    }

    /// Run the focused page's test again and watch it: switches to Live and
    /// opens the new run's page in the focus view when it appears.
    #[tracing::instrument(level = "info", skip_all, fields(view = ?w))]
    fn rerun_test(&mut self, w: Which, cx: &mut Context<Self>) {
        let (suite, args) = match self.rerun_target(w, cx) {
            Ok(t) => t,
            Err(e) => {
                self.error = Some(format!("Can't re-run this test: {e}"));
                cx.notify();
                return;
            }
        };
        let Some(spec) = RunSpec::for_suite(&suite, args) else { return };
        match self.orchestrator.start(spec) {
            Ok(handle) => {
                self.follow_run = Some(handle.id.clone());
                self.live.selected_run = Some(handle.id.clone());
                self.handles.insert(handle.id.clone(), handle);
                if self.live.focus.is_some() {
                    self.close_focus(Which::Live);
                }
                self.mode = Mode::Live;
                self.error = None;
            }
            Err(e) => self.error = Some(format!("{}: {e:#}", suite.name)),
        }
        cx.notify();
    }

    #[tracing::instrument(level = "info", skip(self))]
    fn open_replay(&mut self, run_id: &str) {
        let Some(store) = &self.store else {
            self.error = Some("history unavailable: store failed to open".into());
            return;
        };
        match Replay::load(store, run_id) {
            Ok(r) => {
                if let Some(mut old) = self.replay.replace(r) {
                    self.retired.extend(old.view.take_images());
                }
                self.mode = Mode::History;
                self.error = None;
            }
            Err(e) => self.error = Some(format!("{e:#}")),
        }
    }

    fn pick_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose a project directory".into()),
        });
        self.picking = true;
        cx.spawn_in(window, async move |this, cx| {
            let chosen = rx.await;
            let _ = this.update_in(cx, |app, window, cx| {
                app.picking = false;
                if let Ok(Ok(Some(paths))) = chosen
                    && let Some(dir) = paths.into_iter().next()
                {
                    app.project_input
                        .update(cx, |s, cx| s.set_value(dir.display().to_string(), window, cx));
                    app.scan(cx);
                    app.mode = Mode::Live;
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn view(&self, w: Which) -> Option<&PagesView> {
        match w {
            Which::Live => Some(&self.live),
            Which::Replay => self.replay.as_ref().map(|r| &r.view),
        }
    }

    fn view_mut(&mut self, w: Which) -> Option<&mut PagesView> {
        match w {
            Which::Live => Some(&mut self.live),
            Which::Replay => self.replay.as_mut().map(|r| &mut r.view),
        }
    }

    fn open_live_focus(&mut self, key: PageKey) {
        if let Some(page) = self.live.model.pages.get(&key) {
            let f = PageFocus::live(page);
            self.live.open_focus(f);
        }
    }

    #[tracing::instrument(level = "info", skip_all, fields(view = ?w, page.id = %key.page_id, run.id = %key.run_id))]
    fn open_focus(&mut self, w: Which, key: PageKey, window: &mut Window, cx: &mut Context<Self>) {
        match w {
            Which::Live => self.open_live_focus(key),
            Which::Replay => {
                if let Some(r) = self.replay.as_mut() {
                    r.open_focus(key);
                }
            }
        }
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    #[tracing::instrument(level = "info", skip_all, fields(view = ?w))]
    fn close_focus(&mut self, w: Which) {
        let mut retired = Vec::new();
        if let Some(v) = self.view_mut(w)
            && let Some(mut f) = v.focus.take()
        {
            retired = f.take_images();
        }
        self.retired.extend(retired);
    }

    /// Run `f` on the focused page of view `w`, with its page data. The page may
    /// live inside the focus (History) or the view's model (Live).
    fn with_focus<R>(
        &mut self,
        w: Which,
        f: impl FnOnce(&mut PageFocus, &Page, Option<&Store>, &mut Retired) -> R,
    ) -> Option<R> {
        let Self { live, replay, retired, store, .. } = self;
        let view = match w {
            Which::Live => live,
            Which::Replay => &mut replay.as_mut()?.view,
        };
        let PagesView { model, focus, .. } = view;
        let focus = focus.as_mut()?;
        let recorded = focus.recorded.take();
        let source: &Model = recorded.as_deref().unwrap_or(model);
        let out = source
            .pages
            .get(&focus.key)
            .map(|page| f(focus, page, store.as_deref(), retired));
        focus.recorded = recorded;
        out
    }

    /// The focused page's test steps, (start, title), while comparing.
    fn compare_steps(&self, w: Which) -> Vec<(f64, String)> {
        let Some(view) = self.view(w) else { return Vec::new() };
        let Some(focus) = view.focus.as_ref().filter(|f| f.compare.is_some()) else { return Vec::new() };
        let model = focus.recorded.as_deref().unwrap_or(&view.model);
        model
            .pages
            .get(&focus.key)
            .and_then(|p| model.test_for_page(p))
            .map(|t| t.steps.iter().map(|s| (s.started, s.title.clone())).collect())
            .unwrap_or_default()
    }

    /// Test hook (debug builds only): with TESTUDO_DEBUG_AUTOSCROLL=1, scroll the
    /// run list at 1200 px/s from 4s to 10s after start, reversing at either
    /// end, as wheel scrolling would.
    #[cfg(debug_assertions)]
    fn debug_autoscroll(&mut self, window: &mut Window) {
        if std::env::var_os("TESTUDO_DEBUG_AUTOSCROLL").is_none() {
            return;
        }
        let t = self.started_at.elapsed().as_secs_f32();
        if !(4.0..10.0).contains(&t) {
            if t < 4.0 {
                window.request_animation_frame();
            }
            return;
        }
        let handle = self.run_scroll.0.borrow().base_handle.clone();
        let max = f32::from(handle.max_offset().y).max(1.0);
        let travel = (t - 4.0) * 1200.0;
        let y = travel % (2.0 * max);
        let y = if y > max { 2.0 * max - y } else { y };
        handle.set_offset(point(px(0.), px(-y)));
        self.perf.input("run_list");
        window.request_animation_frame();
    }

    /// Test hook (debug builds only): TESTUDO_DEBUG_SWITCH_DIR=<dir> switches
    /// to that project 8s after start, as if typed in and scanned.
    #[cfg(debug_assertions)]
    fn debug_switch_dir(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dir) = std::env::var_os("TESTUDO_DEBUG_SWITCH_DIR") else { return };
        if self.started_at.elapsed() < Duration::from_secs(8) {
            window.request_animation_frame();
            return;
        }
        let dir = dir.to_string_lossy().into_owned();
        if self.project_input.read(cx).value().as_ref() != dir.as_str() {
            self.project_input.update(cx, |s, cx| s.set_value(dir, window, cx));
            self.scan(cx);
        }
    }

    /// What is on screen, for slow-frame reports.
    fn frame_ctx(&self) -> crate::perf::FrameCtx {
        let w = if self.mode == Mode::History { Which::Replay } else { Which::Live };
        let view = self.view(w);
        crate::perf::FrameCtx {
            history: w == Which::Replay,
            focus: view.is_some_and(|v| v.focus.is_some()),
            pages: view.map_or(0, |v| v.model.pages.len()),
            test_rows: view.map_or(0, |v| v.model.runs.iter().map(|r| r.tests.len()).sum()),
            compare_lanes: view
                .and_then(|v| v.focus.as_ref())
                .and_then(|f| f.compare.as_ref())
                .map_or(0, |c| usize::from(c.lane.is_some())),
        }
    }

    fn focused_view(&self) -> Option<Which> {
        match self.mode {
            Mode::Live if self.live.focus.is_some() => Some(Which::Live),
            Mode::History if self.replay.as_ref().is_some_and(|r| r.view.focus.is_some()) => Some(Which::Replay),
            _ => None,
        }
    }

    // --- per-frame updates -----------------------------------------------------

    /// Advance playback, follow mode, exports and reaping. Returns true if
    /// another frame should be scheduled.
    fn tick(&mut self, dt_ms: f64) -> bool {
        let mut animate = false;
        if let Some(store) = self.store.clone()
            && let Some(replay) = self.replay.as_mut()
        {
            animate |= replay.tick(dt_ms, &store, &mut self.retired);
            animate |= replay.exporter.poll();
        }
        animate |= self.live.reap(now_ms(), &mut self.retired);
        let mut playing: Option<&'static str> = None;
        for w in [Which::Live, Which::Replay] {
            let running_live = w == Which::Live;
            let steps = self.compare_steps(w);
            let r = self.with_focus(w, |f, page, store, retired| {
                let running = running_live && page.closed_at.is_none() && f.recorded.is_none();
                let a = f.tick(page, running, dt_ms);
                if f.playing {
                    playing = Some(if f.compare.is_some() { "focus_playback_compare" } else { "focus_playback" });
                }
                let decoding = f.update_image(page, store, retired);
                let decoding_compare = f.update_compare(page, &steps, retired);
                // Keep frames coming until in-flight decodes land.
                a || decoding || decoding_compare
            });
            animate |= r.unwrap_or(false);
        }
        // Playback counts as continuous input, so its frame rate is measured
        // and logged like a scroll burst.
        if let Some(area) = playing {
            self.perf.input(area);
        }
        animate
    }
}

// --- actions --------------------------------------------------------------------

impl TestudoApp {
    fn on_prev_frame(&mut self, _: &PrevFrame, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(w) = self.focused_view() {
            self.with_focus(w, |f, page, store, _| {
                f.playing = false;
                f.step_frames(-1, page, store)
            });
            cx.notify();
        }
    }

    fn on_next_frame(&mut self, _: &NextFrame, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(w) = self.focused_view() {
            self.with_focus(w, |f, page, store, _| {
                f.playing = false;
                f.step_frames(1, page, store)
            });
            cx.notify();
        }
    }

    fn on_toggle_play(&mut self, _: &TogglePlay, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(w) = self.focused_view() {
            self.with_focus(w, |f, page, _, _| {
                let (start, end) = f.bounds(page);
                if !f.playing && f.t >= end - 1.0 {
                    f.seek(start, page);
                }
                f.playing = !f.playing;
                f.follow_live = false;
            });
            cx.notify();
        }
    }

    fn on_close_focus(&mut self, _: &CloseFocus, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(w) = self.focused_view() {
            self.close_focus(w);
            cx.notify();
        }
    }
}

// --- rendering --------------------------------------------------------------------

fn tone(t: Tone, cx: &App) -> Hsla {
    let th = cx.theme();
    match t {
        Tone::Running => th.blue,
        Tone::Passed => th.green,
        Tone::Failed => th.red,
        Tone::Warn => th.yellow,
        Tone::Muted => th.muted_foreground,
    }
}

/// A small tinted label: status, browser, kind, counts.
fn pill(text: impl Into<SharedString>, color: Hsla) -> Div {
    div()
        .flex_none()
        .px_2()
        .rounded_full()
        .whitespace_nowrap()
        .text_xs()
        .bg(color.opacity(0.16))
        .text_color(color)
        .child(text.into())
}

/// Centred explanation over a skipped test's (usually blank) frame. Shows the
/// first line of the runner's reason; the full text is in the tooltip.
fn skip_card(note: String, theme: &gpui_kit::component::Theme) -> Div {
    let reason = summary_line(&note);
    div().absolute().inset_0().flex().items_center().justify_center().p_3().child(
        v_flex()
            .w_full()
            .max_w(px(360.))
            .gap_1()
            .px_3()
            .py_2()
            .rounded_md()
            .overflow_hidden()
            .bg(theme.background.opacity(0.92))
            .border_1()
            .border_color(theme.border)
            .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child("Test skipped"))
            .child(
                div()
                    .w_full()
                    .truncate()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(if reason.is_empty() { "skipped before the page was used".to_owned() } else { reason }),
            ),
    )
}

/// Playback speed picker: the current speed, opening a menu of `SPEEDS`.
fn speed_menu(
    id: &'static str,
    current: f32,
    cx: &mut Context<TestudoApp>,
    set: impl Fn(&mut TestudoApp, f32) + 'static,
) -> impl IntoElement {
    let app = cx.entity().downgrade();
    let set = Rc::new(set);
    // An outlined chip with a caret, like the speed pickers in session-replay
    // and video players, so it reads as a control rather than a label.
    button(id)
        .label(speed_label(current))
        .small()
        .outline()
        .dropdown_caret(true)
        .tooltip("Playback speed")
        // Open above the chip (the menu's bottom-left corner on it), so the
        // chip stays visible; the controls sit at the bottom of the window.
        .dropdown_menu_with_anchor(Anchor::BottomLeft, move |mut menu, _, _| {
            menu = menu.label("Playback speed");
            for s in crate::state::SPEEDS {
                let (app, set) = (app.clone(), set.clone());
                menu = menu.item(PopupMenuItem::new(speed_label(s)).checked(s == current).on_click(move |_, _, cx| {
                    let _ = app.update(cx, |this, cx| {
                        set(this, s);
                        cx.notify();
                    });
                }));
            }
            menu
        })
}

fn speed_label(s: f32) -> String {
    format!("{s}×")
}

/// The app icon (packaging/icons), embedded. One shared instance so gpui
/// decodes and caches it once.
fn app_icon() -> Arc<Image> {
    static ICON: std::sync::LazyLock<Arc<Image>> = std::sync::LazyLock::new(|| {
        Arc::new(Image::from_bytes(
            ImageFormat::Png,
            include_bytes!("../../../../packaging/icons/testudo-64.png").to_vec(),
        ))
    });
    ICON.clone()
}

/// A button with the pointer cursor (gpui-kit's buttons keep the arrow, the
/// desktop default); disabled buttons put the arrow back.
fn button(id: impl Into<ElementId>) -> Button {
    Button::new(id).cursor_pointer()
}

/// Height of a page tile's title/URL bar; the frame fits above it.
const TILE_LABEL_H: f32 = 46.;

/// A recorded page frame, fitted and centred inside its container (which
/// must be `relative`) whatever its shape. gpui's `img` takes the picture's
/// aspect ratio unless both its width and height are definite: with
/// `size_full` alone a flex-sized container's height is indefinite, so a
/// portrait (mobile) frame was drawn full-width, zoomed and clipped. Pinned to
/// the container (absolute, 100% × 100%), the box is the container and
/// `Contain` fits the picture inside it.
fn frame_img(image: Arc<RenderImage>) -> Img {
    img(image).object_fit(ObjectFit::Contain).absolute().inset_0().size_full()
}

/// A Lucide icon bundled with gpui-kit's assets.
fn media_icon(name: &str) -> Icon {
    Icon::empty().path(format!("icons/{name}.svg"))
}

fn mono_row(text: impl Into<SharedString>, color: Hsla) -> Div {
    div()
        .h(px(18.))
        .font_family(crate::fonts::mono())
        .text_xs()
        .text_color(color)
        .whitespace_nowrap()
        .overflow_hidden()
        .child(text.into())
}

fn tooltip_text(text: impl Into<SharedString>) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let text: SharedString = text.into();
    move |window, cx| Tooltip::new(text.clone()).build(window, cx)
}

impl Render for TestudoApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let frame_start = self.perf.frame_start();
        // Free GPU atlas entries for frames replaced since the last render.
        for image in self.retired.drain(..) {
            let _ = window.drop_image(image);
        }
        let dt = self.last_render.elapsed().as_secs_f64().min(0.1) * 1000.0;
        self.last_render = Instant::now();
        if self.tick(dt) {
            window.request_animation_frame();
        }
        // tick may have retired more (replay seeks, focus frames).
        for image in self.retired.drain(..) {
            let _ = window.drop_image(image);
        }

        let body = match self.mode {
            Mode::Live => self.render_live(window, cx).into_any_element(),
            Mode::History => self.render_history(window, cx).into_any_element(),
        };
        #[cfg(debug_assertions)]
        self.debug_autoscroll(window);
        #[cfg(debug_assertions)]
        self.debug_switch_dir(window, cx);
        let ctx = self.frame_ctx();
        if self.perf.frame_end(frame_start, ctx) {
            window.request_animation_frame();
        }
        v_flex()
            .id("testudo")
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .key_context("PageFocus")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_prev_frame))
            .on_action(cx.listener(Self::on_next_frame))
            .on_action(cx.listener(Self::on_toggle_play))
            .on_action(cx.listener(Self::on_close_focus))
            .child(self.render_top_bar(window, cx))
            .children(self.error.clone().map(|e| {
                div()
                    .px_3()
                    .py_1()
                    .text_sm()
                    .text_color(cx.theme().red)
                    .child(e)
            }))
            .child(div().flex_1().min_h_0().child(body))
    }
}

impl TestudoApp {
    fn render_top_bar(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mode = self.mode;
        h_flex()
            .gap_2()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(img(app_icon()).size(px(24.)).flex_none())
                    .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child("Testudo")),
            )
            .child(
                button("mode-live")
                    .label("● Live")
                    .small()
                    .ghost()
                    .selected(mode == Mode::Live)
                    .on_click(cx.listener(|this, _, _, cx| {
                        let _span = tracing::info_span!("switch_mode", to = "live").entered();
                        this.mode = Mode::Live;
                        cx.notify();
                    })),
            )
            .child(
                button("mode-history")
                    .label("History")
                    .small()
                    .ghost()
                    .selected(mode == Mode::History)
                    .on_click(cx.listener(|this, _, _, cx| {
                        let _span = tracing::info_span!("switch_mode", to = "history").entered();
                        this.refresh_history();
                        this.mode = Mode::History;
                        cx.notify();
                    })),
            )
            .child(div().w(px(1.)).h(px(18.)).bg(cx.theme().border))
            .child(
                button("pick-folder")
                    .label(if self.picking { "…" } else { "📂" })
                    .small()
                    .disabled(self.picking).when(self.picking, |b| b.cursor_default())
                    .tooltip("Choose a project directory")
                    .on_click(cx.listener(|this, _, window, cx| this.pick_folder(window, cx))),
            )
            .child(div().w(px(380.)).child(Input::new(&self.project_input).small()))
            .child(
                button("scan")
                    .label("⟳ Scan")
                    .small()
                    .tooltip("Find test suites in this directory")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.scan(cx);
                        this.mode = Mode::Live;
                    })),
            )
            .child(div().w(px(220.)).child(Input::new(&self.args_input).small()))
            .when(mode == Mode::Live, |row| {
                row.child(
                    Checkbox::new("keep-closed")
                        .cursor_pointer()
                        .label("Keep closed pages")
                        .checked(self.live.keep_closed_pages)
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            this.live.keep_closed_pages = *checked;
                            cx.notify();
                        })),
                )
            })
            .child(div().flex_1())
            .child(self.settings_menu(cx))
    }

    /// Gear button opening the settings dialog.
    fn settings_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        button("settings")
            .icon(media_icon("settings"))
            .small()
            .ghost()
            .tooltip("Settings")
            .on_click(cx.listener(|this, _, window, cx| this.open_settings(window, cx)))
    }

    #[tracing::instrument(level = "info", skip_all)]
    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let app = cx.entity().downgrade();
        let store = self.store.clone();
        let panel = cx.new(|_| settings::SettingsPanel::new(app, store));
        window.open_dialog(cx, move |dialog, _, _| dialog.title("Settings").w(px(600.)).child(panel.clone()));
    }

    /// Recordings were deleted from the settings dialog: refresh History and
    /// close a replay whose run is gone.
    fn after_recordings_deleted(&mut self) {
        self.refresh_history();
        let gone = match (&self.replay, &self.store) {
            (Some(r), Some(store)) => store.run(&r.run.id).ok().flatten().is_none(),
            _ => false,
        };
        if gone && let Some(mut r) = self.replay.take() {
            self.retired.extend(r.view.take_images());
        }
    }

    // --- live ---------------------------------------------------------------

    fn render_live(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Suites scroll on their own; the run list is virtualized below them.
        let sidebar = v_flex()
            .size_full()
            .p_2()
            .gap_1()
            .child(
                div()
                    .id("live-suites")
                    .flex_none()
                    .max_h(relative(0.45))
                    .overflow_y_scroll()
                    .child(self.render_suites(cx)),
            )
            .child(div().h(px(1.)).bg(cx.theme().border).my_1())
            .child(div().text_base().font_weight(FontWeight::SEMIBOLD).child("Runs"))
            .when(self.live.model.runs.is_empty(), |d| {
                d.child(div().text_sm().text_color(cx.theme().muted_foreground).child("No runs yet. Press ▶ on a suite."))
            })
            .child(self.run_list(Which::Live, cx));

        h_resizable("live-main")
            .child(resizable_panel().size(px(330.)).size_range(px(220.)..px(700.)).child(sidebar))
            .child(
                resizable_panel().child(if self.live.focus.is_some() {
                    // The focus view brings its own details panel.
                    self.render_focus(Which::Live, window, cx).into_any_element()
                } else {
                    v_resizable("live-center")
                        .child(resizable_panel().child(self.render_pages(Which::Live, window, cx)))
                        .child(
                            resizable_panel()
                                .size(px(240.))
                                .size_range(px(90.)..px(700.))
                                .child(self.render_inspector(Which::Live, window, cx)),
                        )
                        .into_any_element()
                }),
            )
    }

    fn render_suites(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let runnable = self.suites.iter().filter(|s| s.runnable()).count();
        let muted = cx.theme().muted_foreground;
        let mut col = v_flex()
            .gap_1()
            .child(div().text_base().font_weight(FontWeight::SEMIBOLD).child(format!("Suites ({runnable})")));
        if self.scanned.is_some() && self.suites.is_empty() {
            col = col.child(div().text_sm().text_color(muted).child("No Playwright or Vitest suites found here."));
        }
        for (i, s) in self.suites.iter().enumerate() {
            let suite = s.clone();
            let mut hover = vec![s.id.clone()];
            hover.extend(s.problems.iter().map(|p| format!("✘ {p}")));
            hover.extend(s.warnings.iter().map(|w| format!("⚠ {w}")));
            col = col.child(
                h_flex()
                    .id(("suite", i))
                    .gap_2()
                    .child(
                        button(("run-suite", i))
                            .label("▶")
                            .xsmall()
                            .disabled(!s.runnable()).when(!s.runnable(), |b| b.cursor_default())
                            .on_click(cx.listener(move |this, _, _, cx| this.start_suite(&suite, cx))),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_sm()
                            .when(!s.runnable(), |d| d.text_color(muted))
                            .child(s.name.clone()),
                    )
                    .children(s.tool.as_ref().and_then(|t| t.version.clone()).map(|v| div().text_xs().text_color(muted).child(v)))
                    .when(!s.warnings.is_empty(), |d| d.child(div().text_color(cx.theme().yellow).child("⚠")))
                    .tooltip(tooltip_text(hover.join("\n"))),
            );
        }
        if runnable > 1 {
            col = col.child(
                button("run-all")
                    .label(format!("▶ Run all {runnable}"))
                    .small()
                    .on_click(cx.listener(|this, _, _, cx| {
                        let all: Vec<Suite> = this.suites.iter().filter(|s| s.runnable()).cloned().collect();
                        for s in &all {
                            this.start_suite(s, cx);
                        }
                    })),
            );
        }
        col
    }

    /// The run list: every run's header, counts and tests, flattened into
    /// fixed-height rows and virtualized, so only visible rows are built and
    /// laid out (a 600-test Vitest run otherwise costs ~200 ms per frame).
    fn run_list(&mut self, w: Which, cx: &mut Context<Self>) -> AnyElement {
        let Some(view) = self.view(w) else { return div().into_any_element() };
        let rows = Rc::new(run_rows(view));
        let list = uniform_list(
            ("runs", w as usize),
            rows.len(),
            cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                range.map(|i| this.render_run_row(w, &rows[i], cx)).collect::<Vec<_>>()
            }),
        )
        .track_scroll(&self.run_scroll)
        .size_full();
        let errors = self.error_panel(w, cx);
        v_flex()
            .flex_1()
            .min_h_0()
            .gap_1()
            .child(
                div()
                    .id(("run-list", w as usize))
                    .flex_1()
                    .min_h_0()
                    .on_scroll(cx.listener(|this, _: &ScrollEvent, _, _| this.perf.input("run_list")))
                    .child(list),
            )
            .children(errors)
            .into_any_element()
    }

    /// Full errors of the tests opened from the run list, wrapped and
    /// scrollable (list rows are one line each).
    fn error_panel(&mut self, w: Which, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = cx.theme().clone();
        let view = self.view(w)?;
        let open: Vec<(String, String, String)> = view
            .model
            .runs
            .iter()
            .rev()
            .flat_map(|r| r.tests.iter())
            .filter(|t| view.expanded_tests.contains(&t.id))
            .filter_map(|t| Some((t.id.clone(), t.title.clone(), truncate(t.error.as_deref()?, MAX_ERROR_CHARS))))
            .collect();
        if open.is_empty() {
            return None;
        }
        let mut panel = v_flex()
            .id(("error-panel", w as usize))
            .flex_none()
            .max_h(relative(0.5))
            .overflow_y_scroll()
            .gap_2()
            .p_2()
            .rounded_md()
            .bg(theme.red.opacity(0.06))
            .border_1()
            .border_color(theme.red.opacity(0.4));
        for (i, (id, title, error)) in open.into_iter().enumerate() {
            panel = panel.child(
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(div().flex_1().min_w_0().truncate().text_sm().font_weight(FontWeight::SEMIBOLD).child(title))
                            .child(
                                button(("close-error", i))
                                    .label("✕")
                                    .xsmall()
                                    .ghost()
                                    .tooltip("Close")
                                    .on_click(cx.listener(toggle_error(w, id))),
                            ),
                    )
                    .child(
                        div()
                            .font_family(crate::fonts::mono())
                            .text_xs()
                            .text_color(theme.red)
                            .whitespace_normal()
                            .child(error),
                    ),
            );
        }
        Some(panel.into_any_element())
    }

    /// A run-list row, with the selected run (its output is in the Output tab)
    /// marked by an accent bar down every one of its rows: header, counts,
    /// tests and notes. Other runs keep a transparent bar so rows align.
    fn render_run_row(&mut self, w: Which, row: &ListRow, cx: &mut Context<Self>) -> AnyElement {
        let run_index = match *row {
            ListRow::Header(ri) | ListRow::Counts(ri) | ListRow::Test(ri, _) | ListRow::Note(ri, _, _) => Some(ri),
            ListRow::Gap => None,
        };
        let selected = run_index.is_some_and(|ri| {
            self.view(w)
                .is_some_and(|v| v.model.runs.get(ri).is_some_and(|r| v.selected_run.as_deref() == Some(&r.id)))
        });
        let accent = cx.theme().blue;
        let content = self.run_row_content(w, row, cx);
        div()
            .w_full()
            .border_l_2()
            .pl_1()
            .border_color(if selected { accent } else { gpui_kit::transparent_black() })
            .child(content)
            .into_any_element()
    }

    fn run_row_content(&mut self, w: Which, row: &ListRow, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let Some(view) = self.view(w) else { return div().into_any_element() };
        let line = || div().w_full().h(px(RUN_ROW_H)).flex().items_center().overflow_hidden();
        match *row {
            ListRow::Gap => line().child(div().w_full().h(px(1.)).bg(theme.border)).into_any_element(),
            ListRow::Header(ri) => {
                let Some(run) = view.model.runs.get(ri) else { return line().into_any_element() };
                let (status, tn) = match (&run.exit, run.run_end.as_deref()) {
                    (None, _) => ("Running".to_owned(), Tone::Running),
                    (Some(_), Some("passed")) | (Some(Some(0)), _) => ("Passed".to_owned(), Tone::Passed),
                    (Some(_), Some("interrupted")) => ("Interrupted".to_owned(), Tone::Warn),
                    (Some(Some(code)), _) => (format!("Failed · exit {code}"), Tone::Failed),
                    (Some(None), _) => ("Killed".to_owned(), Tone::Failed),
                };
                let collapsed = view.collapsed_runs.contains(&run.id);
                let run_id = run.id.clone();
                let toggle_id = run.id.clone();
                let mut header = line()
                    .gap_1()
                    .child(
                        div()
                            .id(("run-toggle", ri))
                            .flex_none()
                            .w(px(18.))
                            .text_center()
                            .text_xs()
                            .text_color(muted)
                            .rounded_sm()
                            .cursor_pointer()
                            .hover(|d| d.bg(theme.list_hover))
                            .child(if collapsed { "▸" } else { "▾" })
                            .tooltip(tooltip_text(if collapsed { "show tests" } else { "hide tests" }))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(v) = this.view_mut(w)
                                    && !v.collapsed_runs.remove(&toggle_id)
                                {
                                    v.collapsed_runs.insert(toggle_id.clone());
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .id(("run", ri))
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .px_1()
                            .rounded_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .cursor_pointer()
                            .hover(|d| d.bg(theme.list_hover))
                            .child(run.label.clone())
                            .tooltip(tooltip_text("show this run's output"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(v) = this.view_mut(w) {
                                    v.selected_run = Some(run_id.clone());
                                    v.inspector_tab = InspectorTab::Output;
                                }
                                cx.notify();
                            })),
                    )
                    .child(pill(status, tone(tn, cx)));
                if w == Which::Live {
                    if run.is_running() && self.handles.contains_key(&run.id) {
                        let id = run.id.clone();
                        header = header.child(button(("stop", ri)).label("Stop").xsmall().ghost().on_click(
                            cx.listener(move |this, _, _, cx| {
                                if let Some(h) = this.handles.get(&id) {
                                    h.cancel();
                                }
                                cx.notify();
                            }),
                        ));
                    } else if !run.is_running() && self.store.is_some() {
                        let id = run.id.clone();
                        header = header.child(button(("replay", ri)).label("Replay").xsmall().ghost().on_click(
                            cx.listener(move |this, _, _, cx| {
                                this.open_replay(&id);
                                cx.notify();
                            }),
                        ));
                    }
                }
                header.into_any_element()
            }
            ListRow::Counts(ri) => {
                let Some(run) = view.model.runs.get(ri) else { return line().into_any_element() };
                let (passed, failed, running, pending) = run.counts();
                let total = run.total_tests.map(|t| format!(" of {t}")).unwrap_or_default();
                line()
                    .gap_1()
                    .when(passed > 0, |d| d.child(pill(format!("✔ {passed}"), theme.green)))
                    .when(failed > 0, |d| d.child(pill(format!("✘ {failed}"), theme.red)))
                    .when(running > 0, |d| d.child(pill(format!("⏳ {running}"), theme.blue)))
                    .when(pending > 0, |d| d.child(pill(format!("○ {pending}"), muted)))
                    .child(div().text_xs().text_color(muted).child(format!("{}{total} tests", run.tests.len())))
                    .into_any_element()
            }
            ListRow::Test(ri, ti) => {
                let Some(run) = view.model.runs.get(ri) else { return line().into_any_element() };
                let Some(t) = run.tests.get(ti) else { return line().into_any_element() };
                // Duration bars are relative to the slowest test in the run.
                let slowest = run.tests.iter().filter_map(|t| t.duration_ms).fold(1.0_f64, f64::max);
                let (icon, tn) = if t.queued { ("○", Tone::Muted) } else { status_icon(t.status) };
                let bar = t.duration_ms.map(|d| (d / slowest).clamp(0.02, 1.0) as f32);
                let bar_color = if matches!(tn, Tone::Failed) { theme.red } else { muted };
                let store = self.store.clone();
                let detail = {
                    let mut lines = vec![t.title_path.join(" › ")];
                    if let Some(f) = &t.file {
                        lines.push(format!("{f}:{}", t.line.unwrap_or(0)));
                    }
                    lines.join("\n")
                };
                let file = t.file.clone();
                let title_path = t.title_path.clone();
                let failing = t.error.is_some();
                let test_id = t.id.clone();
                line()
                    .id(("test", ri * 100_000 + ti))
                    .gap_2()
                    .px_1()
                    .rounded_sm()
                    .hover(|d| d.bg(theme.list_hover))
                    .when(failing, |d| d.cursor_pointer().on_click(cx.listener(toggle_error(w, test_id))))
                    .child(div().w(px(14.)).flex_none().text_xs().text_color(tone(tn, cx)).child(icon))
                    .child(div().flex_1().min_w_0().truncate().text_sm().child(t.title.clone()))
                    .child(
                        div()
                            .w(px(40.))
                            .h(px(4.))
                            .flex_none()
                            .rounded_full()
                            .bg(theme.secondary)
                            .children(bar.map(|f| div().h_full().w(px(40. * f)).rounded_full().bg(bar_color.opacity(0.8)))),
                    )
                    .child(
                        div()
                            .w(px(52.))
                            .flex_none()
                            .text_right()
                            .font_family(crate::fonts::mono())
                            .text_xs()
                            .text_color(muted)
                            .child(t.duration_ms.map(fmt_dur).unwrap_or_default()),
                    )
                    // Built only when hovered, so history is queried lazily.
                    .tooltip(move |window, cx| {
                        let mut text = detail.clone();
                        if let (Some(store), Some(file)) = (&store, &file)
                            && let Ok(history) = store.test_history(file, &title_path, 20)
                            && !history.is_empty()
                        {
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
                            text.push_str(&format!("\n\nlast {} runs (oldest → newest): {strip}", history.len()));
                            if strip.contains('✔') && strip.contains('✘') {
                                text.push_str("\nflaky: both passes and failures in recent history");
                            }
                        }
                        Tooltip::new(text).build(window, cx)
                    })
                    .into_any_element()
            }
            ListRow::Note(ri, ti, kind) => {
                let Some(t) = view.model.runs.get(ri).and_then(|r| r.tests.get(ti)) else {
                    return line().into_any_element();
                };
                let (text, color) = match kind {
                    NoteKind::Step => (format!("↳ {}", t.current_step.as_deref().unwrap_or_default()), muted),
                    NoteKind::Skip => (format!("skipped: {}", summary_line(t.note.as_deref().unwrap_or_default())), muted),
                    // Failing tests show their error's first line; click for all.
                    NoteKind::Error => {
                        let open = if view.expanded_tests.contains(&t.id) { "▾" } else { "▸" };
                        (format!("{open} {}", summary_line(t.error.as_deref().unwrap_or_default())), theme.red)
                    }
                };
                let test_id = t.id.clone();
                line()
                    .id(("note", ri * 100_000 + ti))
                    .pl(px(26.))
                    .text_xs()
                    .text_color(color)
                    .when(kind == NoteKind::Error, |d| d.cursor_pointer().on_click(cx.listener(toggle_error(w, test_id))))
                    .child(div().min_w_0().truncate().child(text))
                    .into_any_element()
            }
        }
    }

    // --- pages ----------------------------------------------------------------

    fn render_pages(&mut self, w: Which, _window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(view) = self.view(w) else { return div().into_any_element() };
        let muted = cx.theme().muted_foreground;
        let pages = ordered_pages(&view.model);
        if pages.is_empty() {
            let hint = match w {
                Which::Live => "Live browser pages appear here as suites open them.",
                Which::Replay => "No browser pages open at this point in the run.",
            };
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(muted)
                .child(hint)
                .into_any_element();
        }
        let cols = (pages.len() as f32).sqrt().ceil().max(1.0) as u16;
        let rows = (pages.len() as u16).div_ceil(cols).max(1);
        let selected = view.selected_page.clone();
        let mut grid = div().size_full().p_2().gap_2().grid().grid_cols(cols).grid_rows(rows);
        let theme = cx.theme().clone();
        for (i, page) in pages.into_iter().enumerate() {
            let test = view.model.test_for_page(page);
            let status = test.and_then(|t| t.status);
            let (_, tn) = status_icon(status);
            let is_selected = selected.as_ref() == Some(&page.key);
            let key = page.key.clone();
            let title = pane_name(&view.model, page).to_owned();
            let closed = page.closed_at.is_some();
            let live = w == Which::Live && !closed;
            let frame = page.frame.clone();
            // A skipped test's page usually never left about:blank: say so rather
            // than showing a white tile that looks broken.
            let skipped = (status == Some(testudo_protocol::TestStatus::Skipped))
                .then(|| test.and_then(|t| t.note.clone()).unwrap_or_default());
            // The status a pane shows: its test's, or "Closed" for a finished page
            // outside a test.
            let (label, color) = match (status, closed) {
                (None, true) => ("Closed", muted),
                _ => (status_label(status), tone(tn, cx)),
            };
            grid = grid.child(
                div()
                    .id(("pane", i))
                    .relative()
                    .min_h_0()
                    .min_w_0()
                    .overflow_hidden()
                    .rounded_md()
                    .border_1()
                    .border_color(if is_selected { theme.blue } else { theme.border })
                    .bg(theme.secondary.opacity(0.5))
                    .cursor_pointer()
                    .hover(|d| d.border_color(theme.muted_foreground))
                    // The frame fits the tile above the label bar (status pills
                    // sit on top of it).
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .left_0()
                            .right_0()
                            .bottom(px(TILE_LABEL_H))
                            .when(closed, |d| d.opacity(0.7))
                            .when(skipped.is_some(), |d| d.opacity(0.15))
                            .child(match frame {
                            Some(f) => frame_img(f).into_any_element(),
                            None => div()
                                .size_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_sm()
                                .text_color(muted)
                                .child("waiting for frames…")
                                .into_any_element(),
                        }),
                    )
                    .when_some(skipped.clone(), |d, note| d.child(skip_card(note, &theme)))
                    .child(
                        h_flex()
                            .absolute()
                            .top_2()
                            .left_2()
                            .gap_1()
                            .child(pill(label, color).bg(theme.background.opacity(0.85)))
                            .when(live, |d| d.child(pill("● LIVE", theme.red).bg(theme.background.opacity(0.85)))),
                    )
                    .child(
                        v_flex()
                            .absolute()
                            .bottom_0()
                            .left_0()
                            .right_0()
                            .h(px(TILE_LABEL_H))
                            .justify_center()
                            .px_2()
                            .bg(theme.background.opacity(0.82))
                            .child(div().truncate().text_sm().font_weight(FontWeight::SEMIBOLD).child(title.clone()))
                            .child(div().truncate().text_xs().text_color(muted).child(url_path(&page.url).to_owned())),
                    )
                    .tooltip(tooltip_text(match &skipped {
                        Some(note) if !note.is_empty() => {
                            format!("{title}\nskipped: {}\nclick to open", summary_line(note))
                        }
                        _ => format!("{title}\n{}\nclick to open: watch live, or scrub the recording", page.url),
                    }))
                    .on_click(cx.listener(move |this, _, window, cx| this.open_focus(w, key.clone(), window, cx))),
            );
        }
        grid.into_any_element()
    }

    // --- inspector ------------------------------------------------------------

    fn render_inspector(&mut self, w: Which, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(view) = self.view(w) else { return v_flex() };
        let tab = view.inspector_tab;
        let muted = cx.theme().muted_foreground;
        let label = match (tab, &view.selected_page, &view.selected_run, &view.focus) {
            (InspectorTab::Output, _, Some(run), _) => format!("run {run}"),
            (InspectorTab::Output, _, None, _) => "select a run".to_owned(),
            (_, Some(p), _, Some(f)) => {
                let opened = f
                    .recorded
                    .as_deref()
                    .unwrap_or(&view.model)
                    .pages
                    .get(p)
                    .map_or(f.t, |pg| pg.opened_at);
                format!("page {} · synced to {:.2}s", p.page_id, (f.t - opened) / 1000.0)
            }
            (_, Some(p), _, None) => format!("page {}", p.page_id),
            (_, None, _, _) => "click a page to inspect it".to_owned(),
        };
        let selected_page = view.selected_page.clone();

        let mut tabs = h_flex().gap_1().px_2().py_1().border_t_1().border_b_1().border_color(cx.theme().border);
        for (t, name) in [
            (InspectorTab::Network, "Network"),
            (InspectorTab::Console, "Console"),
            (InspectorTab::Output, "Output"),
        ] {
            tabs = tabs.child(
                button(SharedString::from(format!("tab-{name}")))
                    .label(name)
                    .xsmall()
                    .ghost()
                    .selected(tab == t)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(v) = this.view_mut(w) {
                            v.inspector_tab = t;
                        }
                        cx.notify();
                    })),
            );
        }
        tabs = tabs.child(div().text_xs().text_color(muted).child(label));
        if w == Which::Replay
            && let (Some(store), Some(replay)) = (self.store.clone(), self.replay.as_ref())
        {
            let exporter = &replay.exporter;
            if let Some(page) = selected_page.clone() {
                let has = store.frame_path(&page.run_id, &page.page_id).exists();
                tabs = tabs.child(if !has {
                    div().text_xs().text_color(muted).child("(no recording)").into_any_element()
                } else if exporter.busy() {
                    div().text_xs().text_color(muted).child("exporting…").into_any_element()
                } else {
                    let st = store.clone();
                    button("export")
                        .label("⬇ Export MP4")
                        .xsmall()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(r) = this.replay.as_mut() {
                                r.exporter.start(&page, &st);
                            }
                            cx.notify();
                        }))
                        .into_any_element()
                });
            }
            let dir = store.exports_dir();
            tabs = tabs.child(
                button("exports-dir")
                    .label("📂")
                    .xsmall()
                    .tooltip("Open exports folder")
                    .on_click(move |_, _, _| {
                        let _ = std::fs::create_dir_all(&dir);
                        let _ = std::process::Command::new("xdg-open").arg(&dir).spawn();
                    }),
            );
            if let Some(s) = exporter.status.clone() {
                tabs = tabs.child(div().text_xs().truncate().child(s));
            }
        }

        let body = match tab {
            InspectorTab::Output => self.render_output(w, window, cx).into_any_element(),
            InspectorTab::Network => self.render_network(w, cx).into_any_element(),
            InspectorTab::Console => self.render_console(w, cx).into_any_element(),
        };
        v_flex().size_full().child(tabs).child(div().flex_1().min_h_0().px_2().child(body))
    }

    /// Runner output as a log table: time, source, severity bar, message.
    fn render_output(&mut self, w: Which, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let Some(view) = self.view(w) else { return v_flex() };
        let Some(run) = view.selected_run.as_deref().and_then(|id| view.model.run(id)) else {
            return v_flex().child(div().p_2().text_sm().text_color(muted).child("select a run"));
        };
        // Runners and test environments write plenty of non-errors to stderr, so
        // colour by content and optionally fold stderr lines that repeat a lot.
        let mut repeats: HashMap<&str, usize> = HashMap::new();
        for l in run.output.iter().filter(|l| l.source == Source::Stderr) {
            *repeats.entry(l.text.as_str()).or_default() += 1;
        }
        let (hide, stream, follow, seen) =
            (view.hide_repeated_stderr, view.output_stream, view.output_follow, view.output_seen);
        let filter = self.filter_input.read(cx).value().to_lowercase();
        let started = run.started_at;
        let rows: Rc<Vec<(f64, Source, bool, SharedString)>> = Rc::new(
            run.output
                .iter()
                .filter(|l| match stream {
                    StreamFilter::All => true,
                    StreamFilter::Stdout => l.source == Source::Stdout,
                    StreamFilter::Stderr => l.source == Source::Stderr,
                })
                .filter(|l| {
                    !(hide
                        && l.source == Source::Stderr
                        && repeats.get(l.text.as_str()).is_some_and(|n| *n > REPEAT_THRESHOLD))
                })
                .filter(|l| filter.is_empty() || l.text.to_lowercase().contains(&filter))
                .map(|l| ((l.at - started) / 1000.0, l.source, looks_like_failure(&l.text), SharedString::from(l.text.clone())))
                .collect(),
        );
        let mut folded: Vec<(&str, usize)> =
            repeats.iter().filter(|(_, n)| **n > REPEAT_THRESHOLD).map(|(l, n)| (*l, *n)).collect();
        folded.sort_by_key(|f| std::cmp::Reverse(f.1));
        let folded_total: usize = folded.iter().map(|(_, n)| n).sum();
        let folded_detail = folded.iter().map(|(l, n)| format!("{n} × {l}")).collect::<Vec<_>>().join("\n");
        let count = rows.len();

        // Follow the tail; a manual scroll away from the bottom turns it off.
        if follow {
            if count != seen {
                self.output_scroll.scroll_to_bottom();
            } else if self.output_scroll.is_scrolled_to_end() == Some(false)
                && let Some(v) = self.view_mut(w)
            {
                v.output_follow = false;
            }
        }
        if let Some(v) = self.view_mut(w) {
            v.output_seen = count;
        }

        let mut chips = h_flex().gap_1();
        for (f, name) in [(StreamFilter::All, "All"), (StreamFilter::Stdout, "stdout"), (StreamFilter::Stderr, "stderr")] {
            chips = chips.child(
                button(SharedString::from(format!("os-{name}-{}", w as usize)))
                    .label(name)
                    .xsmall()
                    .ghost()
                    .selected(stream == f)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(v) = this.view_mut(w) {
                            v.output_stream = f;
                        }
                        cx.notify();
                    })),
            );
        }
        let toolbar = h_flex()
            .gap_2()
            .py_1()
            .items_center()
            .child(div().w(px(220.)).child(Input::new(&self.filter_input).xsmall()))
            .child(chips)
            .child(
                Checkbox::new(("hide-repeats", w as usize))
                    .cursor_pointer()
                    .label("Hide repeated stderr")
                    .checked(hide)
                    .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                        if let Some(v) = this.view_mut(w) {
                            v.hide_repeated_stderr = *checked;
                        }
                        cx.notify();
                    })),
            )
            .when(hide && folded_total > 0, |row| {
                row.child(
                    div()
                        .id("folded")
                        .text_xs()
                        .text_color(muted)
                        .child(format!("{folded_total} hidden"))
                        .tooltip(tooltip_text(folded_detail)),
                )
            })
            .child(div().flex_1())
            .child(
                button(("follow", w as usize))
                    .label("● Live")
                    .xsmall()
                    .when(follow, |b| b.primary())
                    .when(!follow, |b| b.ghost())
                    .tooltip("Follow new output (turns off when you scroll up)")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(v) = this.view_mut(w) {
                            v.output_follow = !v.output_follow;
                            if v.output_follow {
                                this.output_scroll.scroll_to_bottom();
                            }
                        }
                        cx.notify();
                    })),
            );

        let mono = crate::fonts::mono();
        let list = uniform_list(("output", w as usize), count, move |range, _, _| {
            range
                .map(|i| {
                    let (rel, source, failure, text) = &rows[i];
                    let (bar, src_color) = match source {
                        _ if *failure => (theme.red, theme.red),
                        Source::Stderr => (theme.yellow.opacity(0.6), theme.yellow),
                        Source::Testudo => (theme.blue.opacity(0.6), theme.blue),
                        Source::Adapter => (theme.yellow, theme.yellow),
                        Source::Stdout => (theme.transparent, muted),
                    };
                    h_flex()
                        .w_full()
                        .h(px(20.))
                        .gap_2()
                        .items_center()
                        .border_l_2()
                        .border_color(bar)
                        .when(*failure, |d| d.bg(theme.red.opacity(0.08)))
                        .child(
                            div()
                                .w(px(64.))
                                .flex_none()
                                .pl_1()
                                .font_family(mono.clone())
                                .text_xs()
                                .text_color(muted)
                                .child(format!("{rel:>7.2}s")),
                        )
                        .child(
                            div()
                                .w(px(52.))
                                .flex_none()
                                .text_xs()
                                .text_color(src_color)
                                .child(source.label()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .whitespace_nowrap()
                                .overflow_hidden()
                                .font_family(mono.clone())
                                .text_xs()
                                .text_color(if *failure { theme.red } else { theme.foreground })
                                .child(text.clone()),
                        )
                })
                .collect()
        })
        .track_scroll(&self.output_scroll)
        .flex_1();

        v_flex().size_full().child(toolbar).child(list)
    }

    fn render_network(&mut self, w: Which, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(view) = self.view(w) else { return v_flex() };
        let muted = cx.theme().muted_foreground;
        let Some(page) = view.selected_page.as_ref().and_then(|k| view.model.pages.get(k)) else {
            return v_flex().child(div().text_sm().text_color(muted).child("click a page to inspect it"));
        };
        let text = cx.theme().foreground;
        let red = cx.theme().red;
        let rows: Rc<Vec<(Hsla, SharedString)>> = Rc::new(
            page.network
                .iter()
                .map(|r| {
                    let status = match (&r.status, &r.error) {
                        (Some(s), _) => s.to_string(),
                        (None, Some(e)) => format!("ERR {e}"),
                        (None, None) => "…".into(),
                    };
                    let ms = r.finished.map(|f| format!("{:.0}ms", f - r.started)).unwrap_or_default();
                    let c = if r.error.is_some() || r.status.is_some_and(|s| s >= 400) { red } else { text };
                    let line = format!("{:<6} {:>5} {:>7} {:<10} {}", r.method, status, ms, r.resource_type, truncate(&r.url, 200));
                    (c, SharedString::from(line))
                })
                .collect(),
        );
        let count = rows.len();
        v_flex().size_full().child(
            uniform_list(("net", w as usize), count, move |range, _, _| {
                range.map(|i| mono_row(rows[i].1.clone(), rows[i].0)).collect()
            })
            .flex_1(),
        )
    }

    fn render_console(&mut self, w: Which, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(view) = self.view(w) else { return v_flex() };
        let muted = cx.theme().muted_foreground;
        let (text, red, gold) = (cx.theme().foreground, cx.theme().red, cx.theme().yellow);
        let Some(page) = view.selected_page.as_ref().and_then(|k| view.model.pages.get(k)) else {
            return v_flex().child(div().text_sm().text_color(muted).child("click a page to inspect it"));
        };
        let rows: Rc<Vec<(Hsla, SharedString)>> = Rc::new(
            page.console
                .iter()
                .map(|c| {
                    let color = match c.level.as_str() {
                        "error" => red,
                        "warning" | "warn" => gold,
                        _ => text,
                    };
                    (color, SharedString::from(format!("[{}] {}", c.level, c.text)))
                })
                .collect(),
        );
        let count = rows.len();
        v_flex().size_full().child(
            uniform_list(("console", w as usize), count, move |range, _, _| {
                range.map(|i| mono_row(rows[i].1.clone(), rows[i].0)).collect()
            })
            .track_scroll(&self.console_scroll)
            .flex_1(),
        )
    }

    // --- history --------------------------------------------------------------

    fn render_history(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        if self.store.is_none() {
            return div().size_full().child("History store unavailable.").into_any_element();
        }
        let current = self.replay.as_ref().map(|r| r.run.id.clone());
        let mut list = v_flex().gap_px();
        if self.history.is_empty() {
            list = list.child(div().text_sm().text_color(muted).child("No recorded runs yet."));
        }
        let now = now_ms();
        for (i, r) in self.history.iter().enumerate() {
            let (status, tn) = match r.status.as_str() {
                "passed" => ("Passed", Tone::Passed),
                "running" => ("Running", Tone::Running),
                "interrupted" => ("Interrupted", Tone::Warn),
                "killed" => ("Killed", Tone::Failed),
                _ => ("Failed", Tone::Failed),
            };
            let id = r.id.clone();
            let duration = r.ended_at.map(|e| fmt_dur(e - r.started_at)).unwrap_or_default();
            let theme = cx.theme().clone();
            list = list.child(
                v_flex()
                    .id(("hist", i))
                    .px_2()
                    .py_1()
                    .gap_px()
                    .rounded_md()
                    .cursor_pointer()
                    .when(current.as_deref() == Some(r.id.as_str()), |d| d.bg(theme.list_active))
                    .hover(|d| d.bg(theme.list_hover))
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(pill(status, tone(tn, cx)))
                            .child(div().flex_1().min_w_0().truncate().text_sm().child(r.label.clone()))
                            .child(div().flex_none().text_xs().font_family(crate::fonts::mono()).text_color(muted).child(duration)),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .text_xs()
                            .text_color(muted)
                            .child(format!("✔ {}", r.passed))
                            .when(r.failed > 0, |d| d.child(div().text_color(theme.red).child(format!("✘ {}", r.failed))))
                            .when(r.skipped > 0, |d| d.child(format!("⊘ {}", r.skipped)))
                            .child(div().flex_1())
                            .child(format!("{} · {}", fmt_ago(r.started_at, now), fmt_time(r.started_at))),
                    )
                    .tooltip(tooltip_text(format!("{}\n{}", r.id, r.cwd)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_replay(&id);
                        cx.notify();
                    })),
            );
        }
        let sidebar = v_flex()
            .size_full()
            .p_2()
            .gap_1()
            .child(
                h_flex()
                    .gap_2()
                    .child(div().text_base().font_weight(FontWeight::SEMIBOLD).child("History"))
                    .child(button("hist-refresh").label("⟳").xsmall().tooltip("Refresh").on_click(cx.listener(
                        |this, _, _, cx| {
                            this.refresh_history();
                            cx.notify();
                        },
                    ))),
            )
            .children(self.store.as_ref().map(|s| div().text_xs().text_color(muted).truncate().child(s.dir().display().to_string())))
            .child(self.render_history_summary(cx))
            .child(
                div()
                    .id("hist-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .on_scroll(cx.listener(|this, _: &ScrollEvent, _, _| this.perf.input("history_list")))
                    .child(list),
            )
            .when(self.replay.is_some(), |d| {
                d.child(div().h(px(1.)).bg(cx.theme().border))
                    .child(self.run_list(Which::Replay, cx))
            });

        let replay_focus = self.replay.as_ref().is_some_and(|r| r.view.focus.is_some());
        let center: AnyElement = if replay_focus {
            self.render_focus(Which::Replay, window, cx).into_any_element()
        } else if self.replay.is_some() {
            v_flex()
                .size_full()
                .child(self.render_timeline(cx))
                .child(div().flex_1().min_h_0().child(self.render_pages(Which::Replay, window, cx)))
                .into_any_element()
        } else {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(muted)
                .child("Select a run to replay it.")
                .into_any_element()
        };
        let inspector: AnyElement = if self.replay.is_some() && !replay_focus {
            self.render_inspector(Which::Replay, window, cx).into_any_element()
        } else {
            div().into_any_element()
        };

        h_resizable("history-main")
            .child(resizable_panel().size(px(360.)).size_range(px(220.)..px(700.)).child(sidebar))
            .child(
                resizable_panel().child(if replay_focus {
                    center
                } else {
                    v_resizable("history-center")
                        .child(resizable_panel().child(center))
                        .child(resizable_panel().size(px(240.)).size_range(px(90.)..px(700.)).child(inspector))
                        .into_any_element()
                }),
            )
            .into_any_element()
    }

    /// Pass rate, flaky tests and a recent-runs sparkline above the History list.
    fn render_history_summary(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let recent: Vec<&RunRow> =
            self.history.iter().filter(|r| r.status != "running").take(SUMMARY_RUNS).collect();
        let passed = recent.iter().filter(|r| r.status == "passed").count();
        let rate = (!recent.is_empty()).then(|| passed as f64 / recent.len() as f64 * 100.0);
        let rate_color = match rate {
            Some(r) if r >= 90.0 => theme.green,
            Some(r) if r >= 70.0 => theme.yellow,
            Some(_) => theme.red,
            None => muted,
        };
        let card = |id: &'static str| {
            v_flex()
                .id(id)
                .flex_1()
                .min_w_0()
                .h(px(76.))
                .p_2()
                .gap_px()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .bg(theme.secondary.opacity(0.35))
        };
        let caption = |t: &str| div().text_xs().text_color(muted).truncate().child(t.to_owned());
        let big = |t: String, c: Hsla| div().text_lg().font_weight(FontWeight::SEMIBOLD).text_color(c).child(t);

        let flaky_detail = if self.flaky.is_empty() {
            "No test both passed and failed in the recent runs.".to_owned()
        } else {
            self.flaky
                .iter()
                .take(20)
                .map(|f| format!("✔{} ✘{}  {}", f.passes, f.fails, f.title))
                .collect::<Vec<_>>()
                .join("\n")
        };

        // Sparkline: oldest → newest, bar height by duration, colour by result.
        let spark: Vec<(f32, Hsla)> = {
            let runs: Vec<&&RunRow> = recent.iter().take(SPARK_RUNS).collect();
            let max = runs
                .iter()
                .filter_map(|r| r.ended_at.map(|e| e - r.started_at))
                .fold(1.0_f64, f64::max);
            runs.iter()
                .rev()
                .map(|r| {
                    let d = r.ended_at.map_or(0.0, |e| e - r.started_at);
                    let c = match r.status.as_str() {
                        "passed" => theme.green,
                        "interrupted" => theme.yellow,
                        _ => theme.red,
                    };
                    (((d / max) as f32).clamp(0.08, 1.0), c)
                })
                .collect()
        };
        let spark_n = spark.len();

        h_flex()
            .gap_2()
            .child(
                card("sum-rate")
                    .child(caption("Pass rate"))
                    .child(big(rate.map_or("—".to_owned(), |r| format!("{r:.0}%")), rate_color))
                    .child(caption(&format!("{passed} of {} runs", recent.len()))),
            )
            .child(
                card("sum-flaky")
                    .child(caption("Flaky tests"))
                    .child(big(self.flaky.len().to_string(), if self.flaky.is_empty() { theme.foreground } else { theme.yellow }))
                    .child(caption(&format!("last {SUMMARY_RUNS} runs")))
                    .tooltip(tooltip_text(flaky_detail)),
            )
            .child(
                card("sum-spark")
                    .child(caption("Recent runs"))
                    .child(
                        canvas(
                            |_, _, _| {},
                            move |bounds, _, window, _| {
                                if spark.is_empty() {
                                    return;
                                }
                                let slot = bounds.size.width / spark.len() as f32;
                                let bar = (slot - px(2.)).max(px(1.));
                                for (i, (h, c)) in spark.iter().enumerate() {
                                    let height = bounds.size.height * *h;
                                    let origin = point(bounds.left() + slot * i as f32, bounds.bottom() - height);
                                    window.paint_quad(fill(Bounds::new(origin, size(bar, height)), *c));
                                }
                            },
                        )
                        .w_full()
                        .h(px(26.)),
                    )
                    .tooltip(tooltip_text(format!("duration of the last {spark_n} runs, oldest → newest; colour = result"))),
            )
    }

    fn render_timeline(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(replay) = self.replay.as_ref() else { return v_flex() };
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let span = (replay.end - replay.start).max(1.0);
        let lanes = replay.lanes;
        let lane_h = (80.0 / lanes as f32).clamp(2.0, 10.0);
        let height = 14.0 + lane_h * lanes as f32;
        let frac = |x: f64| ((x - replay.start) / span).clamp(0.0, 1.0) as f32;
        let spans: Vec<(usize, f32, f32, Tone)> = replay
            .spans
            .iter()
            .map(|s| (s.lane, frac(s.start), frac(s.end), status_icon(s.status).1))
            .collect();
        let colors: HashMap<u8, Hsla> = [
            (0, tone(Tone::Running, cx)),
            (1, tone(Tone::Passed, cx)),
            (2, tone(Tone::Failed, cx)),
            (3, tone(Tone::Muted, cx)),
            (4, tone(Tone::Warn, cx)),
        ]
        .into_iter()
        .collect();
        let tone_key = |t: Tone| match t {
            Tone::Running => 0u8,
            Tone::Passed => 1,
            Tone::Failed => 2,
            Tone::Muted => 3,
            Tone::Warn => 4,
        };
        let spans: Vec<(usize, f32, f32, Hsla)> =
            spans.into_iter().map(|(l, a, b, t)| (l, a, b, colors[&tone_key(t)].opacity(0.65))).collect();
        let cursor = frac(replay.t);
        let cell = self.timeline_scrub.clone();
        let (playing, speed, keep) = (replay.playing, replay.speed, replay.view.keep_closed_pages);
        let time = format!("{} / {}", fmt_secs(replay.t - replay.start), fmt_secs(replay.end - replay.start));
        let hovered = self
            .timeline_hover
            .and_then(|i| replay.spans.get(i))
            .map(|s| format!("{} {}  ({})", status_icon(s.status).0, s.title, fmt_secs(s.end - s.start)))
            .unwrap_or_default();

        v_flex()
            .p_2()
            .gap_1()
            .child(
                h_flex()
                    .gap_2()
                    .child(button("tl-start").icon(media_icon("skip-back")).small().ghost().tooltip("Start").on_click(cx.listener(
                        |this, _, _, cx| {
                            if let (Some(r), Some(store)) = (this.replay.as_mut(), this.store.clone()) {
                                let s = r.start;
                                r.seek(s, &store, &mut this.retired);
                            }
                            cx.notify();
                        },
                    )))
                    .child(button("tl-play")
                        .icon(media_icon(if playing { "pause" } else { "play" }))
                        .small()
                        .primary()
                        .rounded(ButtonRounded::Large)
                        .tooltip("Play / pause")
                        .on_click(cx.listener(
                        |this, _, _, cx| {
                            if let (Some(r), Some(store)) = (this.replay.as_mut(), this.store.clone()) {
                                if !r.playing && r.t >= r.end {
                                    let s = r.start;
                                    r.seek(s, &store, &mut this.retired);
                                }
                                r.playing = !r.playing;
                            }
                            cx.notify();
                        },
                    )))
                    .child(speed_menu("tl-speed", speed, cx, |this, s| {
                        if let Some(r) = this.replay.as_mut() {
                            r.speed = s;
                        }
                    }))
                    .child(div().font_family(crate::fonts::mono()).text_sm().child(time))
                    .child(
                        Checkbox::new("tl-keep")
                                .cursor_pointer()
                            .label("Keep closed pages")
                            .checked(keep)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                if let Some(r) = this.replay.as_mut() {
                                    r.view.keep_closed_pages = *checked;
                                }
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .id("timeline")
                    .cursor_pointer()
                    .w_full()
                    .h(px(height))
                    .rounded_sm()
                    .bg(theme.secondary)
                    .child(
                        canvas(
                            move |bounds, _, _| cell.set(bounds),
                            move |bounds, _, window, _| {
                                let (x0, y0) = (bounds.left(), bounds.top());
                                let wd = bounds.size.width;
                                for (lane, a, b, c) in &spans {
                                    let top = y0 + px(14.0 + lane_h * *lane as f32);
                                    let left = x0 + wd * *a;
                                    let width = (wd * (*b - *a)).max(px(2.));
                                    window.paint_quad(fill(Bounds::new(point(left, top + px(1.)), size(width, px((lane_h - 1.0).max(1.0)))), *c));
                                }
                                window.paint_quad(fill(Bounds::new(point(x0 + wd * cursor - px(1.), y0), size(px(2.), bounds.size.height)), theme.foreground));
                            },
                        )
                        .size_full(),
                    )
                    .on_mouse_down(MouseButton::Left, cx.listener(|this, e: &MouseDownEvent, _, cx| this.scrub_timeline(e.position.x, cx)))
                    .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, _, cx| {
                        if e.pressed_button == Some(MouseButton::Left) {
                            this.scrub_timeline(e.position.x, cx);
                        } else {
                            this.hover_timeline(e.position, cx);
                        }
                    })),
            )
            .child(div().h(px(16.)).text_xs().text_color(muted).truncate().child(hovered))
    }

    /// Which test bar is under the mouse on the replay timeline.
    fn hover_timeline(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) {
        let b = self.timeline_scrub.get();
        let Some(r) = self.replay.as_ref() else { return };
        let lane_h = (80.0 / r.lanes as f32).clamp(2.0, 10.0);
        let fx = f32::from(pos.x - b.left()) / f32::from(b.size.width).max(1.0);
        let fy = f32::from(pos.y - b.top()) - 14.0;
        let t = r.start + fx as f64 * (r.end - r.start);
        let lane = (fy / lane_h).floor();
        let hit = (fy >= 0.0)
            .then(|| r.spans.iter().position(|s| s.lane as f32 == lane && s.start <= t && t <= s.end))
            .flatten();
        if hit != self.timeline_hover {
            self.timeline_hover = hit;
            cx.notify();
        }
    }

    fn scrub_timeline(&mut self, x: Pixels, cx: &mut Context<Self>) {
        let b = self.timeline_scrub.get();
        let frac = (f32::from(x - b.left()) / f32::from(b.size.width).max(1.0)).clamp(0.0, 1.0) as f64;
        if let (Some(r), Some(store)) = (self.replay.as_mut(), self.store.clone()) {
            r.playing = false;
            let t = r.start + frac * (r.end - r.start);
            r.seek(t, &store, &mut self.retired);
        }
        cx.notify();
    }
}

/// Height of every row in the run list (uniform, for virtualization).
const RUN_ROW_H: f32 = 24.;
/// Characters of an error shown in the error panel.
const MAX_ERROR_CHARS: usize = 20_000;

/// One row of the run list.
#[derive(Clone, Copy)]
enum ListRow {
    Header(usize),
    Counts(usize),
    Test(usize, usize),
    Note(usize, usize, NoteKind),
    Gap,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NoteKind {
    Step,
    Skip,
    Error,
}

/// Runs newest first; each test followed by its notes (current step, skip
/// reason, first error line). Whole errors open in the panel under the list.
fn run_rows(view: &PagesView) -> Vec<ListRow> {
    let mut rows = Vec::new();
    for (ri, run) in view.model.runs.iter().enumerate().rev() {
        rows.push(ListRow::Header(ri));
        rows.push(ListRow::Counts(ri));
        if view.collapsed_runs.contains(&run.id) {
            rows.push(ListRow::Gap);
            continue;
        }
        // Running, pending, failed, complete; by name within each.
        let mut order: Vec<usize> = (0..run.tests.len()).collect();
        order.sort_by_cached_key(|&ti| test_order(&run.tests[ti]));
        for ti in order {
            let t = &run.tests[ti];
            rows.push(ListRow::Test(ri, ti));
            if t.current_step.is_some() {
                rows.push(ListRow::Note(ri, ti, NoteKind::Step));
            }
            if t.status == Some(testudo_protocol::TestStatus::Skipped) && t.note.is_some() {
                rows.push(ListRow::Note(ri, ti, NoteKind::Skip));
            }
            if t.error.is_some() {
                rows.push(ListRow::Note(ri, ti, NoteKind::Error));
            }
        }
        rows.push(ListRow::Gap);
    }
    rows
}

/// Click handler: expand or collapse a failing test's error.
fn toggle_error(w: Which, test_id: String) -> impl Fn(&mut TestudoApp, &ClickEvent, &mut Window, &mut Context<TestudoApp>) {
    move |this, _, _, cx| {
        if let Some(v) = this.view_mut(w)
            && !v.expanded_tests.remove(&test_id)
        {
            v.expanded_tests.insert(test_id.clone());
        }
        cx.notify();
    }
}

