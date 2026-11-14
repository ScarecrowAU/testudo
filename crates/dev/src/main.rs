//! `make dev`: rebuild Testudo when sources change and restart the GUI, but only
//! when the build succeeds, so a compile error never takes the window away.
//!
//! - Watches `crates/**/{*.rs,Cargo.toml}`, the workspace `Cargo.toml` and
//!   `Cargo.lock` by polling mtimes (no watcher dependency to install).
//! - Latest change wins: if sources change while cargo is building, the build is
//!   cancelled and restarted from the newest state, so the app only ever
//!   restarts once, on a binary that matches what is on disk.
//! - Restarts with SIGTERM: the app closes like the close button, stopping any
//!   active test runs cleanly, then SIGKILL if it hangs.
//! - Adapter (`adapters/js`) edits need no restart: the runner loads them fresh
//!   for every run, so they are only reported.
//! - Closing the window ends the loop; a crash keeps watching for a fix.
//! - The app's log also goes to `target/dev/testudo.log` (fresh per start;
//!   the previous one is kept as `testudo.prev.log`).
//! - `TESTUDO_*` settings in `dev/.env` (gitignored, `KEY=value` lines) are
//!   passed to the app unless already set, e.g.
//!   `TESTUDO_OTLP_ENDPOINT=http://collector:4318` to export its spans. The
//!   endpoint is only used when it answers, so working offline stays quiet.
//!
//! Usage: testudo-dev [-- args for testudo]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::time::{Duration, Instant, SystemTime};

const POLL: Duration = Duration::from_millis(400);
/// Wait for edits to settle (editors write in bursts, `cargo fmt` touches many files).
const SETTLE: Duration = Duration::from_millis(300);
const STOP_TIMEOUT: Duration = Duration::from_secs(15);

type Snapshot = BTreeMap<PathBuf, SystemTime>;

fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let root = root.canonicalize().unwrap_or(root);
    let app_args: Vec<String> = std::env::args().skip(1).skip_while(|a| a == "--").collect();
    let bin = root.join("target/debug/testudo");
    let log_file = root.join("target/dev/testudo.log");

    // A terminal Ctrl+C reaches the whole foreground group: let the app handle
    // it (graceful close) and exit when the app does.
    ignore_sigint();

    log("watching crates/ (Rust) and adapters/js/ (no restart needed); close the window or Ctrl+C to quit");
    let mut sources = scan(&root, is_source);
    let mut adapters = scan(&root.join("adapters/js"), |p| ext(p, "mjs"));
    let mut app = if build_latest(&root, &mut sources) {
        start(&bin, &app_args, &log_file)
    } else {
        None
    };

    loop {
        std::thread::sleep(POLL);

        if let Some(child) = app.as_mut()
            && let Ok(Some(status)) = child.try_wait()
        {
            app = None;
            if exited_cleanly(status) {
                log("window closed; stopping dev loop");
                return;
            }
            log(&format!("testudo exited ({status}); waiting for changes"));
        }

        let now = scan(&root.join("adapters/js"), |p| ext(p, "mjs"));
        if now != adapters {
            for p in changed(&adapters, &now) {
                log(&format!(
                    "adapter changed: {} (used from the next run, no restart)",
                    rel(&root, &p)
                ));
            }
            adapters = now;
        }

        let now = scan(&root, is_source);
        if now == sources {
            continue;
        }
        let settled = settle(&root, now);
        let files = changed(&sources, &settled);
        sources = settled;
        log(&format!(
            "changed: {}",
            files
                .iter()
                .take(5)
                .map(|p| rel(&root, p))
                .collect::<Vec<_>>()
                .join(", ")
                + if files.len() > 5 { " …" } else { "" }
        ));

        if !build_latest(&root, &mut sources) {
            log(if app.is_some() {
                "build failed; keeping the running app"
            } else {
                "build failed; waiting for a fix"
            });
            continue;
        }
        if let Some(child) = app.take() {
            log("restarting testudo");
            stop(child);
        }
        app = start(&bin, &app_args, &log_file);
    }
}

/// Build until a build completes against the current sources. Edits that land
/// mid-build cancel it and start over (after they settle); `sources` is updated
/// to the snapshot the final build saw.
fn build_latest(root: &Path, sources: &mut Snapshot) -> bool {
    'attempt: loop {
        let started = Instant::now();
        let mut cargo = match Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
            .args(["build", "-p", "testudo-ui", "-p", "testudo-cli"])
            .current_dir(root)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                log(&format!("failed to run cargo: {e}"));
                return false;
            }
        };
        loop {
            if let Ok(Some(status)) = cargo.try_wait() {
                if status.success() {
                    log(&format!("built in {:.1}s", started.elapsed().as_secs_f32()));
                }
                return status.success();
            }
            std::thread::sleep(Duration::from_millis(150));
            let now = scan(root, is_source);
            if now != *sources {
                let _ = cargo.kill();
                let _ = cargo.wait();
                let settled = settle(root, now);
                log(&format!(
                    "more changes during build ({}); restarting build",
                    changed(sources, &settled)
                        .iter()
                        .map(|p| rel(root, p))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
                *sources = settled;
                continue 'attempt;
            }
        }
    }
}

fn start(bin: &Path, args: &[String], log_file: &Path) -> Option<Child> {
    if let Some(dir) = log_file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::rename(log_file, log_file.with_file_name("testudo.prev.log"));
    let mut cmd = Command::new(bin);
    cmd.args(args).env("TESTUDO_LOG_FILE", log_file);
    let root = bin.ancestors().nth(3).unwrap_or(Path::new("."));
    for (k, v) in dev_env(&root.join("dev/.env")) {
        if std::env::var_os(&k).is_none() {
            if k == "TESTUDO_OTLP_ENDPOINT" {
                log(&format!("exporting traces to {v}"));
            }
            cmd.env(k, v);
        }
    }
    match cmd.spawn() {
        Ok(child) => Some(child),
        Err(e) => {
            log(&format!("failed to start {}: {e}", bin.display()));
            None
        }
    }
}

/// `TESTUDO_*` lines from `dev/.env`. The OTLP settings are dropped when the
/// endpoint doesn't answer.
fn dev_env(path: &Path) -> Vec<(String, String)> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut vars: Vec<(String, String)> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().trim_matches('"').to_owned()))
        .filter(|(k, _)| k.starts_with("TESTUDO_"))
        .collect();
    let endpoint = vars
        .iter()
        .find(|(k, _)| k == "TESTUDO_OTLP_ENDPOINT")
        .map(|(_, v)| v.clone());
    if let Some(url) = endpoint
        && !reachable(&url)
    {
        log(&format!("{url} not reachable; not exporting traces"));
        vars.retain(|(k, _)| !k.starts_with("TESTUDO_OTLP_"));
    }
    vars
}

/// Can we open a TCP connection to the URL's host and port (300ms)?
fn reachable(url: &str) -> bool {
    use std::net::ToSocketAddrs;
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let host = rest.split('/').next().unwrap_or(rest);
    let host = if host.contains(':') {
        host.to_owned()
    } else {
        format!("{host}:{}", if url.starts_with("https") { 443 } else { 80 })
    };
    host.to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
        .is_some_and(|addr| std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok())
}

fn stop(mut child: Child) {
    #[cfg(unix)]
    // SAFETY: plain kill(2) on our own child.
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    #[cfg(not(unix))]
    let _ = child.kill();

    let deadline = Instant::now() + STOP_TIMEOUT;
    while Instant::now() < deadline {
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    log("testudo did not exit in time; killing it");
    let _ = child.kill();
    let _ = child.wait();
}

/// Exit 0, or killed by the SIGINT the terminal sent us both.
fn exited_cleanly(status: ExitStatus) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status.success() || status.signal() == Some(libc::SIGINT)
    }
    #[cfg(not(unix))]
    status.success()
}

fn settle(root: &Path, mut snap: Snapshot) -> Snapshot {
    loop {
        std::thread::sleep(SETTLE);
        let next = scan(root, is_source);
        if next == snap {
            return snap;
        }
        snap = next;
    }
}

fn is_source(p: &Path) -> bool {
    let s = p.to_string_lossy();
    let in_crates = s.contains("/crates/") && !s.contains("/crates/dev/");
    (in_crates && (ext(p, "rs") || p.file_name().is_some_and(|n| n == "Cargo.toml")))
        || (!s.contains("/crates/") && p.file_name().is_some_and(|n| n == "Cargo.toml" || n == "Cargo.lock"))
}

fn ext(p: &Path, e: &str) -> bool {
    p.extension().is_some_and(|x| x == e)
}

fn scan(dir: &Path, keep: fn(&Path) -> bool) -> Snapshot {
    let mut out = Snapshot::new();
    walk(dir, keep, &mut out);
    out
}

fn walk(dir: &Path, keep: fn(&Path) -> bool, out: &mut Snapshot) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let path = e.path();
        let name = e.file_name();
        if name == "target" || name == "node_modules" || name.to_string_lossy().starts_with('.') {
            continue;
        }
        match e.file_type() {
            Ok(t) if t.is_dir() => walk(&path, keep, out),
            Ok(_) if keep(&path) => {
                if let Ok(m) = e.metadata().and_then(|m| m.modified()) {
                    out.insert(path, m);
                }
            }
            _ => {}
        }
    }
}

fn changed(old: &Snapshot, new: &Snapshot) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = new
        .iter()
        .filter(|(p, m)| old.get(*p) != Some(m))
        .map(|(p, _)| p.clone())
        .collect();
    out.extend(old.keys().filter(|p| !new.contains_key(*p)).cloned());
    out
}

fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root).unwrap_or(p).display().to_string()
}

fn log(msg: &str) {
    eprintln!("\x1b[36m[dev]\x1b[0m {msg}");
}

#[cfg(unix)]
fn ignore_sigint() {
    // SAFETY: setting a signal disposition before any threads exist.
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
    }
}

#[cfg(not(unix))]
fn ignore_sigint() {}
