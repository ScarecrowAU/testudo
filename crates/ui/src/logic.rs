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
