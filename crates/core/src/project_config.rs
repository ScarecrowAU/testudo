//! Project config: `.testudo/config.json`, rules that give runs extra
//! environment variables and arguments.
//!
//! ```json5
//! {
//!   rules: [
//!     { runner: "playwright", env: { CI: "1" } },              // global
//!     {
//!       path: "apps/web/**",                                   // local
//!       runner: ["playwright", "vitest"],
//!       env: { API_URL: "http://localhost:4000" },
//!       env_file: "apps/web/.env.test",
//!       args: ["--retries=1"],
//!       replaceGlobalArgs: true,
//!     },
//!   ],
//! }
//! ```
//!
//! The file is JSON5 (comments, unquoted keys, single quotes, trailing commas).
//! Testudo uses the nearest one at or above the scanned directory, stopping at
//! the repository root. Paths in it (`path`, `env_file`) are relative to the
//! directory holding `.testudo`.
//!
//! A rule without `path` is global; one with `path` is local and applies to
//! suites whose directory matches the glob (`*` stays within one folder, `**`
//! crosses folders). `runner` limits a rule to some runners (default: all).
//! Matching rules merge, globals first, then locals, each group in file order,
//! so local rules win conflicts:
//!
//! - `env`: merged by name, later wins. Within a rule, `env` overrides `env_file`.
//! - `args`: appended (global, then local; the runner args typed in the GUI or
//!   passed after `--` come last). A matching local rule with
//!   `replaceGlobalArgs: true` drops the global rules' args.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use globset::{GlobBuilder, GlobMatcher};
use serde::Deserialize;

use crate::discover::SuiteKind;

/// Location of the config file, relative to the project root.
pub const CONFIG_PATH: &str = ".testudo/config.json";

/// What the matching rules add to one suite's runs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunConfig {
    /// Extra environment variables, sorted by name.
    pub env: Vec<(String, String)>,
    /// Arguments placed before the user's runner args.
    pub args: Vec<String>,
}

#[derive(Debug)]
pub struct ProjectConfig {
    /// The directory holding `.testudo`; rule paths are relative to it.
    pub root: PathBuf,
    rules: Vec<Rule>,
}

#[derive(Debug)]
struct Rule {
    path: Option<GlobMatcher>,
    runners: Option<Vec<SuiteKind>>,
    env: BTreeMap<String, String>,
    env_file: Option<PathBuf>,
    args: Vec<String>,
    replace_global_args: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    #[serde(default)]
    rules: Vec<RawRule>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRule {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    runner: Option<OneOrMany>,
    #[serde(default)]
    env: BTreeMap<String, Scalar>,
    #[serde(default)]
    env_file: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default, rename = "replaceGlobalArgs", alias = "replace_global_args")]
    replace_global_args: bool,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

/// An env value: strings as they are, numbers and booleans as written.
#[derive(Deserialize)]
#[serde(untagged)]
enum Scalar {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
}

impl Scalar {
    fn into_string(self) -> String {
        match self {
            Self::Str(s) => s,
            Self::Int(n) => n.to_string(),
            Self::Float(n) => n.to_string(),
            Self::Bool(b) => b.to_string(),
        }
    }
}

impl ProjectConfig {
    /// The nearest `.testudo/config.json` at or above `dir`, stopping at the
    /// repository root (a directory with `.git`). `None` when there is none;
    /// `Some(Err)` when it can't be read or is invalid.
    pub fn find(dir: &Path) -> Option<Result<Self>> {
        let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_owned());
        for d in dir.ancestors() {
            let file = d.join(CONFIG_PATH);
            if file.is_file() {
                return Some(Self::load(&file, d));
            }
            if d.join(".git").exists() {
                break;
            }
        }
        None
    }

    fn load(file: &Path, root: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
        Self::parse(&text, root).with_context(|| format!("invalid {}", crate::tilde(file)))
    }

    pub fn parse(text: &str, root: &Path) -> Result<Self> {
        let raw: RawFile = json5::from_str(text)?;
        let rules = raw
            .rules
            .into_iter()
            .enumerate()
            .map(|(i, r)| Rule::compile(r).with_context(|| format!("rules[{i}]")))
            .collect::<Result<_>>()?;
        Ok(Self {
            root: root.to_owned(),
            rules,
        })
    }

    /// Merge the rules that apply to a `kind` suite running in `dir`. Fails
    /// when a matching rule's `env_file` can't be read.
    pub fn resolve(&self, kind: SuiteKind, dir: &Path) -> Result<RunConfig> {
        let rel = dir
            .strip_prefix(&self.root)
            .ok()
            .map(|p| p.to_string_lossy().replace('\\', "/"));
        let applies = |r: &Rule| {
            r.runners.as_ref().is_none_or(|k| k.contains(&kind))
                && match (&r.path, &rel) {
                    (None, _) => true,
                    (Some(glob), Some(rel)) => glob.is_match(rel),
                    (Some(_), None) => false,
                }
        };
        let (global, local): (Vec<&Rule>, Vec<&Rule>) =
            self.rules.iter().filter(|r| applies(r)).partition(|r| r.path.is_none());

        let mut env = BTreeMap::new();
        for rule in global.iter().chain(&local) {
            if let Some(file) = &rule.env_file {
                let path = self.root.join(file);
                let text = std::fs::read_to_string(&path)
                    .with_context(|| format!("env_file {} in {CONFIG_PATH}", crate::tilde(&path)))?;
                env.extend(parse_dotenv(&text));
            }
            env.extend(rule.env.clone());
        }
        let replace = local.iter().any(|r| r.replace_global_args);
        let args = (if replace { &[][..] } else { &global[..] })
            .iter()
            .chain(&local)
            .flat_map(|r| r.args.iter().cloned())
            .collect();
        Ok(RunConfig {
            env: env.into_iter().collect(),
            args,
        })
    }
}

impl Rule {
    fn compile(raw: RawRule) -> Result<Self> {
        let path = match raw
            .path
            .as_deref()
            .map(|p| p.trim().trim_start_matches("./").trim_end_matches('/'))
        {
            None => None,
            Some("") => bail!("`path` is empty; leave it out for a global rule"),
            Some(p) => Some(
                GlobBuilder::new(p)
                    .literal_separator(true)
                    .build()
                    .with_context(|| format!("`path` glob {p:?}"))?
                    .compile_matcher(),
            ),
        };
        if raw.replace_global_args && path.is_none() {
            bail!("`replaceGlobalArgs` only applies to rules with a `path`");
        }
        let runners = match raw.runner {
            None => None,
            Some(OneOrMany::One(r)) => Some(vec![runner_kind(&r)?]),
            Some(OneOrMany::Many(rs)) => Some(rs.iter().map(|r| runner_kind(r)).collect::<Result<_>>()?),
        };
        Ok(Self {
            path,
            runners,
            env: raw.env.into_iter().map(|(k, v)| (k, v.into_string())).collect(),
            env_file: raw.env_file.map(PathBuf::from),
            args: raw.args,
            replace_global_args: raw.replace_global_args,
        })
    }
}

fn runner_kind(name: &str) -> Result<SuiteKind> {
    match name.to_ascii_lowercase().as_str() {
        "playwright" => Ok(SuiteKind::Playwright),
        "vitest" => Ok(SuiteKind::Vitest),
        _ => bail!("unknown runner {name:?} (expected \"playwright\" or \"vitest\")"),
    }
}

/// `KEY=value` lines as in a `.env` file: `#` comments, blank lines, an
/// optional `export ` prefix, and single- or double-quoted values (double
/// quotes understand `\n`). Lines without `=` are ignored.
pub fn parse_dotenv(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let value = value.trim();
        let value = if let Some(v) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
            v.replace("\\n", "\n").replace("\\\"", "\"")
        } else if let Some(v) = value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')) {
            v.to_owned()
        } else {
            // Unquoted: a ` #` starts a trailing comment.
            value.split(" #").next().unwrap_or("").trim_end().to_owned()
        };
        out.push((key.to_owned(), value));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &str = "/repo";

    fn cfg(text: &str) -> ProjectConfig {
        ProjectConfig::parse(text, Path::new(ROOT)).unwrap()
    }

    fn at(c: &ProjectConfig, kind: SuiteKind, dir: &str) -> RunConfig {
        c.resolve(kind, &Path::new(ROOT).join(dir)).unwrap()
    }

    fn env<'a>(r: &'a RunConfig, key: &str) -> Option<&'a str> {
        r.env.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    #[test]
    fn parses_the_json5_example() {
        let c = cfg(r#"{
            // comments, unquoted keys, single quotes, trailing commas
            rules: [
              {
                "path": "app/something/*",
                "runner": ['playwright', 'vitest'],
                "env": { SOME_ENV: 'blah', PORT: 3000, DEBUG: true },
                "args": ["--some-arg"],
              },
            ],
        }"#);
        let r = at(&c, SuiteKind::Vitest, "app/something/web");
        assert_eq!(env(&r, "SOME_ENV"), Some("blah"));
        assert_eq!(env(&r, "PORT"), Some("3000"));
        assert_eq!(env(&r, "DEBUG"), Some("true"));
        assert_eq!(r.args, ["--some-arg"]);
    }

    #[test]
    fn path_globs_match_the_suite_directory() {
        let c = cfg(r#"{ rules: [
            { path: "apps/*", args: ["one-level"] },
            { path: "apps/**", args: ["any-depth"] },
            { path: "./apps/web/", args: ["exact"] },
        ] }"#);
        assert_eq!(
            at(&c, SuiteKind::Playwright, "apps/web").args,
            ["one-level", "any-depth", "exact"]
        );
        assert_eq!(at(&c, SuiteKind::Playwright, "apps/web/e2e").args, ["any-depth"]);
        assert!(at(&c, SuiteKind::Playwright, "packages/ui").args.is_empty());
        // A suite outside the config's root matches no local rule.
        assert!(
            c.resolve(SuiteKind::Playwright, Path::new("/elsewhere/apps/web"))
                .unwrap()
                .args
                .is_empty()
        );
    }

    #[test]
    fn runner_filter() {
        let c = cfg(r#"{ rules: [
            { runner: "playwright", env: { WHO: "pw" } },
            { runner: ["vitest"], env: { WHO: "vt" } },
            { env: { ALL: "1" } },
        ] }"#);
        let pw = at(&c, SuiteKind::Playwright, "x");
        assert_eq!((env(&pw, "WHO"), env(&pw, "ALL")), (Some("pw"), Some("1")));
        assert_eq!(env(&at(&c, SuiteKind::Vitest, "x"), "WHO"), Some("vt"));
    }

    #[test]
    fn locals_win_over_globals_whatever_the_file_order() {
        let c = cfg(r#"{ rules: [
            { path: "apps/**", env: { A: "local", L: "1" }, args: ["--local"] },
            { env: { A: "global", B: "global" }, args: ["--global"] },
            { path: "apps/web", env: { L: "2" } },
        ] }"#);
        let r = at(&c, SuiteKind::Playwright, "apps/web");
        assert_eq!(env(&r, "A"), Some("local"));
        assert_eq!(env(&r, "B"), Some("global"));
        assert_eq!(env(&r, "L"), Some("2"), "later local rule wins");
        assert_eq!(r.args, ["--global", "--local"]);
    }

    #[test]
    fn replace_global_args() {
        let c = cfg(r#"{ rules: [
            { args: ["--global"] },
            { path: "apps/web", args: ["--web"], replaceGlobalArgs: true },
            { path: "apps/**", args: ["--apps"] },
        ] }"#);
        assert_eq!(at(&c, SuiteKind::Playwright, "apps/web").args, ["--web", "--apps"]);
        assert_eq!(at(&c, SuiteKind::Playwright, "apps/admin").args, ["--global", "--apps"]);
    }

    #[test]
    fn env_file_is_read_relative_to_the_root_and_inline_env_wins() {
        let root = std::env::temp_dir().join(format!("testudo-projcfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("apps/web")).unwrap();
        std::fs::write(root.join("apps/web/.env.test"), "FROM_FILE=1\nSHARED=file\n").unwrap();
        let c = ProjectConfig::parse(
            r#"{ rules: [{ path: "apps/web", env_file: "apps/web/.env.test", env: { SHARED: "inline" } }] }"#,
            &root,
        )
        .unwrap();
        let r = c.resolve(SuiteKind::Vitest, &root.join("apps/web")).unwrap();
        assert_eq!(env(&r, "FROM_FILE"), Some("1"));
        assert_eq!(env(&r, "SHARED"), Some("inline"));

        std::fs::remove_file(root.join("apps/web/.env.test")).unwrap();
        let err = c.resolve(SuiteKind::Vitest, &root.join("apps/web")).unwrap_err();
        assert!(format!("{err:#}").contains("env_file"), "{err:#}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn invalid_configs_are_explained() {
        let err = |text: &str| format!("{:#}", ProjectConfig::parse(text, Path::new(ROOT)).unwrap_err());
        assert!(err(r#"{ rules: [{ runner: "playright" }] }"#).contains("unknown runner \"playright\""));
        assert!(err(r#"{ rules: [{ replaceGlobalArgs: true }] }"#).contains("only applies to rules with a `path`"));
        assert!(err(r#"{ rules: [{ pathh: "x" }] }"#).contains("pathh"));
        assert!(err(r#"{ rules: [{ path: "a/[" }] }"#).contains("glob"));
        assert!(!err("{ rules: [").is_empty(), "syntax errors are errors");
    }

    #[test]
    fn find_stops_at_the_repository_root() {
        let base = std::env::temp_dir().join(format!("testudo-projcfg-find-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("apps/web")).unwrap();
        std::fs::create_dir_all(base.join(".testudo")).unwrap();
        std::fs::write(base.join(CONFIG_PATH), "{ rules: [] }").unwrap();
        assert!(
            ProjectConfig::find(&repo.join("apps/web")).is_none(),
            "outside the repo is ignored"
        );

        std::fs::create_dir_all(repo.join(".testudo")).unwrap();
        std::fs::write(repo.join(CONFIG_PATH), "{ rules: [{ env: { X: '1' } }] }").unwrap();
        let c = ProjectConfig::find(&repo.join("apps/web")).unwrap().unwrap();
        assert_eq!(c.root, repo.canonicalize().unwrap());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn dotenv_parsing() {
        let vars = parse_dotenv(
            "# comment\n\nexport A=1\nB = two words # note\nC=\"multi\\nline\"\nD='$NOT_EXPANDED'\nnot a var\nE=\n",
        );
        assert_eq!(
            vars,
            [
                ("A".into(), "1".into()),
                ("B".into(), "two words".into()),
                ("C".into(), "multi\nline".into()),
                ("D".into(), "$NOT_EXPANDED".into()),
                ("E".into(), String::new()),
            ]
        );
    }
}
