//! Git worktrees of the repository a project directory belongs to.
//!
//! `git worktree list --porcelain` lists the main worktree first, then every
//! linked one. Testudo shows the main worktree's path as the project directory
//! and lets the user pick which worktree to scan and run (the GUI's worktree
//! selector); opening Testudo inside a linked worktree preselects it.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    /// Short branch name (`feature/x`), `None` when HEAD is detached.
    pub branch: Option<String>,
    /// Commit HEAD points at.
    pub head: Option<String>,
}

impl Worktree {
    /// `feature/x`, or `detached 1a2b3c4` (or `detached`).
    pub fn label(&self) -> String {
        match (&self.branch, &self.head) {
            (Some(b), _) => b.clone(),
            (None, Some(h)) => format!("detached {}", &h[..h.len().min(7)]),
            (None, None) => "detached".to_owned(),
        }
    }
}

/// The worktrees of one repository and where a directory sits among them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktrees {
    /// Main worktree first.
    pub list: Vec<Worktree>,
    /// Index of the worktree the detected directory is in.
    pub current: usize,
    /// The detected directory relative to that worktree's root (empty at the root).
    pub sub: PathBuf,
}

impl Worktrees {
    pub fn main(&self) -> &Worktree {
        &self.list[0]
    }

    /// The directory to scan in worktree `i`: its root joined with `sub`.
    pub fn dir(&self, i: usize) -> PathBuf {
        let root = &self.list[i.min(self.list.len() - 1)].path;
        if self.sub.as_os_str().is_empty() {
            root.clone()
        } else {
            root.join(&self.sub)
        }
    }

    /// Index of the worktree at `path`, if listed.
    pub fn position(&self, path: &Path) -> Option<usize> {
        self.list.iter().position(|w| w.path == path)
    }
}

/// Parse `git worktree list --porcelain`. Bare and prunable (missing) entries
/// are dropped: neither has files to scan.
pub fn parse_porcelain(out: &str) -> Vec<Worktree> {
    let mut list = Vec::new();
    for block in out.split("\n\n") {
        let mut wt: Option<Worktree> = None;
        let mut skip = false;
        for line in block.lines() {
            let (key, value) = line.split_once(' ').unwrap_or((line, ""));
            match key {
                "worktree" => {
                    wt = Some(Worktree {
                        path: PathBuf::from(value),
                        branch: None,
                        head: None,
                    });
                }
                "HEAD" => {
                    if let Some(w) = wt.as_mut() {
                        w.head = Some(value.to_owned());
                    }
                }
                "branch" => {
                    if let Some(w) = wt.as_mut() {
                        w.branch = Some(value.strip_prefix("refs/heads/").unwrap_or(value).to_owned());
                    }
                }
                "bare" | "prunable" => skip = true,
                _ => {}
            }
        }
        if let Some(w) = wt
            && !skip
        {
            list.push(w);
        }
    }
    list
}

/// Place `dir` among `list`: the deepest worktree containing it (a linked
/// worktree nested inside the main one wins over the main one).
pub fn locate(list: Vec<Worktree>, dir: &Path) -> Option<Worktrees> {
    let (current, w) = list
        .iter()
        .enumerate()
        .filter(|(_, w)| dir.starts_with(&w.path))
        .max_by_key(|(_, w)| w.path.components().count())?;
    let sub = dir.strip_prefix(&w.path).ok()?.to_owned();
    Some(Worktrees { list, current, sub })
}

/// The worktrees of the repository containing `dir`, when it has linked
/// worktrees. `None` outside a git repository, with only the main worktree,
/// or when git is unavailable.
#[tracing::instrument(level = "info", skip_all, fields(dir = %dir.display()))]
pub fn detect(dir: &Path) -> Option<Worktrees> {
    let dir = dir.canonicalize().ok()?;
    let out = Command::new("git")
        .arg("-C")
        .arg(&dir)
        .args(["worktree", "list", "--porcelain"])
        // Never let an inherited repository override the one at `dir`.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let mut list = parse_porcelain(&String::from_utf8_lossy(&out.stdout));
    for w in &mut list {
        if let Ok(p) = w.path.canonicalize() {
            w.path = p;
        }
    }
    if list.len() < 2 {
        return None;
    }
    locate(list, &dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PORCELAIN: &str = "\
worktree /src/shop
HEAD 1111111111111111111111111111111111111111
branch refs/heads/main

worktree /src/shop/.claude/worktrees/fix-cart
HEAD 2222222222222222222222222222222222222222
branch refs/heads/fix/cart

worktree /src/shop-review
HEAD 3333333333333333333333333333333333333333
detached

worktree /src/gone
HEAD 4444444444444444444444444444444444444444
branch refs/heads/old
prunable gitdir file points to non-existent location

";

    #[test]
    fn parses_porcelain_output() {
        let list = parse_porcelain(PORCELAIN);
        assert_eq!(list.len(), 3, "prunable entry dropped: {list:?}");
        assert_eq!(list[0].path, PathBuf::from("/src/shop"));
        assert_eq!(list[0].label(), "main");
        assert_eq!(list[1].label(), "fix/cart");
        assert_eq!(list[2].branch, None);
        assert_eq!(list[2].label(), "detached 3333333");
    }

    #[test]
    fn bare_main_is_dropped() {
        let out = "worktree /src/repo.git\nbare\n\nworktree /src/repo-main\nHEAD abc\nbranch refs/heads/main\n";
        let list = parse_porcelain(out);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].path, PathBuf::from("/src/repo-main"));
    }

    #[test]
    fn locates_the_deepest_worktree_and_keeps_the_subdirectory() {
        let list = parse_porcelain(PORCELAIN);
        let at = |d: &str| locate(list.clone(), Path::new(d)).map(|w| (w.current, w.sub));
        assert_eq!(at("/src/shop"), Some((0, PathBuf::new())));
        assert_eq!(at("/src/shop/apps/web"), Some((0, PathBuf::from("apps/web"))));
        assert_eq!(
            at("/src/shop/.claude/worktrees/fix-cart/apps/web"),
            Some((1, PathBuf::from("apps/web")))
        );
        assert_eq!(at("/src/shop-review"), Some((2, PathBuf::new())));
        assert_eq!(at("/elsewhere"), None);

        let wts = locate(list, Path::new("/src/shop/.claude/worktrees/fix-cart/apps/web")).unwrap();
        assert_eq!(wts.dir(0), PathBuf::from("/src/shop/apps/web"));
        assert_eq!(wts.dir(2), PathBuf::from("/src/shop-review/apps/web"));
    }

    #[test]
    fn detects_a_real_repository() {
        let git = |dir: &Path, args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .output()
                .is_ok_and(|o| o.status.success())
        };
        let root = std::env::temp_dir().join(format!("testudo-worktree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let main = root.join("repo");
        std::fs::create_dir_all(main.join("apps/web")).unwrap();
        std::fs::write(main.join("apps/web/a.txt"), "a").unwrap();
        let setup = git(&main, &["init", "-q", "-b", "main"])
            && git(&main, &["add", "."])
            && git(
                &main,
                &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "init"],
            );
        if !setup {
            eprintln!("git unavailable; skipping");
            return;
        }
        assert_eq!(detect(&main), None, "no linked worktrees: no selector");
        assert!(git(&main, &["worktree", "add", "-q", "-b", "feat", "../feat"]));

        let wts = detect(&root.join("feat/apps/web")).unwrap();
        assert_eq!(wts.list.len(), 2);
        assert_eq!(wts.list[wts.current].label(), "feat");
        assert_eq!(wts.sub, PathBuf::from("apps/web"));
        assert_eq!(wts.dir(0), main.canonicalize().unwrap().join("apps/web"));
        assert_eq!(detect(&root), None, "not a repository");
        std::fs::remove_dir_all(root).unwrap();
    }
}
