//! Find the test suites inside a project directory.
//!
//! A suite is one runnable unit: a runner, the directory it runs from, and
//! optionally an explicit config file. Discovery walks the tree (skipping
//! dependency/build/output dirs) and recognises:
//!
//! - Playwright: every `playwright*.config.{ts,mts,cts,js,mjs,cjs}`, so
//!   `playwright.storybook.config.ts` is its own suite next to the default one.
//! - Vitest: `vitest*.config.*` and `vitest.workspace.*`; a `vite.config.*` with
//!   a `test` block when no Vitest config sits beside it. Configs below a Vitest
//!   workspace (or a config declaring `projects`) are left to that workspace.
//! - Go modules: listed, not yet runnable.
//!
//! Directories below the root that are their own git checkout (a linked
//! worktree, whose `.git` is a file, or a nested clone) are skipped, so the
//! main worktree's scan never picks up a linked worktree's copy of a suite.
//!
//! Each suite resolves its runner from its own directory: the nearest
//! `node_modules/.bin/<runner>` walking upward, and that copy's version. In a
//! monorepo where apps pin different Playwright versions, each suite uses its
//! own. Nothing is ever fetched (no `npx` fallback): a missing install is
//! reported as a problem instead.

use std::path::{Path, PathBuf};

use crate::RunnerKind;
use crate::project_config::{ProjectConfig, RunConfig};

const MAX_DEPTH: usize = 8;
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "dist",
    "build",
    "out",
    "coverage",
    "vendor",
    "storybook-static",
    "test-results",
    "playwright-report",
    "blob-report",
];
const CONFIG_EXTS: &[&str] = &["ts", "mts", "cts", "js", "mjs", "cjs"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuiteKind {
    Playwright,
    Vitest,
    Go,
}

impl SuiteKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Playwright => "Playwright",
            Self::Vitest => "Vitest",
            Self::Go => "Go",
        }
    }

    pub fn runner(self) -> Option<RunnerKind> {
        match self {
            Self::Playwright => Some(RunnerKind::Playwright),
            Self::Vitest => Some(RunnerKind::Vitest),
            Self::Go => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Tool {
    pub bin: PathBuf,
    pub version: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Suite {
    /// Stable identifier: config (or marker) path relative to the scanned root.
    pub id: String,
    /// Display name, e.g. `web/app · Playwright (storybook)`.
    pub name: String,
    pub kind: SuiteKind,
    /// Working directory for the run.
    pub dir: PathBuf,
    /// Config to pass explicitly (`-c` / `--config`).
    pub config: Option<PathBuf>,
    pub tool: Option<Tool>,
    /// Reasons the suite cannot run.
    pub problems: Vec<String>,
    /// Things likely to make it fail (e.g. browsers not installed).
    pub warnings: Vec<String>,
    /// The directory that was scanned; the project config is looked up from
    /// here, as discovery did.
    pub project_root: PathBuf,
}

impl Suite {
    /// The project config's rules for this suite, read from disk now, so
    /// edits to the config or an `env_file` apply without a rescan.
    pub fn run_config(&self) -> anyhow::Result<RunConfig> {
        match ProjectConfig::find(&self.project_root) {
            None => Ok(RunConfig::default()),
            Some(config) => config?.resolve(self.kind, &self.dir),
        }
    }

    pub fn runnable(&self) -> bool {
        self.problems.is_empty() && self.kind.runner().is_some()
    }

    /// Case-insensitive match on name, id or runner (`playwright`, `vitest`).
    pub fn matches(&self, filter: &str) -> bool {
        let f = filter.to_lowercase();
        f == "all"
            || self.kind.label().to_lowercase() == f
            || self.name.to_lowercase().contains(&f)
            || self.id.to_lowercase().contains(&f)
    }
}

#[tracing::instrument(level = "info", skip_all, fields(root = %root.display(), suites = tracing::field::Empty))]
pub fn discover(root: &Path) -> Vec<Suite> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_owned());
    let mut files = Vec::new();
    walk(&root, 0, &mut files);
    files.sort();

    let mut suites = Vec::new();
    let mut vitest_workspaces: Vec<PathBuf> = Vec::new();

    // Vitest workspaces first so their member configs can be suppressed.
    for f in &files {
        let name = file_name(f);
        let is_workspace = name.starts_with("vitest.workspace.")
            || (is_config(&name, "vitest")
                && read(f).is_some_and(|t| t.contains("projects:") || t.contains("workspace:")));
        if is_workspace {
            vitest_workspaces.push(f.parent().unwrap().to_owned());
        }
    }

    for f in &files {
        let name = file_name(f);
        let dir = f.parent().unwrap().to_owned();
        let in_workspace = |d: &Path| vitest_workspaces.iter().any(|w| d != w && d.starts_with(w));

        if is_config(&name, "playwright") {
            let variant = name
                .trim_start_matches("playwright")
                .split(".config.")
                .next()
                .unwrap_or("")
                .trim_matches(|c| c == '.' || c == '-' || c == '_')
                .to_owned();
            suites.push(node_suite(&root, f, &dir, SuiteKind::Playwright, &variant));
        } else if is_config(&name, "vitest") || name.starts_with("vitest.workspace.") {
            if in_workspace(&dir) {
                continue;
            }
            let variant = name
                .trim_start_matches("vitest")
                .split(".config.")
                .next()
                .unwrap_or("")
                .trim_matches(|c| c == '.' || c == '-' || c == '_')
                .to_owned();
            let variant = if variant == "workspace" { String::new() } else { variant };
            if suites
                .iter()
                .any(|s: &Suite| s.kind == SuiteKind::Vitest && s.dir == dir)
            {
                continue; // vitest.config + vitest.workspace in one dir: one suite
            }
            suites.push(node_suite(&root, f, &dir, SuiteKind::Vitest, &variant));
        } else if is_config(&name, "vite")
            && !in_workspace(&dir)
            && !files
                .iter()
                .any(|o| o.parent() == Some(&dir) && is_config(&file_name(o), "vitest"))
            && read(f).is_some_and(|t| t.contains("test:") || t.contains("test :"))
        {
            suites.push(node_suite(&root, f, &dir, SuiteKind::Vitest, ""));
        } else if name == "go.mod" {
            suites.push(Suite {
                id: rel(&root, f),
                name: format!("{} · Go", rel_dir(&root, &dir)),
                kind: SuiteKind::Go,
                dir,
                config: None,
                tool: None,
                problems: vec!["Go test runner support is planned".into()],
                warnings: vec![],
                project_root: root.clone(),
            });
        }
    }
    check_project_config(&root, &mut suites);
    tracing::Span::current().record("suites", suites.len());
    suites
}

/// Check the project config against each runnable suite so mistakes show
/// up in the suite list. Runs read the config again when they start
/// (`Suite::run_config`), so these are warnings: fixing the file is enough,
/// no rescan needed, and a run refuses to start while it is still broken.
fn check_project_config(root: &Path, suites: &mut [Suite]) {
    let Some(config) = ProjectConfig::find(root) else {
        return;
    };
    for suite in suites.iter_mut().filter(|s| s.kind.runner().is_some()) {
        let checked = match &config {
            Ok(c) => c.resolve(suite.kind, &suite.dir).map(drop),
            Err(e) => Err(anyhow::anyhow!("{e:#}")),
        };
        if let Err(e) = checked {
            suite
                .warnings
                .push(format!("{e:#} (runs won't start until it's fixed)"));
        }
    }
}

fn node_suite(root: &Path, config: &Path, dir: &Path, kind: SuiteKind, variant: &str) -> Suite {
    let mut name = format!("{} · {}", rel_dir(root, dir), kind.label());
    if !variant.is_empty() {
        name.push_str(&format!(" ({variant})"));
    }
    let (bin_name, pkgs): (&str, &[&str]) = match kind {
        SuiteKind::Playwright => ("playwright", &["@playwright/test", "playwright"]),
        _ => ("vitest", &["vitest"]),
    };

    let mut problems = Vec::new();
    let mut warnings = Vec::new();
    let tool = find_bin(dir, bin_name).map(|bin| Tool {
        bin,
        version: pkgs.iter().find_map(|p| package_version(dir, p)),
    });
    if tool.is_none() {
        problems.push(format!(
            "{} is not installed here (no node_modules/.bin/{bin_name} in {} or above); run your package manager's install",
            kind.label(),
            rel_dir(root, dir)
        ));
    }
    if kind == SuiteKind::Playwright
        && tool.is_some()
        && let Some(w) = missing_chromium(dir)
    {
        warnings.push(w);
    }

    Suite {
        id: rel(root, config),
        name,
        kind,
        dir: dir.to_owned(),
        config: Some(config.to_owned()),
        tool,
        problems,
        warnings,
        project_root: root.to_owned(),
    }
}

/// Nearest `node_modules/.bin/<name>` from `dir` upward.
pub fn find_bin(dir: &Path, name: &str) -> Option<PathBuf> {
    dir.ancestors()
        .map(|d| d.join("node_modules/.bin").join(name))
        .find(|p| p.exists())
}

/// Resolve a package the way Node does from `from` (nearest node_modules
/// upward), following symlinks so pnpm's store layout works.
fn resolve_pkg(from: &Path, name: &str) -> Option<PathBuf> {
    from.ancestors()
        .map(|d| d.join("node_modules").join(name))
        .find(|p| p.join("package.json").exists())
        .and_then(|p| p.canonicalize().ok())
}

pub fn package_version(from: &Path, name: &str) -> Option<String> {
    let dir = resolve_pkg(from, name)?;
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("package.json")).ok()?).ok()?;
    json.get("version")?.as_str().map(str::to_owned)
}

/// Warn when neither Chromium build that this Playwright version expects is in
/// the browser cache. Each Playwright version pins its own browser revisions.
fn missing_chromium(dir: &Path) -> Option<String> {
    let test_pkg = resolve_pkg(dir, "@playwright/test").or_else(|| resolve_pkg(dir, "playwright"))?;
    let playwright = if test_pkg.ends_with("playwright") {
        test_pkg.clone()
    } else {
        resolve_pkg(&test_pkg, "playwright")?
    };
    let core = resolve_pkg(&playwright, "playwright-core")?;
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(core.join("browsers.json")).ok()?).ok()?;
    let browsers = json.get("browsers")?.as_array()?;
    let revision = |name: &str| {
        browsers
            .iter()
            .find(|b| b.get("name").and_then(|n| n.as_str()) == Some(name))
            .and_then(|b| b.get("revision")?.as_str().map(str::to_owned))
    };

    let cache = std::env::var_os("PLAYWRIGHT_BROWSERS_PATH")
        .filter(|p| p != "0")
        .map(PathBuf::from)
        .or_else(default_browsers_path)?;
    let installed = ["chromium", "chromium-headless-shell"]
        .iter()
        .any(|name| revision(name).is_some_and(|rev| cache.join(format!("{}-{rev}", name.replace('-', "_"))).exists()));
    if installed {
        return None;
    }
    let version = package_version(dir, "@playwright/test").unwrap_or_default();
    Some(format!(
        "Chromium for Playwright {version} is not installed; run `playwright install chromium` in this directory"
    ))
}

/// Playwright's default browser cache for this platform (its registry's
/// `cacheDirectory`): `~/Library/Caches` on macOS, `$XDG_CACHE_HOME` or
/// `~/.cache` elsewhere.
fn default_browsers_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from);
    let cache = if cfg!(target_os = "macos") {
        home?.join("Library/Caches")
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .or_else(|| home.map(|h| h.join(".cache")))?
    };
    Some(cache.join("ms-playwright"))
}

fn is_config(file_name: &str, tool: &str) -> bool {
    let Some(rest) = file_name.strip_prefix(tool) else {
        return false;
    };
    // `playwright.config.ts`, `playwright.storybook.config.ts`, `playwright-ct.config.ts`
    let Some((_, ext)) = rest.rsplit_once(".config.") else {
        return false;
    };
    (rest.starts_with(".config.") || rest.starts_with('.') || rest.starts_with('-') || rest.starts_with('_'))
        && CONFIG_EXTS.contains(&ext)
}

fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            let path = e.path();
            // A `.git` (a directory, or a file pointing at a worktree's gitdir)
            // marks another checkout: a linked worktree or a nested clone.
            // Its suites belong to it, not to the tree being scanned.
            if !name.starts_with('.') && !SKIP_DIRS.contains(&name.as_str()) && !path.join(".git").exists() {
                walk(&path, depth + 1, out);
            }
        } else if name.starts_with("playwright")
            || name.starts_with("vitest")
            || name.starts_with("vite.config.")
            || name == "go.mod"
        {
            out.push(e.path());
        }
    }
}

fn read(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok()
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root).unwrap_or(p).display().to_string()
}

/// A suite directory relative to the scanned root; the root itself is named
/// after its folder ("acme-shop", not ".").
fn rel_dir(root: &Path, dir: &Path) -> String {
    match dir.strip_prefix(root) {
        Ok(r) if r.as_os_str().is_empty() => root
            .file_name()
            .map_or_else(|| ".".to_owned(), |n| n.to_string_lossy().into_owned()),
        Ok(r) => r.display().to_string(),
        Err(_) => dir.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    fn fake_pkg(root: &Path, at: &str, name: &str, version: &str, bin: &str) {
        touch(
            root,
            &format!("{at}/node_modules/{name}/package.json"),
            &format!(r#"{{"name":"{name}","version":"{version}"}}"#),
        );
        touch(root, &format!("{at}/node_modules/.bin/{bin}"), "");
    }

    #[test]
    fn monorepo_with_two_playwright_versions_and_vitest() {
        let root = std::env::temp_dir().join(format!("testudo-discover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        touch(&root, "go.mod", "module x");
        touch(&root, "apps/shop/playwright.config.ts", "");
        touch(&root, "apps/shop/playwright.storybook.config.ts", "");
        fake_pkg(&root, "apps/shop", "@playwright/test", "1.60.0", "playwright");
        touch(&root, "apps/admin/playwright.config.ts", "");
        fake_pkg(&root, "apps/admin", "@playwright/test", "1.64.1", "playwright");
        touch(
            &root,
            "apps/admin/vite.config.ts",
            "export default { test: { environment: 'jsdom' } }",
        );
        fake_pkg(&root, "apps/admin", "vitest", "4.1.0", "vitest");
        touch(&root, "apps/docs/playwright.config.ts", ""); // not installed
        touch(&root, "apps/shop/node_modules/foo/playwright.config.ts", ""); // ignored
        touch(&root, "packages/ui/vite.config.ts", "export default {}"); // no test block

        let suites = discover(&root);
        let by_id = |id: &str| {
            suites
                .iter()
                .find(|s| s.id == id)
                .unwrap_or_else(|| panic!("missing {id}"))
        };
        let names: Vec<&str> = suites.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(suites.len(), 6, "{names:?}");

        let shop = by_id("apps/shop/playwright.config.ts");
        assert_eq!(shop.tool.as_ref().unwrap().version.as_deref(), Some("1.60.0"));
        assert!(
            by_id("apps/shop/playwright.storybook.config.ts")
                .name
                .ends_with("Playwright (storybook)")
        );
        assert_eq!(
            by_id("apps/admin/playwright.config.ts")
                .tool
                .as_ref()
                .unwrap()
                .version
                .as_deref(),
            Some("1.64.1")
        );
        let vite = by_id("apps/admin/vite.config.ts");
        assert_eq!((vite.kind, vite.runnable()), (SuiteKind::Vitest, true));
        let docs = by_id("apps/docs/playwright.config.ts");
        assert!(!docs.runnable() && docs.problems[0].contains("not installed"));
        assert!(!by_id("go.mod").runnable());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn vitest_workspace_owns_member_configs() {
        let root = std::env::temp_dir().join(format!("testudo-discover-ws-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        touch(&root, "vitest.workspace.ts", "export default ['packages/*']");
        touch(&root, "packages/a/vitest.config.ts", "");
        touch(&root, "packages/b/vitest.config.ts", "");
        let suites = discover(&root);
        assert_eq!(suites.len(), 1);
        assert_eq!(suites[0].id, "vitest.workspace.ts");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn suite_at_the_root_is_named_after_the_folder() {
        let root = std::env::temp_dir()
            .join(format!("testudo-rootname-{}", std::process::id()))
            .join("acme-shop");
        let _ = std::fs::remove_dir_all(&root);
        touch(&root, "playwright.config.ts", "export default {}");
        let suites = discover(&root);
        assert_eq!(suites[0].name, "acme-shop · Playwright");
        std::fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }

    #[test]
    fn nested_worktrees_and_checkouts_are_skipped() {
        let root = std::env::temp_dir().join(format!("testudo-discover-wt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        touch(&root, ".git/HEAD", "ref: refs/heads/main"); // the root's own repo: still scanned
        touch(&root, "apps/web/playwright.config.ts", "");
        // Linked worktrees: `.git` is a file pointing at the main repo's gitdir.
        touch(
            &root,
            "worktrees/fix-cart/.git",
            "gitdir: /src/shop/.git/worktrees/fix-cart",
        );
        touch(&root, "worktrees/fix-cart/apps/web/playwright.config.ts", "");
        touch(
            &root,
            ".claude/worktrees/feat/.git",
            "gitdir: /src/shop/.git/worktrees/feat",
        );
        touch(&root, ".claude/worktrees/feat/apps/web/playwright.config.ts", "");
        // A separate clone inside the tree.
        touch(&root, "vendor-clone/.git/HEAD", "ref: refs/heads/main");
        touch(&root, "vendor-clone/vitest.config.ts", "");
        let ids: Vec<String> = discover(&root).into_iter().map(|s| s.id).collect();
        assert_eq!(ids, ["apps/web/playwright.config.ts"]);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn project_config_is_read_when_a_run_starts() {
        use crate::RunSpec;
        let root = std::env::temp_dir().join(format!("testudo-discover-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        touch(&root, "apps/web/playwright.config.ts", "");
        fake_pkg(&root, "apps/web", "@playwright/test", "1.60.0", "playwright");
        touch(&root, "apps/web/vitest.config.ts", "");
        fake_pkg(&root, "apps/web", "vitest", "4.1.0", "vitest");
        touch(
            &root,
            ".testudo/config.json",
            "{ rules: [{ args: ['--g'] }, { path: 'apps/*', runner: 'vitest', env: { V: '1' }, args: ['--v'] }] }",
        );
        let suites = discover(&root);
        let suite = |k: SuiteKind| suites.iter().find(|s| s.kind == k).unwrap();
        let spec = |k: SuiteKind| RunSpec::for_suite(suite(k), vec!["--user".into()]);
        let vitest = spec(SuiteKind::Vitest).unwrap();
        assert_eq!(vitest.args, ["--g", "--v", "--user"]);
        assert_eq!(vitest.env, [("V".to_owned(), "1".to_owned())]);
        assert_eq!(spec(SuiteKind::Playwright).unwrap().args, ["--g", "--user"]);

        // Edited after the scan: the next run uses the new rules, env_file included.
        touch(&root, "apps/web/.env.test", "FROM_FILE=yes\n");
        touch(
            &root,
            ".testudo/config.json",
            "{ rules: [{ path: 'apps/web', env_file: 'apps/web/.env.test', env: { V: '2' } }] }",
        );
        let vitest = spec(SuiteKind::Vitest).unwrap();
        assert_eq!(vitest.args, ["--user"]);
        assert_eq!(
            vitest.env,
            [
                ("FROM_FILE".to_owned(), "yes".to_owned()),
                ("V".to_owned(), "2".to_owned())
            ]
        );

        // Broken: a warning in the list, suites stay runnable, but no run starts.
        touch(&root, ".testudo/config.json", "{ rules: [{ runner: 'jest' }] }");
        let suites = discover(&root);
        assert!(
            suites
                .iter()
                .all(|s| s.runnable() && s.warnings.iter().any(|w| w.contains("unknown runner")))
        );
        let err = RunSpec::for_suite(&suites[0], vec![]).unwrap_err();
        assert!(format!("{err:#}").contains("unknown runner"), "{err:#}");
        // Fixed without a rescan: runs start again.
        touch(&root, ".testudo/config.json", "{ rules: [] }");
        assert!(RunSpec::for_suite(&suites[0], vec![]).is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn config_name_matching() {
        assert!(is_config("playwright.config.ts", "playwright"));
        assert!(is_config("playwright.storybook.config.mjs", "playwright"));
        assert!(is_config("playwright-ct.config.ts", "playwright"));
        assert!(!is_config("playwright.ts", "playwright"));
        assert!(!is_config("playwrightish.config.ts", "playwright"));
        assert!(is_config("vite.config.ts", "vite"));
        assert!(!is_config("vitest.config.ts", "vite"));
    }
}
