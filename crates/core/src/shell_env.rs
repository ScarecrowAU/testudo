//! Import the user's login-shell environment.
//!
//! Apps started from a desktop launcher inherit the session environment, not
//! the one the user's shell builds (`~/.bashrc`, `~/.zshrc`: nvm, Homebrew,
//! pnpm, bun…). Runners then fail in confusing ways, e.g. Playwright's
//! `webServer` reporting `pnpm: command not found`. Like VS Code, ask the
//! user's `$SHELL` for its environment once at startup and merge it in.

use std::collections::HashMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const START: &str = "__TESTUDO_ENV_START__";
const END: &str = "__TESTUDO_ENV_END__";
const TIMEOUT: Duration = Duration::from_secs(5);
/// Shell bookkeeping that must not leak into our environment.
const SKIP: &[&str] = &["_", "PWD", "OLDPWD", "SHLVL", "PS1", "PS2", "PROMPT_COMMAND"];

/// Read the login shell's environment. `None` if disabled, unavailable,
/// or it took too long.
pub fn login_shell_env() -> Option<HashMap<String, String>> {
    if std::env::var_os("TESTUDO_NO_SHELL_ENV").is_some() || cfg!(windows) {
        return None;
    }
    let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty())?;
    let script = format!("printf '%s' {START}; env -0; printf '%s' {END}");
    let mut child = Command::new(&shell)
        .args(["-i", "-l", "-c", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() < TIMEOUT => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut out = Vec::new();
    child.stdout.take()?.read_to_end(&mut out).ok()?;
    parse(&String::from_utf8_lossy(&out))
}

/// Pull the NUL-separated `env -0` block out from between the markers (rc
/// files may print banners around it).
fn parse(output: &str) -> Option<HashMap<String, String>> {
    let body = output.split_once(START)?.1.rsplit_once(END)?.0;
    Some(
        body.split('\0')
            .filter_map(|kv| kv.split_once('='))
            .filter(|(k, _)| !k.is_empty() && !SKIP.contains(k))
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect(),
    )
}

/// PATH with the shell's entries first, then any of ours it lacks.
pub fn merge_path(shell: &str, ours: &str) -> String {
    let mut seen = std::collections::HashSet::new();
    shell
        .split(':')
        .chain(ours.split(':'))
        .filter(|p| !p.is_empty() && seen.insert(*p))
        .collect::<Vec<_>>()
        .join(":")
}

/// Merge the login shell's environment into this process: PATH is combined,
/// other variables are only added when not already set. Returns how many
/// variables changed.
///
/// # Safety
/// Mutates the process environment: call before any other threads start.
pub unsafe fn import_login_shell_env() -> usize {
    let Some(env) = login_shell_env() else { return 0 };
    let mut changed = 0;
    for (k, v) in env {
        if k == "PATH" {
            let merged = merge_path(&v, &std::env::var("PATH").unwrap_or_default());
            if std::env::var("PATH").ok().as_deref() != Some(merged.as_str()) {
                // SAFETY: caller guarantees no other threads yet.
                unsafe { std::env::set_var("PATH", merged) };
                changed += 1;
            }
        } else if std::env::var_os(&k).is_none() {
            // SAFETY: as above.
            unsafe { std::env::set_var(&k, v) };
            changed += 1;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_between_markers_ignoring_banners() {
        let out = format!("Welcome!\n{START}PATH=/a:/b\0HOME=/h\0_=/usr/bin/env\0EMPTY=\0{END}bye\n");
        let env = parse(&out).unwrap();
        assert_eq!(env.get("PATH").map(String::as_str), Some("/a:/b"));
        assert_eq!(env.get("EMPTY").map(String::as_str), Some(""));
        assert!(!env.contains_key("_"));
        assert!(parse("no markers").is_none());
    }

    #[test]
    fn path_merge_puts_shell_first_without_duplicates() {
        assert_eq!(
            merge_path("/brew/bin:/usr/bin", "/usr/bin:/bin:/brew/bin"),
            "/brew/bin:/usr/bin:/bin"
        );
        assert_eq!(merge_path("", "/usr/bin"), "/usr/bin");
    }
}
