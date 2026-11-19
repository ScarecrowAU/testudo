//! UI-agnostic rules: pane ordering, status classification, output heuristics.
//! Kept free of gpui so they are cheap to unit test.

use testudo_protocol::TestStatus;

use crate::model::{Model, Page, PageKey, Source, Test};

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

/// Run-list order of a test: running, pending, failed, then complete
/// (passed, skipped, interrupted); by name (case-insensitive) within a group.
pub fn test_order(t: &Test) -> (u8, String) {
    let group = match t.status {
        _ if t.queued => 1,
        None => 0,
        Some(TestStatus::Failed | TestStatus::TimedOut) => 2,
        Some(_) => 3,
    };
    (group, t.title.to_lowercase())
}

/// A running test that hasn't opened a page this long after starting is
/// shown as a terminal tile (most browser tests open one well within it).
pub const TERMINAL_GRACE_MS: f64 = 1000.0;

/// One tile of the page grid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tile {
    /// A browser page.
    Page(PageKey),
    /// A test without a page (API tests, setup, plain Vitest), shown as its
    /// terminal output. Indexes into `model.runs` and that run's tests.
    Terminal { run: usize, test: usize },
    /// A Vitest test file's page-less tests, one tile for the file. Indexes
    /// into `model.runs` and that run's tests, in the order they arrived.
    File { run: usize, tests: Vec<usize> },
}

/// What the page grid shows.
#[derive(Debug, Default)]
pub struct GridTiles {
    pub tiles: Vec<Tile>,
    /// Completed tiles left out by "Hide completed".
    pub hidden: usize,
    /// A running test may still open a page: look again shortly.
    pub waiting: bool,
}

fn test_state(t: &Test) -> PaneState {
    match t.status {
        None => PaneState::Running,
        Some(TestStatus::Failed | TestStatus::TimedOut) => PaneState::Failed,
        Some(_) => PaneState::Complete,
    }
}

/// The grid's tiles at time `now`: pages and page-less tests of `run` (every
/// run when None), running first, then failed, then complete, by name within
/// each. `hide_completed` leaves out completed ones (failures stay).
pub fn grid_tiles(model: &Model, run: Option<&str>, hide_completed: bool, now: f64) -> GridTiles {
    let in_run = |id: &str| run.is_none_or(|r| r == id);
    let mut keyed: Vec<((PaneState, String, u8, String), Tile)> = model
        .pages
        .values()
        .filter(|p| in_run(&p.key.run_id))
        .map(|p| {
            let key = (
                pane_state(p, model.test_for_page(p)),
                pane_name(model, p).to_lowercase(),
                0,
                p.key.page_id.clone(),
            );
            (key, Tile::Page(p.key.clone()))
        })
        .collect();
    let mut waiting = false;
    for (ri, r) in model.runs.iter().enumerate().filter(|(_, r)| in_run(&r.id)) {
        let pages: Vec<&Page> = model.pages.values().filter(|p| p.key.run_id == r.id).collect();
        // Vitest browser mode runs every test in one shared page: that page
        // is their visual output.
        if r.runner == "vitest" && pages.iter().any(|p| p.test_id.is_none()) {
            continue;
        }
        // Vitest: one tile per test file, holding its tests (queued ones too).
        let by_file = r.runner == "vitest";
        let mut files: Vec<(&str, Vec<usize>)> = Vec::new();
        for (ti, t) in r.tests.iter().enumerate() {
            let has_page = pages.iter().any(|p| p.test_id.as_deref() == Some(t.id.as_str()));
            if by_file
                && !has_page
                && let Some(file) = t.file.as_deref()
            {
                match files.iter_mut().find(|(f, _)| *f == file) {
                    Some((_, tests)) => tests.push(ti),
                    None => files.push((file, vec![ti])),
                }
                continue;
            }
            if t.queued || has_page {
                continue;
            }
            if t.status.is_none() && t.started_at.is_none_or(|s| now - s < TERMINAL_GRACE_MS) {
                waiting = true;
                continue;
            }
            let key = (test_state(t), t.title.to_lowercase(), 1, t.id.clone());
            keyed.push((key, Tile::Terminal { run: ri, test: ti }));
        }
        for (file, tests) in files {
            let members: Vec<&Test> = tests.iter().map(|&ti| &r.tests[ti]).collect();
            // Not started yet: the file appears with its first test.
            if members.iter().all(|t| t.queued) {
                continue;
            }
            let name = file.rsplit('/').next().unwrap_or(file).to_lowercase();
            let key = (file_state(&members), name, 1, file.to_owned());
            keyed.push((key, Tile::File { run: ri, tests }));
        }
    }
    let total = keyed.len();
    if hide_completed {
        keyed.retain(|(k, _)| k.0 != PaneState::Complete);
    }
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    GridTiles {
        hidden: total - keyed.len(),
        tiles: keyed.into_iter().map(|(_, t)| t).collect(),
        waiting,
    }
}

/// A test file's state: running while any of its tests is still to finish,
/// else failed if any failed.
fn file_state(tests: &[&Test]) -> PaneState {
    if tests.iter().any(|t| t.status.is_none()) {
        PaneState::Running
    } else if tests.iter().any(|t| test_state(t) == PaneState::Failed) {
        PaneState::Failed
    } else {
        PaneState::Complete
    }
}

/// A test file's overall status, for its tile's pill: None while running.
pub fn file_status(tests: &[&Test]) -> Option<TestStatus> {
    match file_state(tests) {
        PaneState::Running => None,
        PaneState::Failed => Some(TestStatus::Failed),
        PaneState::Complete if tests.iter().all(|t| t.status == Some(TestStatus::Skipped)) => Some(TestStatus::Skipped),
        PaneState::Complete if tests.iter().any(|t| t.status == Some(TestStatus::Interrupted)) => {
            Some(TestStatus::Interrupted)
        }
        PaneState::Complete => Some(TestStatus::Passed),
    }
}

/// A test file tile's lines: each test with its result, followed by what
/// it printed and the first line of its error. Shows `max` lines: the tail
/// while the file runs, from the first failure once it failed, else the top.
pub fn file_lines(tests: &[&Test], max: usize) -> Vec<(TermKind, String)> {
    let mut lines = Vec::new();
    let mut first_failure = None;
    for t in tests {
        let (icon, kind) = match t.status {
            _ if t.queued => ("·", TermKind::Note),
            None => ("›", TermKind::Stdout),
            Some(TestStatus::Passed) => ("✔", TermKind::Passed),
            Some(TestStatus::Failed | TestStatus::TimedOut) => ("✘", TermKind::Error),
            Some(s) => (status_icon(Some(s)).0, TermKind::Note),
        };
        if kind == TermKind::Error && first_failure.is_none() {
            first_failure = Some(lines.len());
        }
        let line = match t.duration_ms {
            Some(ms) => format!("{icon} {} ({})", t.title, fmt_dur(ms)),
            None => format!("{icon} {}", t.title),
        };
        lines.push((kind, line));
        for l in &t.output {
            let kind = match l.source {
                Source::Stderr => TermKind::Stderr,
                _ => TermKind::Stdout,
            };
            lines.push((kind, format!("  {}", l.text)));
        }
        if let Some(error) = &t.error {
            lines.push((TermKind::Error, format!("  {}", summary_line(error))));
        }
    }
    let max = max.max(1);
    let start = if tests.iter().any(|t| t.status.is_none()) {
        lines.len().saturating_sub(max)
    } else {
        first_failure.unwrap_or(0).min(lines.len().saturating_sub(max))
    };
    lines.into_iter().skip(start).take(max).collect()
}

/// The tests a log view shows, from the model.
pub fn log_tests<'a>(model: &'a Model, focus: &crate::state::LogFocus) -> Vec<&'a Test> {
    use crate::state::LogFocus;
    let (run_id, file, test_id) = match focus {
        LogFocus::File { run_id, file } => (run_id, Some(file.as_str()), None),
        LogFocus::Test { run_id, test_id } => (run_id, None, Some(test_id.as_str())),
    };
    let keep = |t: &Test| match (file, test_id) {
        (Some(f), _) => t.file.as_deref() == Some(f),
        (_, Some(id)) => t.id == id,
        _ => false,
    };
    model
        .run(run_id)
        .map(|r| r.tests.iter().filter(|t| keep(t)).collect())
        .unwrap_or_default()
}

/// Everything a log view shows for `tests`: each test's result, its group,
/// what it printed (else its steps), its skip reason and its whole error.
pub fn log_lines(tests: &[&Test]) -> Vec<(TermKind, String)> {
    let mut lines = Vec::new();
    for (i, t) in tests.iter().enumerate() {
        if i > 0 {
            lines.push((TermKind::Note, String::new()));
        }
        let (icon, kind) = match t.status {
            _ if t.queued => ("·", TermKind::Note),
            None => ("›", TermKind::Stdout),
            Some(TestStatus::Passed) => ("✔", TermKind::Passed),
            Some(TestStatus::Failed | TestStatus::TimedOut) => ("✘", TermKind::Error),
            Some(s) => (status_icon(Some(s)).0, TermKind::Note),
        };
        let state = match (t.queued, t.status, t.duration_ms) {
            (true, ..) => "pending".to_owned(),
            (_, s, Some(ms)) => format!("{}, {}", status_label(s).to_lowercase(), fmt_dur(ms)),
            (_, s, None) => status_label(s).to_lowercase(),
        };
        lines.push((kind, format!("{icon} {} ({state})", t.title)));
        if let [groups @ .., _] = t.title_path.as_slice()
            && !groups.is_empty()
        {
            lines.push((TermKind::Note, format!("  {}", groups.join(" › "))));
        }
        if t.output.is_empty() {
            for s in &t.steps {
                lines.push(match s.error {
                    Some(_) => (TermKind::StepFailed, format!("  ✘ {}", s.title)),
                    None => (TermKind::Step, format!("  › {}", s.title)),
                });
            }
        }
        for l in &t.output {
            let kind = match l.source {
                Source::Stderr => TermKind::Stderr,
                _ => TermKind::Stdout,
            };
            lines.push((kind, format!("  {}", l.text)));
        }
        if let Some(note) = t.note.as_deref().filter(|_| t.status == Some(TestStatus::Skipped)) {
            lines.push((TermKind::Note, format!("  skipped: {note}")));
        }
        if let Some(error) = &t.error {
            lines.extend(error.lines().map(|l| (TermKind::Error, format!("  {l}"))));
        }
    }
    lines
}

/// How a terminal tile line is coloured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TermKind {
    Stdout,
    Stderr,
    Passed,
    Step,
    StepFailed,
    Error,
    Note,
}

/// The last `max` lines of a page-less test's terminal tile: what it printed
/// (else its steps), then its skip reason or error.
pub fn terminal_lines(t: &Test, max: usize) -> Vec<(TermKind, String)> {
    let mut lines: Vec<(TermKind, String)> = if t.output.is_empty() {
        t.steps
            .iter()
            .map(|s| match s.error {
                Some(_) => (TermKind::StepFailed, format!("✘ {}", s.title)),
                None => (TermKind::Step, format!("› {}", s.title)),
            })
            .collect()
    } else {
        t.output
            .iter()
            .map(|l| {
                let kind = match l.source {
                    Source::Stderr => TermKind::Stderr,
                    _ => TermKind::Stdout,
                };
                (kind, l.text.clone())
            })
            .collect()
    };
    if let Some(note) = t.note.as_deref().filter(|_| t.status == Some(TestStatus::Skipped)) {
        lines.push((TermKind::Note, format!("skipped: {}", summary_line(note))));
    }
    if let Some(error) = &t.error {
        lines.extend(error.lines().map(|l| (TermKind::Error, l.to_owned())));
    }
    if lines.is_empty() {
        // Nothing printed: say where the test sits and how it went.
        if let [groups @ .., _] = t.title_path.as_slice()
            && !groups.is_empty()
        {
            lines.push((TermKind::Note, groups.join(" › ")));
        }
        let state = match (t.status, t.duration_ms) {
            (None, _) => "running…".to_owned(),
            (Some(s), Some(ms)) => format!("{} in {}", status_label(Some(s)).to_lowercase(), fmt_dur(ms)),
            (Some(s), None) => status_label(Some(s)).to_lowercase(),
        };
        lines.push((TermKind::Note, state));
        if t.status.is_some() {
            lines.push((TermKind::Note, "no console output".to_owned()));
        }
    }
    let skip = lines.len().saturating_sub(max);
    lines.split_off(skip)
}

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

/// Stderr lines repeated more than this many times in a run count as noise.
pub const REPEAT_THRESHOLD: usize = 3;

/// Heuristic for output lines worth highlighting, whichever stream they came on.
pub fn looks_like_failure(line: &str) -> bool {
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

/// Visual status of a test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Running,
    Passed,
    Failed,
    Muted,
    Warn,
}

pub fn status_icon(status: Option<TestStatus>) -> (&'static str, Tone) {
    match status {
        None => ("⏳", Tone::Running),
        Some(TestStatus::Passed) => ("✔", Tone::Passed),
        Some(TestStatus::Failed | TestStatus::TimedOut) => ("✘", Tone::Failed),
        Some(TestStatus::Interrupted) => ("⏹", Tone::Warn),
        Some(TestStatus::Skipped) => ("⊘", Tone::Muted),
        Some(TestStatus::Unknown) => ("?", Tone::Muted),
    }
}

pub fn status_label(status: Option<TestStatus>) -> &'static str {
    match status {
        None => "Running",
        Some(TestStatus::Passed) => "Passed",
        Some(TestStatus::Failed) => "Failed",
        Some(TestStatus::TimedOut) => "Timed out",
        Some(TestStatus::Skipped) => "Skipped",
        Some(TestStatus::Interrupted) => "Interrupted",
        Some(TestStatus::Unknown) => "Unknown",
    }
}

/// The path (and query) of a URL, or the whole string if it has none.
pub fn url_path(url: &str) -> &str {
    url.split_once("://")
        .and_then(|(_, rest)| rest.find('/').map(|i| &rest[i..]))
        .unwrap_or(url)
}

/// First non-empty line, without terminal colour codes: for one-line
/// summaries of runner messages (skip reasons carry whole call logs).
pub fn summary_line(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // ESC [ ... final-byte
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default()
        .to_owned()
}

/// Where another recording of the same test should be, by test step, when
/// this one is at `t`. Steps are `(start time, title)` in start order; the
/// current step is matched to the same occurrence of the same title in the
/// other recording, and the time into that step carried over (held at the
/// other step's end if this run is slower). Before the first step: time
/// since page open, held at the other's first step. `None` when the other
/// recording has no matching step; compare by time then.
pub fn align_by_step(
    cur: &[(f64, String)],
    other: &[(f64, String)],
    t: f64,
    cur_open: f64,
    other_open: f64,
) -> Option<f64> {
    let Some(k) = cur.iter().rposition(|s| s.0 <= t) else {
        let at = other_open + (t - cur_open);
        return Some(other.first().map_or(at, |s| at.min(s.0)));
    };
    let title = &cur[k].1;
    let occurrence = cur[..k].iter().filter(|s| &s.1 == title).count();
    let j = other
        .iter()
        .enumerate()
        .filter(|(_, s)| &s.1 == title)
        .nth(occurrence)
        .map(|(j, _)| j)?;
    let at = other[j].0 + (t - cur[k].0);
    Some(other.get(j + 1).map_or(at, |next| at.min(next.0 - 0.001)))
}

/// Runner arguments that select one test: `file:line` (relative to the
/// project directory, as both CLIs document it) and, for Playwright, the
/// project the test ran under. `None` when the runner can't filter that way.
pub fn rerun_args(runner: &str, cwd: &str, file: &str, line: u32, project: Option<&str>) -> Option<Vec<String>> {
    if !matches!(runner, "playwright" | "vitest") {
        return None;
    }
    let rel = std::path::Path::new(file)
        .strip_prefix(cwd)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| file.to_owned());
    let mut args = vec![format!("{rel}:{line}")];
    if runner == "playwright"
        && let Some(p) = project.filter(|p| !p.is_empty())
    {
        args.push(format!("--project={p}"));
    }
    Some(args)
}

/// Grid (columns, rows) for `n` recordings of shape `frame` (w, h) in an
/// `area` (w, h): the one that shows them largest. Each cell loses `header`
/// px to its label row, cells are `gap` px apart. Ties go to fewer rows.
pub fn best_grid(area: (f32, f32), frame: (f32, f32), n: usize, header: f32, gap: f32) -> (usize, usize) {
    let n = n.max(1);
    let (w, h) = area;
    let (fw, fh) = (frame.0.max(1.0), frame.1.max(1.0));
    let mut best = ((n, 1), -1.0_f32);
    for rows in 1..=n {
        let cols = n.div_ceil(rows);
        let cw = (w - gap * (cols - 1) as f32) / cols as f32;
        let ch = (h - gap * (rows - 1) as f32) / rows as f32 - header;
        let fit = (cw / fw).min(ch / fh).max(0.0);
        if fit > best.1 {
            best = ((cols, rows), fit);
        }
    }
    best.0
}

/// How the page panes are laid out: `cols` across, `rows` in all, each
/// `tile_h` px tall. Rows past the panel's height scroll.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PaneGrid {
    pub cols: usize,
    pub rows: usize,
    pub tile_h: f32,
}

/// Grid for `n` page panes in an `area` (w, h), panes `gap` px apart. Panes
/// never shrink below `min` (w, h): as many fit as the space allows (wider
/// rows first), the rest go in rows below the fold. While everything fits,
/// the panes fill the height, as before.
pub fn pane_grid(area: (f32, f32), n: usize, min: (f32, f32), gap: f32) -> PaneGrid {
    let n = n.max(1);
    let fit = |len: f32, min: f32| (((len + gap) / (min + gap)).floor() as usize).max(1);
    let (max_cols, max_rows) = (fit(area.0, min.0), fit(area.1, min.1));
    let mut cols = ((n as f32).sqrt().ceil() as usize).clamp(1, max_cols);
    while n.div_ceil(cols) > max_rows && cols < max_cols.min(n) {
        cols += 1;
    }
    let rows = n.div_ceil(cols);
    let shown = rows.min(max_rows);
    let tile_h = ((area.1 - gap * (shown - 1) as f32) / shown as f32).max(min.1);
    PaneGrid { cols, rows, tile_h }
}

/// Chronological number (1 = oldest) of the earlier run at `i` in a
/// newest-first list of `total`: the one just before this run is `total`.
pub fn run_number(i: usize, total: usize) -> usize {
    total.saturating_sub(i)
}

/// The comparison counter for a window of `count` earlier runs starting at
/// `index` (newest first): "oldest–newest of total" in chronological
/// numbers, so moving to older runs counts down.
pub fn compare_position(index: usize, count: usize, total: usize) -> String {
    let last = (index + count).min(total).saturating_sub(1);
    let (oldest, newest) = (run_number(last, total), run_number(index, total));
    if oldest == newest {
        format!("{newest} of {total}")
    } else {
        format!("{oldest}–{newest} of {total}")
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

pub fn fmt_secs(ms: f64) -> String {
    let s = (ms / 1000.0).max(0.0);
    format!("{:02}:{:06.3}", (s / 60.0) as u64, s % 60.0)
}

/// Local wall-clock `MM-DD HH:MM:SS` without a date-time dependency.
pub fn fmt_time(ms: f64) -> String {
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

/// "just now", "6m ago", "3h ago", "2d ago" relative to `now_ms`.
pub fn fmt_ago(ms: f64, now_ms: f64) -> String {
    let s = ((now_ms - ms) / 1000.0).max(0.0) as u64;
    match s {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", s / 60),
        3600..=86_399 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86_400),
    }
}

/// Compact duration: "820ms", "4.2s", "3m 05s".
pub fn fmt_dur(ms: f64) -> String {
    if ms < 1000.0 {
        format!("{ms:.0}ms")
    } else if ms < 60_000.0 {
        format!("{:.1}s", ms / 1000.0)
    } else {
        let s = (ms / 1000.0) as u64;
        format!("{}m {:02}s", s / 60, s % 60)
    }
}

pub use testudo_core::tilde;

/// `file:line`, or just `file` when the runner didn't report the line.
pub fn with_line(file: &str, line: Option<u32>) -> String {
    match line {
        Some(l) => format!("{file}:{l}"),
        None => file.to_owned(),
    }
}

/// Whether opening `dir` should skip the automatic suite scan: the home
/// folder (where Testudo starts when launched from Finder or the Dock) and the
/// filesystem root hold everything, so scanning them walks the whole disk.
/// Scan still searches them when asked.
pub fn skip_startup_scan(dir: &std::path::Path, home: Option<&std::path::Path>) -> bool {
    let canon = |p: &std::path::Path| p.canonicalize().unwrap_or_else(|_| p.to_owned());
    let dir = canon(dir);
    dir.parent().is_none() || home.is_some_and(|h| canon(h) == dir)
}

pub fn expand_tilde(p: &str) -> String {
    match (p.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => p.to_owned(),
    }
}

/// Suites waiting to run, by suite id, in order. One suite runs at a time:
/// the next one starts when nothing is running.
#[derive(Debug, Default, Clone)]
pub struct SuiteQueue(Vec<String>);

impl SuiteQueue {
    /// Add `id` at the back. False if it is already waiting.
    pub fn push(&mut self, id: &str) -> bool {
        if self.contains(id) {
            return false;
        }
        self.0.push(id.to_owned());
        true
    }

    /// Take `id` out of the queue. False if it wasn't waiting.
    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.0.len();
        self.0.retain(|q| q != id);
        self.0.len() != before
    }

    pub fn contains(&self, id: &str) -> bool {
        self.0.iter().any(|q| q == id)
    }

    /// 0-based place in line.
    pub fn position(&self, id: &str) -> Option<usize> {
        self.0.iter().position(|q| q == id)
    }

    pub fn pop_front(&mut self) -> Option<String> {
        (!self.0.is_empty()).then(|| self.0.remove(0))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use testudo_protocol::Event;

    #[test]
    fn suite_queue_keeps_order_and_skips_duplicates() {
        let mut q = SuiteQueue::default();
        assert!(q.push("a"));
        assert!(q.push("b"));
        assert!(!q.push("a"));
        assert!(q.push("c"));
        assert_eq!(q.len(), 3);
        assert_eq!(q.position("b"), Some(1));
        assert!(q.remove("b"));
        assert!(!q.remove("b"));
        assert_eq!(q.pop_front().as_deref(), Some("a"));
        assert_eq!(q.pop_front().as_deref(), Some("c"));
        assert_eq!(q.pop_front(), None);
        assert!(q.is_empty());
    }

    #[test]
    fn failure_heuristic() {
        assert!(!looks_like_failure("Not implemented: Window's scrollTo() method"));
        assert!(!looks_like_failure(
            "(node:1) ExperimentalWarning: localStorage is not available"
        ));
        assert!(!looks_like_failure("      Tests  632 passed (632)"));
        assert!(!looks_like_failure("  11 passed, 0 failed"));
        assert!(looks_like_failure("  1 failed"));
        assert!(looks_like_failure(
            "Error: page.waitForURL: Test timeout of 30000ms exceeded."
        ));
        assert!(looks_like_failure(" × fails on purpose 117ms"));
        assert!(looks_like_failure("AssertionError: expected 2 to be 3"));
    }

    #[test]
    fn summary_line_strips_codes_and_takes_first_line() {
        let note = "backend unreachable: connect ECONNREFUSED ::1:3100\nCall log:\n\x1b[2m  - → GET /healthz\x1b[22m";
        assert_eq!(summary_line(note), "backend unreachable: connect ECONNREFUSED ::1:3100");
        assert_eq!(summary_line("\n  \x1b[31mred\x1b[39m  \n"), "red");
        assert_eq!(summary_line(""), "");
    }

    #[test]
    fn step_alignment() {
        let steps = |v: &[(f64, &str)]| v.iter().map(|(t, s)| (*t, s.to_string())).collect::<Vec<_>>();
        // This run: opened at 0, click at 100, fill at 200, click at 300.
        let cur = steps(&[(100.0, "click"), (200.0, "fill"), (300.0, "click")]);
        // Other run (opened at 1000) is slower: click 1500, fill 1900, click 2600.
        let other = steps(&[(1500.0, "click"), (1900.0, "fill"), (2600.0, "click")]);
        let at = |t| align_by_step(&cur, &other, t, 0.0, 1000.0);
        assert_eq!(at(50.0), Some(1050.0), "before steps: time since open");
        assert_eq!(at(99.0), Some(1099.0));
        assert_eq!(at(210.0), Some(1910.0), "10ms into fill");
        assert_eq!(at(320.0), Some(2620.0), "second click, not the first");
        // Before the first step, held at the other's first step.
        assert_eq!(
            align_by_step(&cur, &steps(&[(1020.0, "click")]), 50.0, 0.0, 1000.0),
            Some(1020.0)
        );
        // Other run is faster: held just before its next step.
        let fast = steps(&[(1100.0, "click"), (1150.0, "fill")]);
        assert_eq!(align_by_step(&cur, &fast, 190.0, 0.0, 1000.0), Some(1149.999));
        // No matching step.
        assert_eq!(
            align_by_step(&cur, &steps(&[(1100.0, "goto")]), 150.0, 0.0, 1000.0),
            None
        );
    }

    #[test]
    fn rerun_args_select_one_test() {
        assert_eq!(
            rerun_args("playwright", "/p/web", "/p/web/tests/a.spec.ts", 12, Some("chromium")),
            Some(vec!["tests/a.spec.ts:12".to_owned(), "--project=chromium".to_owned()])
        );
        assert_eq!(
            rerun_args("vitest", "/p", "/p/src/x.test.ts", 3, Some("ignored")),
            Some(vec!["src/x.test.ts:3".to_owned()])
        );
        assert_eq!(
            rerun_args("playwright", "/p", "/elsewhere/a.spec.ts", 1, None),
            Some(vec!["/elsewhere/a.spec.ts:1".to_owned()])
        );
        assert_eq!(rerun_args("command", "/p", "/p/a", 1, None), None);
    }

    #[test]
    fn comparison_grid_follows_the_space_and_page_shape() {
        let grid = |area, frame, n| best_grid(area, frame, n, 30.0, 8.0);
        // Two recordings: phone side by side in a wide pane, desktop stacked
        // in a tall or square one, desktop side by side in a very wide one.
        assert_eq!(grid((1000.0, 600.0), (390.0, 844.0), 2), (2, 1));
        assert_eq!(grid((600.0, 1000.0), (1280.0, 720.0), 2), (1, 2));
        assert_eq!(grid((900.0, 800.0), (1280.0, 720.0), 2), (1, 2));
        assert_eq!(grid((2000.0, 600.0), (1280.0, 720.0), 2), (2, 1));
        // Four desktop recordings in a square pane: 2×2; phones in a wide one: a row.
        assert_eq!(grid((1000.0, 900.0), (1280.0, 720.0), 4), (2, 2));
        assert_eq!(grid((1600.0, 600.0), (390.0, 844.0), 4), (4, 1));
        // Six (this run + five earlier) desktop recordings in a square pane: 2×3.
        assert_eq!(grid((1000.0, 900.0), (1280.0, 720.0), 6), (2, 3));
        // Three: never more cells than needed per row.
        assert_eq!(grid((900.0, 900.0), (1280.0, 720.0), 3), (1, 3));
    }

    #[test]
    fn startup_scan_skips_home_and_root_only() {
        let home = std::env::temp_dir().join(format!("testudo-home-{}", std::process::id()));
        let project = home.join("project");
        std::fs::create_dir_all(&project).unwrap();
        assert!(skip_startup_scan(&home, Some(&home)));
        // The same folder through a trailing `.` still counts as home.
        assert!(skip_startup_scan(&home.join("."), Some(&home)));
        assert!(skip_startup_scan(std::path::Path::new("/"), Some(&home)));
        assert!(!skip_startup_scan(&project, Some(&home)));
        assert!(!skip_startup_scan(&project, None));
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn page_panes_stop_shrinking_and_scroll() {
        let grid = |area, n| {
            let g = pane_grid(area, n, (260.0, 200.0), 8.0);
            (g.cols, g.rows, g.tile_h)
        };
        // Few panes: the usual square-ish grid, filling the height.
        assert_eq!(grid((1000.0, 800.0), 4), (2, 2, 396.0));
        assert_eq!(grid((1000.0, 800.0), 1), (1, 1, 800.0));
        // Many panes: at most 3 across (3×260 + 2×8 ≤ 1000) and 3 rows tall
        // (3×200 + 2×8 ≤ 800); the rest scroll, visible rows fill the height.
        let many = grid((1000.0, 800.0), 30);
        assert_eq!((many.0, many.1), (3, 10));
        assert!((many.2 - (800.0 - 16.0) / 3.0).abs() < 0.01);
        // Wide and short: use the width before scrolling.
        assert_eq!(grid((2000.0, 300.0), 6), (6, 1, 300.0));
        // Not measured yet (or tiny): one column at the minimum size.
        assert_eq!(grid((0.0, 0.0), 5), (1, 5, 200.0));
    }

    #[test]
    fn compare_counter_counts_in_time_order() {
        // The run just before this one is the highest number.
        assert_eq!(compare_position(0, 1, 28), "28 of 28");
        assert_eq!(compare_position(0, 4, 28), "25–28 of 28");
        // Five steps older: lower numbers.
        assert_eq!(compare_position(5, 4, 28), "20–23 of 28");
        // The oldest window, and fewer runs than the window.
        assert_eq!(compare_position(24, 4, 28), "1–4 of 28");
        assert_eq!(compare_position(0, 4, 2), "1–2 of 2");
        assert_eq!(run_number(0, 28), 28);
        assert_eq!(run_number(27, 28), 1);
    }

    #[test]
    fn url_paths() {
        assert_eq!(url_path("http://localhost:3000/feeds?view=all"), "/feeds?view=all");
        assert_eq!(url_path("https://example.com"), "https://example.com");
        assert_eq!(url_path("about:blank"), "about:blank");
    }

    #[test]
    fn relative_time_and_durations() {
        let now = 10_000_000.0;
        assert_eq!(fmt_ago(now - 5_000.0, now), "just now");
        assert_eq!(fmt_ago(now - 6.0 * 60_000.0, now), "6m ago");
        assert_eq!(fmt_ago(now - 3.0 * 3_600_000.0, now), "3h ago");
        assert_eq!(fmt_ago(now - 2.0 * 86_400_000.0, now), "2d ago");
        assert_eq!(fmt_dur(820.0), "820ms");
        assert_eq!(fmt_dur(4_210.0), "4.2s");
        assert_eq!(fmt_dur(185_000.0), "3m 05s");
    }

    #[test]
    fn rows_relative_to_scrub_time() {
        assert_eq!(row_state(10.0, Some(20.0), 5.0), RowState::Pending);
        assert_eq!(row_state(10.0, Some(20.0), 10.0), RowState::InFlight);
        assert_eq!(row_state(10.0, Some(20.0), 15.0), RowState::InFlight);
        assert_eq!(row_state(10.0, Some(20.0), 20.0), RowState::Done);
        assert_eq!(row_state(10.0, None, 99.0), RowState::InFlight, "no response yet");
    }

    #[test]
    fn panes_order_by_state_then_name() {
        let mut m = Model::new();
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
            note: None,
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
            m.apply("r1", i as f64, e);
        }
        let page_ids = |g: GridTiles| -> Vec<String> {
            g.tiles
                .into_iter()
                .map(|t| match t {
                    Tile::Page(k) => k.page_id,
                    Tile::Terminal { .. } | Tile::File { .. } => panic!("every test has a page"),
                })
                .collect()
        };
        assert_eq!(
            page_ids(grid_tiles(&m, None, false, 100.0)),
            ["p3", "p5", "p2", "p6", "p4", "p1"]
        );
        // Hide completed keeps running and failed tiles.
        let shown = grid_tiles(&m, Some("r1"), true, 100.0);
        assert_eq!(shown.hidden, 2);
        assert_eq!(page_ids(shown), ["p3", "p5", "p2", "p6"]);
        assert!(grid_tiles(&m, Some("other"), false, 100.0).tiles.is_empty());
    }

    fn begin(id: &str) -> Event {
        Event::TestBegin {
            test_id: id.into(),
            title: id.into(),
            title_path: vec![],
            file: None,
            line: None,
            project: None,
            worker_index: None,
        }
    }

    fn end(id: &str, status: TestStatus, error: Option<&str>) -> Event {
        Event::TestEnd {
            test_id: id.into(),
            status,
            duration_ms: None,
            error: error.map(Into::into),
            note: None,
        }
    }

    #[test]
    fn tests_without_pages_get_terminal_tiles() {
        let mut m = Model::new();
        m.apply(
            "r1",
            0.0,
            Event::RunStarted {
                label: "r1".into(),
                runner: "playwright".into(),
                cwd: "/p".into(),
                args: vec![],
                user_args: None,
            },
        );
        m.apply("r1", 0.0, begin("api"));
        m.apply("r1", 0.0, begin("ui"));
        m.apply("r1", 0.0, begin("slow"));
        m.apply(
            "r1",
            10.0,
            Event::PageOpened {
                page_id: "p1".into(),
                test_id: Some("ui".into()),
                worker_index: None,
                browser: None,
                url: None,
            },
        );
        m.apply("r1", 20.0, end("api", TestStatus::Passed, None));
        // Before the grace period a running test may still open a page.
        let early = grid_tiles(&m, None, false, 500.0);
        assert!(early.waiting);
        assert_eq!(early.tiles.len(), 2, "page and finished api test");
        let later = grid_tiles(&m, None, false, TERMINAL_GRACE_MS + 1.0);
        assert!(!later.waiting);
        assert_eq!(
            later.tiles,
            [
                Tile::Terminal { run: 0, test: 2 },
                Tile::Page(PageKey {
                    run_id: "r1".into(),
                    page_id: "p1".into()
                }),
                Tile::Terminal { run: 0, test: 0 },
            ],
            "running first (by name), then complete"
        );
    }

    #[test]
    fn vitest_browser_mode_has_no_terminal_tiles() {
        let mut m = Model::new();
        m.apply(
            "r1",
            0.0,
            Event::RunStarted {
                label: "r1".into(),
                runner: "vitest".into(),
                cwd: "/p".into(),
                args: vec![],
                user_args: None,
            },
        );
        m.apply(
            "r1",
            0.0,
            Event::PageOpened {
                page_id: "shared".into(),
                test_id: None,
                worker_index: None,
                browser: None,
                url: None,
            },
        );
        m.apply("r1", 1.0, begin("t1"));
        m.apply("r1", 2.0, end("t1", TestStatus::Passed, None));
        assert_eq!(grid_tiles(&m, None, false, 10_000.0).tiles.len(), 1);
    }

    #[test]
    fn output_sent_before_begin_survives_the_begin() {
        // Vitest's order: queued, console logs, then begin and end.
        let mut m = Model::new();
        m.apply(
            "r1",
            0.0,
            Event::TestQueued {
                test_id: "t".into(),
                title: "t".into(),
                title_path: vec![],
                file: None,
                line: None,
                project: None,
            },
        );
        let log = Event::TestOutput {
            test_id: "t".into(),
            stream: testudo_protocol::StdStream::Stdout,
            text: "hello\n".into(),
        };
        m.apply("r1", 1.0, log.clone());
        m.apply("r1", 1.0, begin("t"));
        m.apply("r1", 2.0, end("t", TestStatus::Passed, None));
        let t = m.runs[0].test("t").unwrap();
        assert_eq!(terminal_lines(t, 10), [(TermKind::Stdout, "hello".to_owned())]);

        // A retry starts clean.
        m.apply("r1", 3.0, begin("t"));
        assert!(m.runs[0].test("t").unwrap().output.is_empty());
    }

    #[test]
    fn vitest_tests_share_a_tile_per_file() {
        let mut m = Model::new();
        m.apply(
            "r1",
            0.0,
            Event::RunStarted {
                label: "r1".into(),
                runner: "vitest".into(),
                cwd: "/p".into(),
                args: vec![],
                user_args: None,
            },
        );
        let queued = |id: &str, file: &str| Event::TestQueued {
            test_id: id.into(),
            title: id.into(),
            title_path: vec![id.into()],
            file: Some(file.into()),
            line: None,
            project: None,
        };
        m.apply("r1", 0.0, queued("a1", "/p/a.test.ts"));
        m.apply("r1", 0.0, queued("b1", "/p/b.test.ts"));
        m.apply("r1", 0.0, queued("a2", "/p/a.test.ts"));
        m.apply("r1", 0.0, queued("c1", "/p/c.test.ts"));
        let ends = [
            ("a1", TestStatus::Passed, None),
            ("a2", TestStatus::Failed, Some("expected 1\nreceived 2")),
            ("b1", TestStatus::Passed, None),
        ];
        for (id, status, error) in ends {
            m.apply("r1", 1.0, begin(id));
            m.apply("r1", 2.0, end(id, status, error));
        }
        // c.test.ts hasn't started: no tile yet. a failed, so it sorts first.
        let g = grid_tiles(&m, None, false, 10_000.0);
        assert_eq!(
            g.tiles,
            [
                Tile::File {
                    run: 0,
                    tests: vec![0, 2]
                },
                Tile::File { run: 0, tests: vec![1] },
            ]
        );
        let r = &m.runs[0];
        let a: Vec<&Test> = [0, 2].iter().map(|&i| &r.tests[i]).collect();
        assert_eq!(file_status(&a), Some(TestStatus::Failed));
        assert_eq!(
            file_lines(&a, 10),
            [
                (TermKind::Passed, "✔ a1".to_owned()),
                (TermKind::Error, "✘ a2".to_owned()),
                (TermKind::Error, "  expected 1".to_owned()),
            ]
        );
        // Short on room: start at the first failure.
        assert_eq!(file_lines(&a, 2)[0].1, "✘ a2");

        // c starts: its file is running, its queued test listed as pending.
        m.apply("r1", 3.0, begin("c1"));
        let g = grid_tiles(&m, None, true, 3.0);
        assert_eq!(g.tiles[0], Tile::File { run: 0, tests: vec![3] });
        assert_eq!(g.hidden, 1, "the passed file is hidden");
    }

    #[test]
    fn log_view_shows_each_test_in_full() {
        use crate::state::LogFocus;
        let mut m = Model::new();
        let test = |id: &str, file: &str| Event::TestBegin {
            test_id: id.into(),
            title: id.into(),
            title_path: vec!["Group".into(), id.into()],
            file: Some(file.into()),
            line: None,
            project: None,
            worker_index: None,
        };
        m.apply("r1", 0.0, test("a", "/p/x.test.ts"));
        m.apply("r1", 0.0, test("b", "/p/y.test.ts"));
        m.apply("r1", 0.0, test("c", "/p/x.test.ts"));
        m.apply(
            "r1",
            1.0,
            Event::TestOutput {
                test_id: "a".into(),
                stream: testudo_protocol::StdStream::Stdout,
                text: "hi\n".into(),
            },
        );
        m.apply("r1", 2.0, end("a", TestStatus::Passed, None));
        m.apply("r1", 2.0, end("c", TestStatus::Failed, Some("one\ntwo\nthree")));
        let focus = LogFocus::File {
            run_id: "r1".into(),
            file: "/p/x.test.ts".into(),
        };
        let tests = log_tests(&m, &focus);
        assert_eq!(tests.len(), 2);
        let text: Vec<String> = log_lines(&tests).into_iter().map(|(_, l)| l).collect();
        assert_eq!(
            text,
            [
                "✔ a (passed)",
                "  Group",
                "  hi",
                "",
                "✘ c (failed)",
                "  Group",
                "  one",
                "  two",
                "  three"
            ]
        );
        let one = LogFocus::Test {
            run_id: "r1".into(),
            test_id: "b".into(),
        };
        assert_eq!(log_tests(&m, &one).len(), 1);
    }

    #[test]
    fn quiet_test_tile_shows_group_and_result() {
        let mut m = Model::new();
        m.apply(
            "r1",
            0.0,
            Event::TestBegin {
                test_id: "t".into(),
                title: "closes".into(),
                title_path: vec!["Sheet".into(), "dismissal".into(), "closes".into()],
                file: None,
                line: None,
                project: None,
                worker_index: None,
            },
        );
        let t = m.runs[0].test("t").unwrap();
        assert_eq!(
            terminal_lines(t, 10),
            [
                (TermKind::Note, "Sheet › dismissal".to_owned()),
                (TermKind::Note, "running…".to_owned())
            ]
        );
        m.apply(
            "r1",
            1.0,
            Event::TestEnd {
                test_id: "t".into(),
                status: TestStatus::Passed,
                duration_ms: Some(12.0),
                error: None,
                note: None,
            },
        );
        let t = m.runs[0].test("t").unwrap();
        assert_eq!(terminal_lines(t, 10)[1].1, "passed in 12ms");
        assert_eq!(terminal_lines(t, 10)[2].1, "no console output");
        assert_eq!(with_line("a.ts", None), "a.ts");
        assert_eq!(with_line("a.ts", Some(7)), "a.ts:7");
    }

    #[test]
    fn terminal_lines_show_output_then_error() {
        let mut m = Model::new();
        m.apply("r1", 0.0, begin("t"));
        let out = |stream, text: &str| Event::TestOutput {
            test_id: "t".into(),
            stream,
            text: text.into(),
        };
        m.apply("r1", 1.0, out(testudo_protocol::StdStream::Stdout, "one\ntwo\n"));
        m.apply("r1", 2.0, out(testudo_protocol::StdStream::Stderr, "warn"));
        m.apply("r1", 3.0, end("t", TestStatus::Failed, Some("expected 1\nreceived 2")));
        let t = m.runs[0].test("t").unwrap();
        assert_eq!(
            terminal_lines(t, 10),
            [
                (TermKind::Stdout, "one".to_owned()),
                (TermKind::Stdout, "two".to_owned()),
                (TermKind::Stderr, "warn".to_owned()),
                (TermKind::Error, "expected 1".to_owned()),
                (TermKind::Error, "received 2".to_owned()),
            ]
        );
        assert_eq!(terminal_lines(t, 2).len(), 2, "keeps the tail");
        assert_eq!(terminal_lines(t, 2)[1].1, "received 2");

        // No output: its steps stand in.
        let mut m = Model::new();
        m.apply("r1", 0.0, begin("t"));
        m.apply(
            "r1",
            1.0,
            Event::StepBegin {
                test_id: "t".into(),
                step_id: "s".into(),
                title: "GET /health".into(),
                category: Some("pw:api".into()),
            },
        );
        let t = m.runs[0].test("t").unwrap();
        assert_eq!(terminal_lines(t, 10), [(TermKind::Step, "› GET /health".to_owned())]);
    }
}
