//! Process-tree helpers for stopping runs without leaving orphans.
//!
//! Killing the runner's process group is not enough: runners start helpers in
//! their own session/group (Playwright's `webServer` is spawned detached), and
//! those survive a group kill and get reparented. So when a stop is requested
//! we snapshot every descendant (via `/proc` on Linux), and once the runner has
//! exited, clean up whatever in that snapshot is still alive. Each process is
//! identified by (pid, start time) so a recycled pid is never signalled.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Proc {
    pub pid: i32,
    pub pgid: i32,
    start_time: u64,
}

#[derive(Debug, Clone, Copy)]
pub enum Sig {
    Int,
    Term,
    Kill,
}

#[cfg(unix)]
fn raw(sig: Sig) -> i32 {
    match sig {
        Sig::Int => libc::SIGINT,
        Sig::Term => libc::SIGTERM,
        Sig::Kill => libc::SIGKILL,
    }
}

/// Signal a whole process group.
#[cfg(unix)]
pub fn signal_group(pgid: i32, sig: Sig) {
    // SAFETY: plain kill(2) on a negative pid = process group.
    unsafe {
        libc::kill(-pgid, raw(sig));
    }
}

#[cfg(not(unix))]
pub fn signal_group(_pgid: i32, _sig: Sig) {}

/// All current descendants of `root` (not including `root`).
#[cfg(target_os = "linux")]
pub fn descendants(root: i32) -> Vec<Proc> {
    let Ok(entries) = std::fs::read_dir("/proc") else { return vec![] };
    let mut all: Vec<(Proc, i32)> = Vec::new();
    for e in entries.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else { continue };
        if let Some((p, ppid)) = stat(pid) {
            all.push((p, ppid));
        }
    }
    let mut out = Vec::new();
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for (p, ppid) in &all {
            if *ppid == parent && !out.contains(p) {
                out.push(*p);
                frontier.push(p.pid);
            }
        }
    }
    out
}

#[cfg(not(target_os = "linux"))]
pub fn descendants(_root: i32) -> Vec<Proc> {
    // TODO: libproc on macOS, Toolhelp/Job objects on Windows.
    vec![]
}

/// Is this exact process (pid + start time) still alive?
pub fn alive(p: &Proc) -> bool {
    #[cfg(target_os = "linux")]
    {
        stat(p.pid).is_some_and(|(now, _)| now.start_time == p.start_time)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = p;
        false
    }
}

/// Signal the process groups of the snapshot's survivors (each group once).
pub fn signal_survivors(snapshot: &[Proc], skip_pgid: i32, sig: Sig) -> usize {
    let mut groups: Vec<i32> = Vec::new();
    for p in snapshot.iter().filter(|p| alive(p)) {
        if p.pgid != skip_pgid && !groups.contains(&p.pgid) {
            groups.push(p.pgid);
        }
    }
    for g in &groups {
        signal_group(*g, sig);
    }
    // Survivors inside the runner's own group are covered by signalling it again.
    if snapshot.iter().any(|p| p.pgid == skip_pgid && alive(p)) {
        signal_group(skip_pgid, sig);
    }
    groups.len()
}

/// Parse `/proc/<pid>/stat`: returns the process and its parent pid.
#[cfg(target_os = "linux")]
fn stat(pid: i32) -> Option<(Proc, i32)> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm (field 2) may contain spaces and parens; fields resume after the last ')'.
    let rest = &text[text.rfind(')')? + 2..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    // rest[0] = state (field 3) => field N is f[N - 3]
    let ppid = f.get(1)?.parse().ok()?;
    let pgid = f.get(2)?.parse().ok()?;
    let start_time = f.get(19)?.parse().ok()?;
    Some((Proc { pid, pgid, start_time }, ppid))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    #[test]
    fn finds_and_kills_detached_grandchild() {
        // bash (own group) -> setsid sleep (own session/group), like a detached webServer.
        let mut child = Command::new("bash")
            .args(["-c", "setsid sleep 30 & echo $!; wait"])
            .stdout(std::process::Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        let mut line = String::new();
        std::io::BufRead::read_line(&mut std::io::BufReader::new(child.stdout.as_mut().unwrap()), &mut line).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));

        let root = child.id() as i32;
        let snap = descendants(root);
        let detached = snap.iter().find(|p| p.pgid != root).expect("detached grandchild in snapshot");
        assert!(alive(detached));

        signal_group(root, Sig::Kill);
        let _ = child.wait();
        assert!(alive(detached), "group kill must not reach the detached process");

        assert_eq!(signal_survivors(&snap, root, Sig::Kill), 1);
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(!alive(detached));
    }
}
