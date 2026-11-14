//! GUI-side state built from the hub's message stream.

use std::collections::HashMap;

use std::sync::Arc;

use gpui_kit::RenderImage;
use testudo_core::RunId;
use testudo_protocol::{Event, NetworkPhase, StdStream, TestStatus};

const MAX_LINES: usize = 5_000;
const MAX_ROWS: usize = 2_000;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PageKey {
    pub run_id: RunId,
    pub page_id: String,
}

pub struct Model {
    pub runs: Vec<Run>,
    pub pages: HashMap<PageKey, Page>,
}

pub struct Run {
    pub id: RunId,
    pub label: String,
    pub exit: Option<Option<i32>>,
    pub run_end: Option<String>,
    pub total_tests: Option<u32>,
    pub tests: Vec<Test>,
    /// When the run's first event arrived (hub receive time).
    pub started_at: f64,
    /// Runner output and adapter notes, in arrival order.
    pub output: Vec<OutputLine>,
    /// Runner name ("playwright", "vitest") and project directory, from
    /// RunStarted: enough to find the suite again to re-run a test.
    pub runner: String,
    pub cwd: String,
}

impl Run {
    pub fn is_running(&self) -> bool {
        self.exit.is_none()
    }

    pub fn test(&self, id: &str) -> Option<&Test> {
        self.tests.iter().find(|t| t.id == id)
    }

    fn test_mut(&mut self, id: &str) -> Option<&mut Test> {
        self.tests.iter_mut().find(|t| t.id == id)
    }

    /// (passed, failed, running, pending)
    pub fn counts(&self) -> (usize, usize, usize, usize) {
        let (mut passed, mut failed, mut running, mut pending) = (0, 0, 0, 0);
        for t in &self.tests {
            match t.status {
                _ if t.queued => pending += 1,
                Some(TestStatus::Passed) => passed += 1,
                Some(TestStatus::Failed | TestStatus::TimedOut) => failed += 1,
                None => running += 1,
                _ => {}
            }
        }
        (passed, failed, running, pending)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    Stdout,
    Stderr,
    /// Testudo's own notes in the run output (`[testudo] …`).
    Testudo,
    /// Messages from the injected adapters.
    Adapter,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Stdout => "stdout",
            Source::Stderr => "stderr",
            Source::Testudo => "testudo",
            Source::Adapter => "adapter",
        }
    }
}

pub struct OutputLine {
    pub at: f64,
    pub source: Source,
    pub text: String,
}

pub struct Test {
    pub id: String,
    pub title: String,
    pub title_path: Vec<String>,
    pub file: Option<String>,
    pub line: Option<u32>,
    /// Playwright project the test ran under.
    pub project: Option<String>,
    /// Announced by the runner but not started yet (pending).
    pub queued: bool,
    pub status: Option<TestStatus>,
    pub duration_ms: Option<f64>,
    pub error: Option<String>,
    pub current_step: Option<String>,
    /// Why the test was skipped, when the runner says.
    pub note: Option<String>,
    /// Every reported step (Playwright actions, expects, test.step), in start
    /// order, stamped with hub receive time like everything else.
    pub steps: Vec<StepRow>,
}

pub struct StepRow {
    pub id: String,
    pub title: String,
    pub category: Option<String>,
    pub started: f64,
    pub ended: Option<f64>,
    pub error: Option<String>,
}

pub struct Page {
    pub key: PageKey,
    pub test_id: Option<String>,
    pub browser: Option<String>,
    pub url: String,
    /// Latest frame. Replacing it must hand the old one to the retire queue
    /// (`window.drop_image`), or GPU atlas memory leaks.
    pub frame: Option<Arc<RenderImage>>,
    pub last_frame_ts: f64,
    pub frame_count: u64,
    /// Event time (ms since epoch, hub receive clock) the page opened.
    pub opened_at: f64,
    /// Event time (ms since epoch) the page closed.
    pub closed_at: Option<f64>,
    /// Main-frame navigations as (event time, url).
    pub navigations: Vec<(f64, String)>,
    pub network: Vec<NetRow>,
    pub console: Vec<ConsoleRow>,
}

pub struct NetRow {
    pub request_id: String,
    pub method: String,
    pub url: String,
    pub resource_type: String,
    pub status: Option<u16>,
    pub error: Option<String>,
    pub started: f64,
    pub finished: Option<f64>,
}

pub struct ConsoleRow {
    pub at: f64,
    pub level: String,
    pub text: String,
}

/// What the UI needs to do after applying an event.
pub enum Effect {
    None,
    PageOpened,
}

impl Model {
    pub fn new() -> Self {
        Self {
            runs: Vec::new(),
            pages: HashMap::new(),
        }
    }

    fn ensure_run(&mut self, id: &str, at: f64) -> &mut Run {
        if let Some(i) = self.runs.iter().position(|r| r.id == id) {
            return &mut self.runs[i];
        }
        self.runs.push(Run {
            id: id.to_owned(),
            label: id.to_owned(),
            exit: None,
            run_end: None,
            total_tests: None,
            tests: Vec::new(),
            started_at: at,
            output: Vec::new(),
            runner: String::new(),
            cwd: String::new(),
        });
        self.runs.last_mut().unwrap()
    }

    pub fn run(&self, id: &str) -> Option<&Run> {
        self.runs.iter().find(|r| r.id == id)
    }

    pub fn test_for_page(&self, page: &Page) -> Option<&Test> {
        let id = page.test_id.as_deref()?;
        self.run(&page.key.run_id)?.test(id)
    }

    /// Apply one event observed at `at` (hub receive time, ms since epoch).
    pub fn apply(&mut self, run_id: &str, at: f64, event: Event) -> Effect {
        self.ensure_run(run_id, at);
        let page_key = |page_id: &str| PageKey {
            run_id: run_id.to_owned(),
            page_id: page_id.to_owned(),
        };

        match event {
            Event::PageOpened {
                page_id,
                test_id,
                browser,
                url,
                ..
            } => {
                let key = page_key(&page_id);
                self.pages.insert(
                    key.clone(),
                    Page {
                        key: key.clone(),
                        test_id,
                        browser,
                        url: url.unwrap_or_default(),
                        frame: None,
                        last_frame_ts: 0.0,
                        frame_count: 0,
                        opened_at: at,
                        closed_at: None,
                        navigations: Vec::new(),
                        network: Vec::new(),
                        console: Vec::new(),
                    },
                );
                return Effect::PageOpened;
            }
            Event::PageClosed { page_id } => {
                if let Some(p) = self.pages.get_mut(&page_key(&page_id)) {
                    p.closed_at.get_or_insert(at);
                }
            }
            Event::Navigated { page_id, url } => {
                if let Some(p) = self.pages.get_mut(&page_key(&page_id)) {
                    p.navigations.push((at, url.clone()));
                    p.url = url;
                }
            }
            Event::Network {
                page_id,
                request_id,
                phase,
                ts: _,
                method,
                url,
                status,
                resource_type,
                error,
            } => {
                if let Some(p) = self.pages.get_mut(&page_key(&page_id)) {
                    match phase {
                        NetworkPhase::Request => push_capped(
                            &mut p.network,
                            NetRow {
                                request_id,
                                method: method.unwrap_or_default(),
                                url: url.unwrap_or_default(),
                                resource_type: resource_type.unwrap_or_default(),
                                status: None,
                                error: None,
                                started: at,
                                finished: None,
                            },
                            MAX_ROWS,
                        ),
                        NetworkPhase::Response | NetworkPhase::Failed => {
                            if let Some(row) = p.network.iter_mut().rev().find(|r| r.request_id == request_id) {
                                row.status = status.or(row.status);
                                row.error = error.or(row.error.take());
                                row.finished = Some(at);
                            }
                        }
                    }
                }
            }
            Event::Console {
                page_id, level, text, ..
            } => {
                if let Some(p) = self.pages.get_mut(&page_key(&page_id)) {
                    push_capped(&mut p.console, ConsoleRow { at, level, text }, MAX_ROWS);
                }
            }
            other => apply_run_event(self.ensure_run(run_id, at), at, other),
        }
        Effect::None
    }

    /// Install a decoded frame. Returns the frame it replaced, which the caller
    /// must retire.
    pub fn apply_frame(&mut self, key: &PageKey, at: f64, image: Arc<RenderImage>) -> Option<Arc<RenderImage>> {
        let Some(page) = self.pages.get_mut(key) else {
            return Some(image);
        };
        if at < page.last_frame_ts {
            return Some(image);
        }
        page.last_frame_ts = at;
        page.frame_count += 1;
        page.frame.replace(image)
    }

    /// Take every frame out of the model, for retiring before it is dropped.
    pub fn take_frames(&mut self) -> Vec<Arc<RenderImage>> {
        self.pages.values_mut().filter_map(|p| p.frame.take()).collect()
    }
}

fn apply_run_event(run: &mut Run, at: f64, event: Event) {
    match event {
        Event::RunBegin { total_tests } => run.total_tests = total_tests,
        Event::TestBegin {
            test_id,
            title,
            title_path,
            file,
            line,
            project,
            ..
        } => {
            // Queued tests start here; retries reuse the test id: reset in place.
            if let Some(t) = run.test_mut(&test_id) {
                t.queued = false;
                t.status = None;
                t.error = None;
                t.duration_ms = None;
                t.steps.clear();
                t.current_step = None;
                t.note = None;
            } else {
                run.tests.push(Test {
                    id: test_id,
                    title,
                    title_path,
                    file,
                    line,
                    project,
                    queued: false,
                    status: None,
                    duration_ms: None,
                    error: None,
                    current_step: None,
                    note: None,
                    steps: Vec::new(),
                });
            }
        }
        Event::TestQueued {
            test_id,
            title,
            title_path,
            file,
            line,
            project,
        } => {
            if run.test(&test_id).is_none() {
                run.tests.push(Test {
                    id: test_id,
                    title,
                    title_path,
                    file,
                    line,
                    project,
                    queued: true,
                    status: None,
                    duration_ms: None,
                    error: None,
                    current_step: None,
                    note: None,
                    steps: Vec::new(),
                });
            }
        }
        Event::StepBegin {
            test_id,
            step_id,
            title,
            category,
        } => {
            if let Some(t) = run.test_mut(&test_id) {
                t.current_step = Some(title.clone());
                push_capped(
                    &mut t.steps,
                    StepRow {
                        id: step_id,
                        title,
                        category,
                        started: at,
                        ended: None,
                        error: None,
                    },
                    MAX_ROWS,
                );
            }
        }
        Event::StepEnd {
            test_id,
            step_id,
            error,
            ..
        } => {
            if let Some(t) = run.test_mut(&test_id) {
                if let Some(s) = t.steps.iter_mut().rev().find(|s| s.id == step_id) {
                    s.ended = Some(at);
                    s.error = error;
                }
                // The innermost step still running, if any.
                t.current_step = t
                    .steps
                    .iter()
                    .rev()
                    .find(|s| s.ended.is_none())
                    .map(|s| s.title.clone());
            }
        }
        Event::TestEnd {
            test_id,
            status,
            duration_ms,
            error,
            note,
        } => {
            if let Some(t) = run.test_mut(&test_id) {
                t.status = Some(status);
                t.duration_ms = duration_ms;
                t.error = error;
                t.note = note;
                t.current_step = None;
            }
        }
        Event::RunEnd { status } => run.run_end = Some(status),
        Event::Stdio { stream, line } => {
            let source = match stream {
                _ if line.starts_with("[testudo]") => Source::Testudo,
                StdStream::Stdout => Source::Stdout,
                StdStream::Stderr => Source::Stderr,
            };
            push_capped(&mut run.output, OutputLine { at, source, text: line }, MAX_LINES)
        }
        Event::AdapterLog { level, message } => {
            let text = format!("[{level}] {message}");
            push_capped(
                &mut run.output,
                OutputLine {
                    at,
                    source: Source::Adapter,
                    text,
                },
                MAX_LINES,
            )
        }
        Event::RunStarted { label, runner, cwd, .. } => {
            run.label = label;
            run.runner = runner;
            run.cwd = cwd;
        }
        Event::ProcessExited { code } => run.exit = Some(code),
        Event::Hello { .. } => {}
        // Page-scoped events are handled by the caller.
        _ => {}
    }
}

fn push_capped<T>(v: &mut Vec<T>, item: T, cap: usize) {
    if v.len() >= cap {
        v.drain(..cap / 10);
    }
    v.push(item);
}
