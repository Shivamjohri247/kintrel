//! Kintrel's CI engine (SPEC §10): checks from `kintrel.toml`, content-addressed cache keys, and a
//! local executor that runs checks in a fresh checkout of a snapshot.
//!
//! Results live in `ci.db`, keyed by cache key: two agents (or two promotion candidates) that
//! reach the same inputs share a result without running anything.

pub mod config;
pub mod packages;
pub mod sandbox;

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use globset::{Glob, GlobSet, GlobSetBuilder};
use kintrel_core::state::State;
use kintrel_core::tree::Kind;
use kintrel_core::{Hash, board, encode, worktree};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

pub use config::{Check, Config, Scope, Tier};

/// The tail of a check's output kept in results and notices.
const OUTPUT_TAIL: usize = 4000;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);
/// Variables every check sees (SPEC §10.2 env allowlist), on top of `check.env`.
#[cfg(unix)]
const BASE_ENV: &[&str] = &["PATH", "HOME"];
/// Windows processes need a few more to work at all.
#[cfg(windows)]
const BASE_ENV: &[&str] =
    &["PATH", "PATHEXT", "SystemRoot", "SystemDrive", "COMSPEC", "USERPROFILE", "HOMEDRIVE", "HOMEPATH", "NUMBER_OF_PROCESSORS", "PROCESSOR_ARCHITECTURE"];

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Config(#[from] config::ConfigError),
    #[error(transparent)]
    Store(#[from] kintrel_core::Error),
    #[error(transparent)]
    Db(#[from] rusqlite::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// What to check: a root, plus a session's own writes on top (`None` = deleted).
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub root: Hash,
    pub overlay: BTreeMap<String, Option<(Kind, Hash)>>,
    /// Contents the store does not have yet (a fold's merge results, for bisection).
    pub blobs: HashMap<Hash, Vec<u8>>,
}

impl Snapshot {
    fn blob(&self, st: &State, id: Hash) -> Result<Vec<u8>> {
        match self.blobs.get(&id) {
            Some(b) => Ok(b.clone()),
            None => Ok(st.get_blob(id)?.ok_or_else(|| kintrel_core::Error::Corrupt(format!("missing blob {id}")))?),
        }
    }
}

/// The blob of `kintrel.toml` in `root`, if it has one: the config to pass to [`Ci::run`].
///
/// Callers choose the config, never the snapshot: the daemon uses the promoted root's, so an
/// agent cannot weaken the gate, or have the daemon run a new command, by editing the file
/// (a changed `kintrel.toml` needs a person's approval to be promoted).
pub fn config_of(st: &State, root: Hash) -> Result<Option<Hash>> {
    Ok(st.lookup(root, config::FILE)?.filter(|(k, _)| *k != Kind::Dir).map(|(_, id)| id))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Outcome {
    pub check: String,
    pub tier: Tier,
    /// The file for a file-scope check, else empty.
    pub target: String,
    pub ok: bool,
    pub cached: bool,
    pub timed_out: bool,
    /// The check could not run (no sandbox backend, a bad `writable` entry): not a verdict on
    /// the code, so never cached.
    pub infra_error: bool,
    pub ms: u64,
    /// The tail of stdout and stderr.
    pub output: String,
    pub key: Hash,
    /// The check's input globs (who a failure concerns).
    pub inputs: Vec<String>,
}

/// One check over one target, before it runs.
struct Job<'a> {
    check: &'a Check,
    /// The file (file scope) or package directory (package scope); empty for repo scope.
    target: String,
    /// The package's name, for `{package}`.
    package: String,
    key: Hash,
}

pub struct Ci {
    db: Mutex<Connection>,
    /// Tool-version command → its output, memoized per daemon (SPEC §10.2).
    tools: Mutex<HashMap<String, String>>,
    /// Imports and signatures per (blob, file extension), which never change (SPEC §10.3).
    facts: Mutex<HashMap<(Hash, String), packages::FileFacts>>,
    jobs: usize,
}

impl Ci {
    pub fn open(repo: &Path) -> Result<Ci> {
        let conn = Connection::open(board::data_dir(repo).join("ci.db"))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS results (key BLOB PRIMARY KEY, check_name TEXT, target TEXT,
                 ok INTEGER, output TEXT, ms INTEGER, at INTEGER) WITHOUT ROWID;",
        )?;
        let jobs = std::thread::available_parallelism().map_or(4, |n| n.get());
        Ok(Ci { db: Mutex::new(conn), tools: Mutex::new(HashMap::new()), facts: Mutex::new(HashMap::new()), jobs })
    }

    /// Runs the checks of `config` (a `kintrel.toml` blob, see [`config_of`]) on the snapshot,
    /// only `tier`'s if given, reusing cached results. Without a config there is nothing to run.
    pub fn run(&self, st: &State, repo: &Path, config: Option<Hash>, snap: &Snapshot, tier: Option<Tier>) -> Result<Vec<Outcome>> {
        let out = self.run_where(st, repo, config, snap, &|c: &Check| tier.is_none_or(|t| c.tier == t), false)?;
        Ok(out.expect("runs when not cached-only"))
    }

    /// Like [`Ci::run`] for the checks `keep` selects. With `cached_only`, nothing runs: the
    /// result is `None` unless every selected check is cached (the gate's fast path).
    pub fn run_where(
        &self,
        st: &State,
        repo: &Path,
        config: Option<Hash>,
        snap: &Snapshot,
        keep: &dyn Fn(&Check) -> bool,
        cached_only: bool,
    ) -> Result<Option<Vec<Outcome>>> {
        let Some(config) = config else { return Ok(Some(vec![])) };
        let config = Config::parse(&String::from_utf8_lossy(&snap.blob(st, config)?))?;
        let files = snapshot_files(st, snap)?;

        let graph = config.checks.iter().any(|c| c.scope == Scope::Package).then(|| self.graph(st, snap, &files)).transpose()?;
        let mut jobs = vec![];
        for check in config.checks.iter().filter(|c| keep(c)) {
            let set = globs(&check.inputs);
            let inputs: Vec<(&String, Hash)> = files.iter().filter(|(p, _)| set.is_match(p)).map(|(p, (_, h))| (p, *h)).collect();
            let base = KeyBase { check, tools: self.tool_versions(check, repo), env: env_values(check) };
            match check.scope {
                Scope::File => {
                    // A file check's inputs are its file plus the config files it names literally.
                    let literal: Vec<(&String, Hash)> =
                        inputs.iter().filter(|(p, _)| check.inputs.iter().any(|g| g == *p)).copied().collect();
                    for (path, hash) in inputs.iter().filter(|(p, _)| !check.inputs.iter().any(|g| g == *p)) {
                        let mut own = literal.clone();
                        own.push((path, *hash));
                        jobs.push(Job { check, target: (*path).clone(), package: String::new(), key: base.key(path, &own, &[]) });
                    }
                }
                Scope::Repo => jobs.push(Job { check, target: String::new(), package: String::new(), key: base.key("", &inputs, &[]) }),
                Scope::Package => {
                    let (g, interfaces) = graph.as_ref().expect("built when a check has package scope");
                    let own = |i: usize| -> Vec<(&String, Hash)> {
                        inputs.iter().filter(|(p, _)| g.owner(p) == Some(i)).copied().collect()
                    };
                    // Inputs no package owns (a root Cargo.lock, shared config) concern every
                    // package (SPEC §10.3).
                    let shared: Vec<(&String, Hash)> = inputs.iter().filter(|(p, _)| g.owner(p).is_none()).copied().collect();
                    for (i, pkg) in g.packages.iter().enumerate() {
                        let mut mine = own(i);
                        if mine.is_empty() {
                            continue;
                        }
                        mine.extend(shared.iter().copied());
                        // Early cutoff (SPEC §10.2): lint and type checks see their dependencies'
                        // interfaces only; tests see everything they depend on.
                        let deps: Vec<Hash> = g
                            .closure(i)
                            .into_iter()
                            .map(|d| if check.tier == Tier::Test { encode::hash(&own(d)) } else { interfaces[d] })
                            .collect();
                        let key = base.key(&pkg.dir, &mine, &deps);
                        jobs.push(Job { check, target: pkg.dir.clone(), package: pkg.name.clone(), key });
                    }
                }
            }
        }

        let mut out: Vec<Option<Outcome>> = jobs.iter().map(|j| self.cached(j)).collect::<Result<_>>()?;
        let todo: Vec<usize> = (0..jobs.len()).filter(|i| out[*i].is_none()).collect();
        if cached_only && !todo.is_empty() {
            return Ok(None);
        }
        if !todo.is_empty() {
            let dir = checkout(st, repo, snap)?;
            let next = AtomicUsize::new(0);
            let done = Mutex::new(vec![]);
            std::thread::scope(|s| {
                for _ in 0..self.jobs.min(todo.len()) {
                    s.spawn(|| {
                        while let Some(&i) = todo.get(next.fetch_add(1, Ordering::Relaxed)) {
                            let o = execute(&jobs[i], &dir, repo);
                            done.lock().unwrap().push((i, o));
                        }
                    });
                }
            });
            // ponytail: one fresh checkout per run, shared by its jobs; one per job if checks
            // turn out to interfere through files they write.
            fs::remove_dir_all(&dir).ok();
            for (i, o) in done.into_inner().unwrap() {
                if !o.timed_out && !o.infra_error {
                    self.store(&o)?;
                }
                out[i] = Some(o);
            }
        }
        Ok(Some(out.into_iter().flatten().collect()))
    }

    /// Runs the config's deploy hooks on a promoted root, in order, each at most once per root
    /// (a rerun returns the recorded outcome). Stops at the first failure.
    pub fn deploy(&self, st: &State, repo: &Path, config: Option<Hash>, root: Hash) -> Result<Vec<Outcome>> {
        let Some(config) = config else { return Ok(vec![]) };
        let snap = Snapshot { root, ..Default::default() };
        let config = Config::parse(&String::from_utf8_lossy(&snap.blob(st, config)?))?;
        let checks: Vec<Check> = config
            .deploys
            .iter()
            .map(|d| Check {
                name: format!("deploy:{}", d.name),
                tier: Tier::Test,
                scope: Scope::Repo,
                run: d.run.clone(),
                inputs: vec![],
                env: d.env.clone(),
                tools: vec![],
                gate: false,
                timeout_s: d.timeout_s,
                sandbox: d.sandbox,
                network: d.network,
                writable: d.writable.clone(),
                readable: d.readable.clone(),
            })
            .collect();
        let mut out = vec![];
        let mut dir = None;
        for check in &checks {
            let job = Job { check, target: String::new(), package: String::new(), key: encode::hash(&("deploy", &check.name, &check.run, root)) };
            let o = match self.cached(&job)? {
                Some(o) => o,
                None => {
                    let d = match &dir {
                        Some(d) => d,
                        None => dir.insert(checkout(st, repo, &snap)?),
                    };
                    let o = execute(&job, d, repo);
                    if !o.timed_out && !o.infra_error {
                        self.store(&o)?;
                    }
                    o
                }
            };
            let ok = o.ok;
            out.push(o);
            if !ok {
                break;
            }
        }
        if let Some(d) = dir {
            fs::remove_dir_all(d).ok();
        }
        Ok(out)
    }

    fn cached(&self, job: &Job) -> Result<Option<Outcome>> {
        let db = self.db.lock().unwrap();
        let row = db
            .prepare_cached("SELECT ok, output, ms FROM results WHERE key = ?1")?
            .query_row([&job.key.0[..]], |r| Ok((r.get::<_, bool>(0)?, r.get::<_, String>(1)?, r.get::<_, u64>(2)?)))
            .optional()?;
        Ok(row.map(|(ok, output, ms)| Outcome {
            check: job.check.name.clone(),
            tier: job.check.tier,
            target: job.target.clone(),
            ok,
            cached: true,
            timed_out: false,
            infra_error: false,
            ms,
            output,
            key: job.key,
            inputs: job.check.inputs.clone(),
        }))
    }

    fn store(&self, o: &Outcome) -> Result<()> {
        let at = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64;
        self.db.lock().unwrap().execute(
            "INSERT OR REPLACE INTO results (key, check_name, target, ok, output, ms, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![&o.key.0[..], o.check, o.target, o.ok, o.output, o.ms, at],
        )?;
        Ok(())
    }

    /// The package graph of a root (for `kintrel packages`).
    pub fn packages(&self, st: &State, root: Hash) -> Result<packages::Graph> {
        let snap = Snapshot { root, ..Default::default() };
        let files = snapshot_files(st, &snap)?;
        Ok(self.graph(st, &snap, &files)?.0)
    }

    /// The snapshot's package graph, and each package's interface hash: its atoms' signatures.
    fn graph(&self, st: &State, snap: &Snapshot, files: &BTreeMap<String, (Kind, Hash)>) -> Result<(packages::Graph, Vec<Hash>)> {
        let facts = |path: &str, id: Hash| -> packages::FileFacts {
            let ext = path.rsplit_once('.').map_or("", |(_, e)| e).to_owned();
            if let Some(f) = self.facts.lock().unwrap().get(&(id, ext.clone())) {
                return f.clone();
            }
            let f = packages::FileFacts::of(path, &snap.blob(st, id).unwrap_or_default());
            self.facts.lock().unwrap().insert((id, ext), f.clone());
            f
        };
        let text = |id: Hash| String::from_utf8_lossy(&snap.blob(st, id).unwrap_or_default()).into_owned();
        let g = packages::Graph::build(files, facts, text);
        let mut sigs: Vec<Vec<(String, String)>> = vec![vec![]; g.packages.len()];
        for (path, (_, id)) in files.iter().filter(|(p, _)| kintrel_lang::lang_for_path(p).is_some()) {
            if let Some(i) = g.owner(path) {
                sigs[i].extend(facts(path, *id).sigs);
            }
        }
        let interfaces = sigs.iter_mut().map(|s| {
            s.sort();
            encode::hash(s)
        });
        Ok((g, interfaces.collect()))
    }

    fn tool_versions(&self, check: &Check, repo: &Path) -> Vec<String> {
        let mut memo = self.tools.lock().unwrap();
        check
            .tools
            .iter()
            .map(|cmd| {
                memo.entry(cmd.clone())
                    .or_insert_with(|| match launch(cmd, check, None, repo) {
                        Ok(mut run) => {
                            let mut out = vec![];
                            run.output.read_to_end(&mut out).ok();
                            run.wait(Duration::from_secs(60));
                            String::from_utf8_lossy(&out).into_owned()
                        }
                        Err(e) => format!("error: {e}"),
                    })
                    .clone()
            })
            .collect()
    }
}

/// Everything a cache key covers (SPEC §10.2).
struct KeyBase<'a> {
    check: &'a Check,
    tools: Vec<String>,
    env: Vec<(String, Option<String>)>,
}

impl KeyBase<'_> {
    fn key(&self, target: &str, inputs: &[(&String, Hash)], deps: &[Hash]) -> Hash {
        #[derive(Serialize)]
        struct Key<'a> {
            name: &'a str,
            run: &'a str,
            scope: Scope,
            tools: &'a [String],
            env: &'a [(String, Option<String>)],
            /// The sandbox policy changes what a check can do, so it changes the result.
            sandbox: (bool, bool, &'a [String], &'a [String]),
            target: &'a str,
            inputs: &'a [(&'a String, Hash)],
            deps: &'a [Hash],
        }
        let c = self.check;
        let sandbox = (c.sandbox, c.network, &c.writable[..], &c.readable[..]);
        encode::hash(&Key { name: &c.name, run: &c.run, scope: c.scope, tools: &self.tools, env: &self.env, sandbox, target, inputs, deps })
    }
}

fn globs(patterns: &[String]) -> GlobSet {
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        b.add(Glob::new(p).expect("validated by Config::parse"));
    }
    b.build().expect("validated by Config::parse")
}

fn env_values(check: &Check) -> Vec<(String, Option<String>)> {
    let mut names: Vec<&String> = check.env.iter().collect();
    names.sort();
    names.into_iter().map(|n| (n.clone(), std::env::var(n).ok())).collect()
}

/// Starts `cmd` for `check`: in the sandbox unless the check opts out, with the environment
/// cleared down to the allowlist plus `check.env`. `dir` is the checkout, writable, with a
/// private temp directory inside it (the sandbox lets checks use Unix sockets there).
fn launch(cmd: &str, check: &Check, dir: Option<&Path>, repo: &Path) -> std::result::Result<sandbox::Running, String> {
    let mut env: Vec<(String, std::ffi::OsString)> = BASE_ENV
        .iter()
        .map(|n| n.to_string())
        .chain(check.env.iter().cloned())
        .filter_map(|n| std::env::var_os(&n).map(|v| (n, v)))
        .collect();
    if let Some(dir) = dir {
        let tmp = dir.join(".kintrel-tmp");
        std::fs::create_dir_all(&tmp).map_err(|e| format!("temp dir: {e}"))?;
        for name in ["TMPDIR", "TEMP", "TMP"] {
            env.push((name.into(), tmp.clone().into_os_string()));
        }
    }
    let policy = if check.sandbox {
        let paths = |list: &[String]| list.iter().map(|w| expand_path(w, &check.env, dir)).collect::<std::result::Result<Vec<_>, _>>();
        // Kintrel's runtime directory (the daemon's key, every view and checkout) and the
        // board's own data are off limits to every grant.
        let mut protected = vec![board::data_dir(repo)];
        protected.extend(kintrel_core::os::cache_dir().map(|c| c.join("kintrel")));
        Some(sandbox::Policy {
            network: check.network,
            writable: paths(&check.writable)?,
            readable: paths(&check.readable)?,
            protected,
            hidden: std::iter::once(board::data_dir(repo)).chain(board::keys_dir().ok()).collect(),
        })
    } else {
        None
    };
    sandbox::start(cmd, dir, policy.as_ref(), &env)
}

/// `cmd` with its stderr merged into stdout, in the platform shell's syntax.
fn merged(cmd: &str) -> String {
    if cfg!(windows) { format!("({cmd}) 2>&1") } else { format!("exec 2>&1; {cmd}") }
}

/// A `writable` entry as a path: `~/x`, `$VAR/x` (VAR in `env`, or HOME), absolute, or relative
/// to the checkout.
fn expand_path(entry: &str, env: &[String], dir: Option<&Path>) -> std::result::Result<PathBuf, String> {
    let var = |name: &str| -> std::result::Result<PathBuf, String> {
        if name == "HOME" {
            return kintrel_core::os::home_dir().ok_or_else(|| format!("{entry:?}: no home directory"));
        }
        if !env.iter().any(|e| e == name) {
            return Err(format!("{entry:?}: ${name} must be listed in `env`"));
        }
        std::env::var_os(name).map(PathBuf::from).ok_or_else(|| format!("{entry:?}: ${name} is not set"))
    };
    let (base, rest) = if let Some(rest) = entry.strip_prefix("~/") {
        (var("HOME")?, rest)
    } else if let Some(v) = entry.strip_prefix('$') {
        let (name, rest) = v.split_once('/').unwrap_or((v, ""));
        (var(name)?, rest)
    } else if Path::new(entry).is_absolute() {
        return Ok(PathBuf::from(entry));
    } else {
        (dir.ok_or_else(|| format!("writable {entry:?}: relative, but there is no checkout"))?.to_owned(), entry)
    };
    Ok(if rest.is_empty() { base } else { base.join(rest) })
}

/// Replaces `{name}` placeholders with shell-quoted values in one pass over the template, so a
/// value (a file or package name an agent chose) is never scanned for placeholders itself.
fn expand(template: &str, values: &[(&str, &str)]) -> std::result::Result<String, String> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        match values.iter().find(|(name, _)| rest[1..].starts_with(name) && rest[1 + name.len()..].starts_with('}')) {
            Some((name, value)) => {
                out.push_str(&quote(value)?);
                rest = &rest[name.len() + 2..];
            }
            None => {
                out.push('{');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// One shell word holding `s` exactly. Unix: single quotes, which nothing inside escapes. cmd
/// has no such quoting: `%VAR%` expands even inside double quotes, and `"` ends them, so a value
/// with either (or a control character) is refused and the check fails, never runs unquoted.
fn quote(s: &str) -> std::result::Result<String, String> {
    if cfg!(windows) {
        if s.chars().any(|c| c == '"' || c == '%' || c.is_control()) {
            return Err(format!("{s:?} cannot be passed to cmd.exe safely (contains \", % or a control character)"));
        }
        Ok(format!("\"{s}\""))
    } else {
        Ok(format!("'{}'", s.replace('\'', r"'\''")))
    }
}

fn execute(job: &Job, dir: &Path, repo: &Path) -> Outcome {
    let package_dir = if job.target.is_empty() { "." } else { &job.target };
    let root = dir.to_string_lossy();
    let cmd = expand(&job.check.run, &[("file", &job.target), ("package_dir", package_dir), ("package", &job.package), ("root", &root)]);
    let start = Instant::now();
    let cmd = match cmd {
        Ok(cmd) => cmd,
        Err(why) => {
            return Outcome {
                check: job.check.name.clone(),
                tier: job.check.tier,
                target: job.target.clone(),
                ok: false,
                cached: false,
                timed_out: false,
                infra_error: false,
                ms: 0,
                output: why,
                key: job.key,
                inputs: job.check.inputs.clone(),
            };
        }
    };
    let timeout = job.check.timeout_s.map_or(DEFAULT_TIMEOUT, Duration::from_secs);
    let mut timed_out = false;
    let mut infra_error = false;
    let (ok, output) = match launch(&merged(&cmd), job.check, Some(dir), repo) {
        Err(e) => {
            infra_error = true;
            (false, e)
        }
        Ok(mut run) => {
            // Drain output on a thread so a chatty check cannot block on a full pipe.
            let mut pipe = std::mem::replace(&mut run.output, Box::new(std::io::empty()));
            let reader = std::thread::spawn(move || {
                let mut buf = vec![];
                pipe.read_to_end(&mut buf).ok();
                buf
            });
            let status = run.wait(timeout);
            timed_out = status.is_none();
            let buf = reader.join().unwrap_or_default();
            let from = buf.len().saturating_sub(OUTPUT_TAIL);
            let mut output = String::from_utf8_lossy(&buf[from..]).into_owned();
            if timed_out {
                output.push_str(&format!("\n[timed out after {}s]", timeout.as_secs()));
            }
            (status == Some(true), output)
        }
    };
    Outcome {
        check: job.check.name.clone(),
        tier: job.check.tier,
        target: job.target.clone(),
        ok,
        cached: false,
        timed_out,
        infra_error,
        ms: start.elapsed().as_millis() as u64,
        output,
        key: job.key,
        inputs: job.check.inputs.clone(),
    }
}


/// The snapshot's files: the root's, with the overlay applied.
fn snapshot_files(st: &State, snap: &Snapshot) -> Result<BTreeMap<String, (Kind, Hash)>> {
    let mut files: BTreeMap<String, (Kind, Hash)> =
        worktree::files(st, snap.root)?.into_iter().map(|(p, k, h)| (p, (k, h))).collect();
    for (path, entry) in &snap.overlay {
        match entry {
            Some(e) => files.insert(path.clone(), *e),
            None => files.remove(path),
        };
    }
    Ok(files)
}

/// A fresh checkout of the snapshot: a clone of the root's shared base checkout, plus the
/// overlay written on top. Checks may write into it freely; it is removed after the run.
fn checkout(st: &State, repo: &Path, snap: &Snapshot) -> Result<PathBuf> {
    let base = worktree::base_checkout(st, repo, snap.root)?;
    static N: AtomicUsize = AtomicUsize::new(0);
    let dir = board::runtime_dir(repo)?.join("checks").join(format!("{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    fs::create_dir_all(dir.parent().expect("has parent"))?;
    fs::remove_dir_all(&dir).ok();
    worktree::clone_tree(&base, &dir)?;
    for (path, entry) in &snap.overlay {
        let file = dir.join(path);
        fs::remove_file(&file).ok();
        let Some((kind, id)) = entry else { continue };
        fs::create_dir_all(file.parent().expect("has parent"))?;
        let bytes = snap.blob(st, *id)?;
        match kind {
            Kind::Symlink => kintrel_core::os::write_symlink(&bytes, &file)?,
            _ => {
                fs::write(&file, &bytes)?;
                kintrel_core::os::set_executable(&file, *kind == Kind::Exec)?;
            }
        }
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_expand_once() {
        let v = [("file", "a{package}.py"), ("package", "x'; touch pwned; '")];
        let cmd = expand("check {file} {package} {other}", &v).unwrap();
        if cfg!(windows) {
            assert_eq!(cmd, r#"check "a{package}.py" "x'; touch pwned; '" {other}"#);
            assert!(expand("x {file}", &[("file", "%COMSPEC%")]).is_err(), "cmd would expand it");
        } else {
            assert_eq!(cmd, r#"check 'a{package}.py' 'x'\''; touch pwned; '\''' {other}"#);
        }
    }
}
