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

/// Where the config's JSON Schema is published, for `"$schema"`.
pub const SCHEMA_URL: &str =
    "https://raw.githubusercontent.com/ScarecrowAU/testudo/main/schemas/testudo-config.schema.json";

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

// The JSON Schema (`schemas/testudo-config.schema.json`) is generated from
// these types and their doc comments; see `tests::schema_is_up_to_date`.

/// Testudo project config in `.testudo/config.json`, written in JSON5. Its
/// rules give test runs extra environment variables and arguments.
#[derive(Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[cfg_attr(test, schemars(title = "Testudo project config"))]
#[serde(deny_unknown_fields)]
struct RawFile {
    /// JSON Schema reference for editors. Testudo ignores it.
    #[serde(default, rename = "$schema")]
    #[allow(dead_code)]
    schema: Option<String>,
    /// Rules without `path` are global; rules with `path` are local. Matching
    /// rules merge, globals first then locals, each in file order, so local
    /// rules win conflicts.
    #[serde(default)]
    rules: Vec<RawRule>,
}

/// One rule. Every field is optional.
#[derive(Deserialize)]
#[cfg_attr(test, derive(schemars::JsonSchema))]
#[cfg_attr(test, schemars(rename = "Rule"))]
#[serde(deny_unknown_fields)]
struct RawRule {
    /// Glob matched against the suite's directory, relative to the folder
    /// holding `.testudo`: `*` stays within one folder, `**` crosses folders.
    /// Without it the rule is global.
    #[serde(default)]
    path: Option<String>,
    /// The runners this rule applies to. Without it, the rule applies to all
    /// runners.
    #[serde(default)]
    #[cfg_attr(test, schemars(with = "Option<schema::Runners>"))]
    runner: Option<OneOrMany>,
    /// Environment variables for the run. Merged by name; later rules win.
    /// Overrides values from this rule's `env_file`.
    #[serde(default)]
    env: BTreeMap<String, Scalar>,
    /// A `.env` file of KEY=value lines, relative to the folder holding
    /// `.testudo`. It must exist when the rule applies.
    #[serde(default)]
    env_file: Option<String>,
    /// Runner arguments, appended: global rules, then local rules, then the
    /// runner args typed in the GUI or given after `--`.
    #[serde(default)]
    args: Vec<String>,
    /// On a local rule, one with `path`, drop the global rules' `args` for
    /// matching suites.
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
#[cfg_attr(test, derive(schemars::JsonSchema))]
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

/// Schema-only types: what `runner` accepts, spelled out for editors.
#[cfg(test)]
mod schema {
    #[derive(schemars::JsonSchema)]
    #[serde(rename_all = "lowercase")]
    #[allow(dead_code)]
    pub enum Runner {
        Playwright,
        Vitest,
    }

    #[derive(schemars::JsonSchema)]
    #[serde(untagged)]
    #[allow(dead_code)]
    pub enum Runners {
        One(Runner),
        Many(Vec<Runner>),
    }
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

    /// `schemas/testudo-config.schema.json` matches these types. Regenerate:
    /// `TESTUDO_UPDATE_SCHEMA=1 cargo test -p testudo-core schema_is_up_to_date`.
    fn generated_schema() -> serde_json::Value {
        let root = schemars::generate::SchemaSettings::draft07()
            .into_generator()
            .into_root_schema_for::<RawFile>();
        let mut schema = serde_json::to_value(root).unwrap();
        schema.as_object_mut().unwrap().insert("$id".into(), SCHEMA_URL.into());
        sort_keys(schema)
    }

    /// Keys in sorted order at every level. Other workspace crates turn on
    /// serde_json's `preserve_order`, which would otherwise make the file's key
    /// order depend on which crates are being built.
    fn sort_keys(value: serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Object(map) => {
                let mut entries: Vec<_> = map.into_iter().collect();
                entries.sort_by(|a, b| a.0.cmp(&b.0));
                serde_json::Value::Object(entries.into_iter().map(|(k, v)| (k, sort_keys(v))).collect())
            }
            serde_json::Value::Array(items) => serde_json::Value::Array(items.into_iter().map(sort_keys).collect()),
            other => other,
        }
    }

    #[test]
    fn schema_is_up_to_date() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas/testudo-config.schema.json");
        let text = serde_json::to_string_pretty(&generated_schema()).unwrap() + "\n";
        if std::env::var_os("TESTUDO_UPDATE_SCHEMA").is_some() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &text).unwrap();
            return;
        }
        let current = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            current == text,
            "{} is out of date; regenerate it with \
             `TESTUDO_UPDATE_SCHEMA=1 cargo test -p testudo-core schema_is_up_to_date`",
            path.display()
        );
    }

    /// The schema accepts what Testudo accepts (the README's example
    /// included) and flags the mistakes Testudo rejects.
    #[test]
    fn schema_agrees_with_the_parser() {
        let validator = jsonschema::validator_for(&generated_schema()).unwrap();
        let readme = include_str!("../../../README.md");
        let readme_example = readme
            .split("```json5\n")
            .nth(1)
            .and_then(|rest| rest.split("```").next())
            .expect("README has a json5 config example");
        let good = [
            readme_example,
            r#"{ "$schema": "x", rules: [] }"#,
            r#"{ rules: [{ path: 'a/*', runner: ['playwright', 'vitest'], env: { A: 'x', N: 3000, B: true },
                 env_file: '.env', args: ['--x'], replaceGlobalArgs: true }] }"#,
            "{}",
        ];
        for text in good {
            let value: serde_json::Value = json5::from_str(text).unwrap();
            let errors: Vec<String> = validator.iter_errors(&value).map(|e| e.to_string()).collect();
            assert!(errors.is_empty(), "schema rejects {text}: {errors:?}");
            ProjectConfig::parse(text, Path::new(ROOT)).unwrap_or_else(|e| panic!("Testudo rejects {text}: {e:#}"));
        }
        let bad = [
            r#"{ rules: [{ runner: "playright" }] }"#,
            r#"{ rules: [{ pathh: "x" }] }"#,
            r#"{ rulez: [] }"#,
            r#"{ rules: [{ env: { A: [1] } }] }"#,
            r#"{ rules: [{ args: "--x" }] }"#,
        ];
        for text in bad {
            let value: serde_json::Value = json5::from_str(text).unwrap();
            assert!(!validator.is_valid(&value), "schema accepts {text}");
            assert!(
                ProjectConfig::parse(text, Path::new(ROOT)).is_err(),
                "Testudo accepts {text}"
            );
        }
    }

    #[test]
    fn dollar_schema_is_accepted() {
        let c = cfg(&format!(
            r#"{{ "$schema": "{SCHEMA_URL}", "rules": [{{ "env": {{ "A": "1" }} }}] }}"#
        ));
        assert_eq!(env(&at(&c, SuiteKind::Vitest, "x"), "A"), Some("1"));
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
