//! Process-tree helpers for stopping runs without leaving orphans.
//!
//! Killing the runner's process group is not enough: runners start helpers in
//! their own session/group (Playwright's `webServer` is spawned detached), and
//! those survive a group kill and get reparented. So when a stop is requested
//! we snapshot every descendant (`/proc` on Linux, libproc on macOS), and once the runner has
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
pub fn descendants(root: i32) -> Vec<Proc> {
    let all = all_procs();
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

/// Is this exact process (pid + start time) still alive?
pub fn alive(p: &Proc) -> bool {
    stat(p.pid).is_some_and(|(now, _)| now.start_time == p.start_time)
}

/// Every process on the system with its parent pid.
#[cfg(target_os = "linux")]
fn all_procs() -> Vec<(Proc, i32)> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return vec![];
    };
    entries
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<i32>().ok())
        .filter_map(stat)
        .collect()
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

/// Every process on the system with its parent pid (libproc).
#[cfg(target_os = "macos")]
fn all_procs() -> Vec<(Proc, i32)> {
    // SAFETY: a null buffer asks libproc for the current pid count.
    let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if n <= 0 {
        return vec![];
    }
    // Headroom for processes started between the two calls.
    let mut pids = vec![0i32; n as usize + 64];
    let bytes = (pids.len() * std::mem::size_of::<i32>()) as i32;
    // SAFETY: the buffer is `bytes` long; libproc returns how many pids it wrote.
    let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    pids.truncate(n.max(0) as usize);
    pids.into_iter().filter(|&pid| pid > 0).filter_map(stat).collect()
}

/// One process's group, parent and start time (libproc `PROC_PIDTBSDINFO`).
#[cfg(target_os = "macos")]
fn stat(pid: i32) -> Option<(Proc, i32)> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    // SAFETY: `info` is a properly sized, writable proc_bsdinfo.
    let got = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
    if got != size {
        return None;
    }
    let start_time = info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec;
    let proc = Proc {
        pid,
        pgid: info.pbi_pgid as i32,
        start_time,
    };
    Some((proc, info.pbi_ppid as i32))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn all_procs() -> Vec<(Proc, i32)> {
    // TODO: Toolhelp/Job objects on Windows.
    vec![]
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn stat(_pid: i32) -> Option<(Proc, i32)> {
    None
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

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    #[test]
    fn finds_and_kills_detached_grandchild() {
        // bash (own group) -> setsid sleep (own session/group), like a detached webServer.
        let mut child = Command::new("bash")
            // perl rather than setsid(1), which macOS lacks.
            .args(["-c", "perl -MPOSIX -e 'POSIX::setsid(); sleep 30' & echo $!; wait"])
            .stdout(std::process::Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        let mut line = String::new();
        std::io::BufRead::read_line(&mut std::io::BufReader::new(child.stdout.as_mut().unwrap()), &mut line).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));

        let root = child.id() as i32;
        let snap = descendants(root);
        let detached = snap
            .iter()
            .find(|p| p.pgid != root)
            .expect("detached grandchild in snapshot");
        assert!(alive(detached));

        signal_group(root, Sig::Kill);
        let _ = child.wait();
        assert!(alive(detached), "group kill must not reach the detached process");

        assert_eq!(signal_survivors(&snap, root, Sig::Kill), 1);
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(!alive(detached));
    }
}
