//! `kintrel.toml` (SPEC §10.1): check definitions, read from the root being checked, so the
//! checks are versioned with the code.

use serde::{Deserialize, Serialize};

pub const FILE: &str = "kintrel.toml";

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default, rename = "check")]
    pub checks: Vec<Check>,
    /// CD hooks: run on promoted roots only, once each, in order (SPEC §9.4).
    #[serde(default, rename = "deploy")]
    pub deploys: Vec<Deploy>,
    #[serde(default)]
    pub merge: Merge,
}

/// `[merge]`: how conflicts are handled beyond Tiers 0–2.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Merge {
    /// Ask Claude to propose a merge for each new conflict (Tier 3, SPEC §7.3). Off by default:
    /// it costs model usage. Proposals are validated and gated like any note.
    #[serde(default)]
    pub tier3: bool,
    /// The model for Tier 3 proposals (default `claude-opus-5-5`).
    #[serde(default)]
    pub tier3_model: Option<String>,
    /// A custom backend instead of the API or `claude -p`: a program and its arguments, given
    /// the prompt on stdin, printing the reply. Trusted like check commands (this file needs a
    /// person's approval to change).
    #[serde(default)]
    pub tier3_command: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Deploy {
    pub name: String,
    /// Shell command, run at the root of a checkout of the promoted root (`{root}`).
    pub run: String,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub timeout_s: Option<u64>,
    /// Sandboxed like checks, but with network on by default (deploys usually need it).
    #[serde(default = "yes")]
    pub sandbox: bool,
    #[serde(default = "yes")]
    pub network: bool,
    #[serde(default)]
    pub writable: Vec<String>,
    #[serde(default)]
    pub readable: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub name: String,
    #[serde(default)]
    pub tier: Tier,
    pub scope: Scope,
    /// Shell command, run at the checkout's root. Placeholders: `{file}` (file scope),
    /// `{package}` and `{package_dir}` (package scope), `{root}` (the checkout).
    pub run: String,
    /// Globs over repository paths; matched files are the check's inputs.
    pub inputs: Vec<String>,
    /// Environment variables passed through and made part of the cache key.
    #[serde(default)]
    pub env: Vec<String>,
    /// Commands whose output (tool versions) becomes part of the cache key.
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub gate: bool,
    #[serde(default)]
    pub timeout_s: Option<u64>,
    /// Run in the OS sandbox (default): writes only to the checkout, temp and `writable`; no
    /// network unless `network`; secret directories hidden. See `sandbox.rs`.
    #[serde(default = "yes")]
    pub sandbox: bool,
    #[serde(default)]
    pub network: bool,
    /// Extra writable directories, e.g. a shared build cache: absolute, `~/…`, `$VAR/…` (a
    /// variable listed in `env`, or HOME), or relative to the checkout.
    #[serde(default)]
    pub writable: Vec<String>,
    /// Extra readable directories, where reads are otherwise refused: on Windows, toolchains
    /// installed in the user profile (`~/.rustup`, `~/.cargo`). Same forms as `writable`.
    #[serde(default)]
    pub readable: Vec<String>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    #[default]
    Lint,
    Typecheck,
    Test,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    File,
    Package,
    Repo,
}

#[derive(Debug, thiserror::Error)]
#[error("kintrel.toml: {0}")]
pub struct ConfigError(pub String);

impl Config {
    pub fn parse(text: &str) -> Result<Config, ConfigError> {
        let config: Config = toml::from_str(text).map_err(|e| ConfigError(e.to_string()))?;
        let mut names = std::collections::HashSet::new();
        for c in &config.checks {
            let bad = |why: &str| Err(ConfigError(format!("check {:?}: {why}", c.name)));
            if c.name.is_empty() || !names.insert(&c.name) {
                return bad("names must be non-empty and unique");
            }
            if c.run.trim().is_empty() || c.inputs.is_empty() {
                return bad("needs `run` and at least one `inputs` glob");
            }
            if c.scope != Scope::Package && (c.run.contains("{package}") || c.run.contains("{package_dir}")) {
                return bad("`{package}` and `{package_dir}` only exist for scope = \"package\"");
            }
            if c.scope != Scope::File && c.run.contains("{file}") {
                return bad("`{file}` only exists for scope = \"file\"");
            }
            if let Some(e) = c.inputs.iter().find_map(|g| globset::Glob::new(g).err()) {
                return bad(&e.to_string());
            }
        }
        let mut deploys = std::collections::HashSet::new();
        for d in &config.deploys {
            if d.name.is_empty() || !deploys.insert(&d.name) || d.run.trim().is_empty() {
                return Err(ConfigError(format!("deploy {:?}: needs a unique name and a `run`", d.name)));
            }
        }
        Ok(config)
    }
}
