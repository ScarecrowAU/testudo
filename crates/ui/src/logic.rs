//! UI-agnostic rules: pane ordering, status classification, output heuristics.
//! Kept free of gpui so they are cheap to unit test.

use testudo_protocol::TestStatus;

use crate::model::{Model, Page, Test};

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

/// Pages in display order: running, failed, complete; by name (case-insensitive)
/// within a group, page id as a stable tiebreak.
pub fn ordered_pages(model: &Model) -> Vec<&Page> {
    let mut pages: Vec<&Page> = model.pages.values().collect();
    pages.sort_by_cached_key(|p| {
        let state = pane_state(p, model.test_for_page(p));
        (state, pane_name(model, p).to_lowercase(), p.key.page_id.clone())
    });
    pages
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
    out.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default().to_owned()
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

/// Should two recordings of shape `frame` (w, h) be stacked one above the
/// other (else side by side) in an `area` (w, h)? Picks the arrangement that
/// shows them larger; each cell loses `header` px to its label row, cells are
/// `gap` px apart. Ties go side by side.
pub fn stack_vertically(area: (f32, f32), frame: (f32, f32), header: f32, gap: f32) -> bool {
    let (w, h) = area;
    let (fw, fh) = (frame.0.max(1.0), frame.1.max(1.0));
    let fit = |cw: f32, ch: f32| (cw / fw).min(ch / fh).max(0.0);
    let side_by_side = fit((w - gap) / 2.0, h - header);
    let stacked = fit(w, (h - gap) / 2.0 - header);
    stacked > side_by_side
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

/// A path for display, with the home directory shown as `~`.
pub fn tilde(p: &std::path::Path) -> String {
    match std::env::var_os("HOME").map(std::path::PathBuf::from) {
        Some(home) if !home.as_os_str().is_empty() => match p.strip_prefix(&home) {
            Ok(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => p.display().to_string(),
        },
        _ => p.display().to_string(),
    }
}

pub fn expand_tilde(p: &str) -> String {
    match (p.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => p.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use testudo_protocol::Event;

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
        assert_eq!(align_by_step(&cur, &steps(&[(1020.0, "click")]), 50.0, 0.0, 1000.0), Some(1020.0));
        // Other run is faster: held just before its next step.
        let fast = steps(&[(1100.0, "click"), (1150.0, "fill")]);
        assert_eq!(align_by_step(&cur, &fast, 190.0, 0.0, 1000.0), Some(1149.999));
        // No matching step.
        assert_eq!(align_by_step(&cur, &steps(&[(1100.0, "goto")]), 150.0, 0.0, 1000.0), None);
    }

    #[test]
    fn rerun_args_select_one_test() {
        assert_eq!(
            rerun_args("playwright", "/p/web", "/p/web/tests/a.spec.ts", 12, Some("chromium")),
            Some(vec!["tests/a.spec.ts:12".to_owned(), "--project=chromium".to_owned()])
        );
        assert_eq!(rerun_args("vitest", "/p", "/p/src/x.test.ts", 3, Some("ignored")), Some(vec!["src/x.test.ts:3".to_owned()]));
        assert_eq!(rerun_args("playwright", "/p", "/elsewhere/a.spec.ts", 1, None), Some(vec!["/elsewhere/a.spec.ts:1".to_owned()]));
        assert_eq!(rerun_args("command", "/p", "/p/a", 1, None), None);
    }

    #[test]
    fn comparison_layout_follows_the_space_and_page_shape() {
        // Wide pane, phone page: side by side.
        assert!(!stack_vertically((1000.0, 600.0), (390.0, 844.0), 30.0, 8.0));
        // Tall pane, desktop page: stacked.
        assert!(stack_vertically((600.0, 1000.0), (1280.0, 720.0), 30.0, 8.0));
        // Desktop page in a roughly square pane: stacking shows it larger.
        assert!(stack_vertically((900.0, 800.0), (1280.0, 720.0), 30.0, 8.0));
        // Very wide pane, desktop page: side by side.
        assert!(!stack_vertically((2000.0, 600.0), (1280.0, 720.0), 30.0, 8.0));
    }

    #[test]
    fn tests_order_by_state_then_name() {
        let mut m = Model::new();
        let queue = |id: &str| Event::TestQueued {
            test_id: id.into(),
            title: id.into(),
            title_path: vec![],
            file: None,
            line: None,
            project: None,
        };
        let begin = |id: &str| Event::TestBegin {
            test_id: id.into(),
            title: id.into(),
            title_path: vec![],
            file: None,
            line: None,
            project: None,
            worker_index: None,
        };
        let end = |id: &str, status| Event::TestEnd { test_id: id.into(), status, duration_ms: None, error: None, note: None };
        let events = [
            queue("zeta passes"),
            queue("Alpha pending"),
            queue("beta fails"),
            queue("alpha passes"),
            queue("gamma running"),
            queue("delta pending"),
            queue("Beta skipped"),
            begin("zeta passes"),
            end("zeta passes", TestStatus::Passed),
            begin("beta fails"),
            end("beta fails", TestStatus::Failed),
            begin("alpha passes"),
            end("alpha passes", TestStatus::Passed),
            begin("Beta skipped"),
            end("Beta skipped", TestStatus::Skipped),
            begin("gamma running"),
        ];
        for (i, e) in events.into_iter().enumerate() {
            m.apply("r1", i as f64, e);
        }
        let run = &m.runs[0];
        let mut tests: Vec<&Test> = run.tests.iter().collect();
        tests.sort_by_cached_key(|t| test_order(t));
        let order: Vec<&str> = tests.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(
            order,
            ["gamma running", "Alpha pending", "delta pending", "beta fails", "alpha passes", "Beta skipped", "zeta passes"]
        );
        assert_eq!(run.counts(), (2, 1, 1, 2), "passed, failed, running, pending");
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
        let order: Vec<&str> = ordered_pages(&m).iter().map(|p| p.key.page_id.as_str()).collect();
        assert_eq!(order, ["p3", "p5", "p2", "p6", "p4", "p1"]);
    }
}
