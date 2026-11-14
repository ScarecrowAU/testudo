//! Spawning test runners with Testudo instrumentation injected from outside, so the
//! user's tests and fixtures stay untouched:
//!
//! - `NODE_OPTIONS=--import <adapters>/preload.mjs` instruments playwright-core in
//!   every Node process the runner starts (Playwright workers, Vitest's browser
//!   provider) and streams frames/network/console.
//! - Playwright: `PW_TEST_REPORTER=<adapters>/playwright-reporter.mjs`, which
//!   Playwright appends after the configured reporters. Nothing is written into the
//!   user's project. (Undocumented but long-standing; the VS Code extension relies
//!   on it.)
//! - Vitest: `--reporter` flags on the CLI.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use testudo_protocol::{Event, Message, StdStream, env};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;
use tokio::sync::oneshot;

use crate::hub::{Hub, RunId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunnerKind {
    Playwright,
    Vitest,
    /// Arbitrary command; still gets the preload and socket env, so any Node process
    /// using playwright-core is instrumented.
    Command { program: String },
}

impl RunnerKind {
    pub fn name(&self) -> &str {
        match self {
            Self::Playwright => "playwright",
            Self::Vitest => "vitest",
            Self::Command { .. } => "command",
        }
    }
}

#[derive(Debug, Clone)]
pub struct RunSpec {
    pub label: String,
    pub kind: RunnerKind,
    pub cwd: PathBuf,
    pub args: Vec<String>,
}

pub struct RunHandle {
    pub id: RunId,
    cancel: Option<oneshot::Sender<()>>,
}

impl RunHandle {
    /// Ask the run to stop: SIGTERM to the process group, SIGKILL after a grace period.
    pub fn cancel(&mut self) {
        if let Some(tx) = self.cancel.take() {
            let _ = tx.send(());
        }
    }
}

#[derive(Clone)]
pub struct Orchestrator {
    inner: Arc<Inner>,
}

struct Inner {
    hub: Hub,
    socket: PathBuf,
    adapters: PathBuf,
    rt: tokio::runtime::Handle,
    next_id: AtomicU64,
}

impl Orchestrator {
    pub fn new(hub: Hub, socket: PathBuf, adapters: PathBuf, rt: tokio::runtime::Handle) -> Self {
        Self {
            inner: Arc::new(Inner {
                hub,
                socket,
                adapters,
                rt,
                next_id: AtomicU64::new(1),
            }),
        }
    }

    /// Spawn a run. Safe to call from any thread.
    pub fn start(&self, spec: RunSpec) -> Result<RunHandle> {
        let inner = &self.inner;
        let id = format!("run-{}", inner.next_id.fetch_add(1, Ordering::Relaxed));
        let cwd = spec
            .cwd
            .canonicalize()
            .with_context(|| format!("project dir {}", spec.cwd.display()))?;

        let Launch { program, args, envs } = build_command(&spec.kind, &cwd, &spec.args, &inner.adapters);

        let preload = inner.adapters.join("preload.mjs");
        let mut node_options = format!("--import {}", file_url(&preload));
        if let Ok(existing) = std::env::var("NODE_OPTIONS") {
            node_options = format!("{node_options} {existing}");
        }

        let _guard = inner.rt.enter();
        let mut cmd = Command::new(&program);
        cmd.args(&args)
            .current_dir(&cwd)
            .env("NODE_OPTIONS", node_options)
            .env(env::SOCKET, &inner.socket)
            .env(env::RUN_ID, &id)
            .env(env::PROJECT_ROOT, &cwd)
            .env("FORCE_COLOR", "0")
            .envs(envs)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);

        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning {program} {}", args.join(" ")))?;

        let hub = inner.hub.clone();
        pipe_lines(child.stdout.take().unwrap(), StdStream::Stdout, hub.clone(), id.clone());
        pipe_lines(child.stderr.take().unwrap(), StdStream::Stderr, hub.clone(), id.clone());

        let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
        let run_id = id.clone();
        tokio::spawn(async move {
            let pid = child.id();
            let status = tokio::select! {
                s = child.wait() => s,
                _ = cancel_rx => {
                    signal_group(pid, Signal::Term);
                    match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
                        Ok(s) => s,
                        Err(_) => {
                            signal_group(pid, Signal::Kill);
                            child.wait().await
                        }
                    }
                }
            };
            let code = status.ok().and_then(|s| s.code());
            hub.publish(&run_id, Message::Event(Event::ProcessExited { code }));
        });

        Ok(RunHandle {
            id,
            cancel: Some(cancel_tx),
        })
    }
}

struct Launch {
    program: String,
    args: Vec<String>,
    envs: Vec<(String, String)>,
}

fn build_command(kind: &RunnerKind, cwd: &Path, user_args: &[String], adapters: &Path) -> Launch {
    match kind {
        RunnerKind::Playwright => {
            let (program, mut args) = node_bin(cwd, "playwright");
            args.push("test".into());
            args.extend(user_args.iter().cloned());
            let reporter = adapters.join("playwright-reporter.mjs").display().to_string();
            Launch {
                program,
                args,
                envs: vec![("PW_TEST_REPORTER".into(), reporter)],
            }
        }
        RunnerKind::Vitest => {
            let (program, mut args) = node_bin(cwd, "vitest");
            let subcommands = ["run", "watch", "dev", "related", "bench"];
            if !user_args.first().is_some_and(|a| subcommands.contains(&a.as_str())) {
                args.push("run".into());
            }
            args.push("--reporter=default".into());
            args.push(format!("--reporter={}", adapters.join("vitest-reporter.mjs").display()));
            args.extend(user_args.iter().cloned());
            Launch { program, args, envs: vec![] }
        }
        RunnerKind::Command { program } => Launch {
            program: program.clone(),
            args: user_args.to_vec(),
            envs: vec![],
        },
    }
}

/// Prefer the project's local binary; fall back to `npx`.
fn node_bin(cwd: &Path, name: &str) -> (String, Vec<String>) {
    let local = cwd.join("node_modules/.bin").join(name);
    if local.exists() {
        (local.display().to_string(), vec![])
    } else {
        ("npx".into(), vec![name.into()])
    }
}

fn file_url(path: &Path) -> String {
    let s = path.display().to_string();
    let mut out = String::from("file://");
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn pipe_lines<R: AsyncRead + Unpin + Send + 'static>(reader: R, stream: StdStream, hub: Hub, run_id: RunId) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let line = strip_ansi(&line);
            hub.publish(&run_id, Message::Event(Event::Stdio { stream, line }));
        }
    });
}

/// Drop ANSI escape sequences (CSI `ESC [ ... final` and two-byte `ESC x`). Some
/// runners colour output regardless of FORCE_COLOR.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        if chars.next() == Some('[') {
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        }
    }
    out
}

enum Signal {
    Term,
    Kill,
}

#[cfg(unix)]
fn signal_group(pid: Option<u32>, sig: Signal) {
    let Some(pid) = pid else { return };
    let sig = match sig {
        Signal::Term => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
    };
    // Negative pid targets the process group created by `process_group(0)`, which
    // includes runner workers and the browsers they launched.
    unsafe {
        libc::kill(-(pid as i32), sig);
    }
}

#[cfg(not(unix))]
fn signal_group(_pid: Option<u32>, _sig: Signal) {
    // TODO: Job objects on Windows. kill_on_drop covers the leader process.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_ansi() {
        assert_eq!(strip_ansi("\x1b[32m- 3\x1b[39m ok"), "- 3 ok");
        assert_eq!(strip_ansi("plain"), "plain");
    }

    #[test]
    fn file_url_escapes() {
        assert_eq!(file_url(Path::new("/a b/c.mjs")), "file:///a%20b/c.mjs");
    }
}
