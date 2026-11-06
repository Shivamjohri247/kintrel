use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand, ValueEnum};
use kintrel::daemon::{self, Options};
use kintrel::connect;
use kintrel::rpc::Client;
use kintrel::view::{self, Marker};
use kintrel_core::log::Durability;
use kintrel_core::{Hash, proto};

#[derive(Parser)]
#[command(name = "kintrel", version, about = "Version control for AI agents")]
struct Cli {
    /// Board directory (the repository root).
    #[arg(long, global = true, default_value = ".")]
    repo: PathBuf,
    /// Machine-readable output.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a board, optionally from a Git repository's HEAD.
    Init {
        #[arg(long)]
        from_git: Option<PathBuf>,
    },
    Daemon {
        #[command(subcommand)]
        cmd: DaemonCmd,
    },
    Session {
        #[command(subcommand)]
        cmd: SessionCmd,
    },
    /// Agent working directories.
    View {
        #[command(subcommand)]
        cmd: ViewCmd,
    },
    /// Post changed files from the view containing the current directory (all changes if no paths).
    Post {
        paths: Vec<PathBuf>,
        #[arg(long)]
        view: Option<PathBuf>,
    },
    /// Claude Code hook: reads the hook JSON on stdin, posts what the tool changed and passes
    /// unread notices to the agent.
    Hook,
    /// Announce a planned change to atoms; agents working on them are told (advisory).
    Intent {
        #[arg(short, long)]
        message: String,
        #[arg(required = true)]
        atoms: Vec<String>,
        #[arg(long)]
        view: Option<PathBuf>,
    },
    /// Claim atoms you are working on (advisory, leased; never blocks anyone).
    Claim {
        #[arg(short, long)]
        message: String,
        #[arg(required = true)]
        atoms: Vec<String>,
        /// Lease in milliseconds (default 10 minutes, renewed by activity).
        #[arg(long)]
        lease_ms: Option<u64>,
        #[arg(long)]
        view: Option<PathBuf>,
    },
    /// Undo one note's changes (later work stays), then refresh the view.
    Revert {
        /// Note id, full or 8+ hex digits.
        note: String,
        #[arg(long)]
        view: Option<PathBuf>,
    },
    /// End one of your claims or intents.
    Release {
        id: u64,
        #[arg(long)]
        view: Option<PathBuf>,
    },
    /// Run the `kintrel.toml` checks: on your view inside one (after posting its changes), else
    /// on head or `--at <root>`. Exits non-zero if any check fails.
    Checks {
        #[arg(long)]
        tier: Option<String>,
        #[arg(long)]
        at: Option<String>,
        #[arg(long)]
        view: Option<PathBuf>,
    },
    /// List packages and the dependencies Kintrel infers between them (at head, or `--at`).
    Packages {
        #[arg(long)]
        at: Option<String>,
    },
    /// Take this view's unread notices (other agents' changes to code it read or wrote).
    Inbox {
        #[arg(long)]
        view: Option<PathBuf>,
        /// Keep printing notices as they arrive.
        #[arg(long)]
        follow: bool,
    },
    /// Print a file: from your view inside one, else from head (or `--at <root>`).
    Read {
        path: String,
        #[arg(long)]
        at: Option<String>,
    },
    /// List a file's atoms.
    Symbols { path: String },
    /// List open conflicts.
    Conflicts,
    /// MCP server on stdio for agents that use tools instead of files (one session per process).
    Mcp {
        /// Agent identity recorded on every note.
        #[arg(long, default_value = "mcp")]
        agent: String,
    },
    Status,
    /// Promote head now if every changed file parses and the gate checks pass.
    Promote {
        /// Review head's changed kintrel.toml and approve it (needs an interactive terminal):
        /// check definitions are a person's call, never an agent's.
        #[arg(long)]
        approve_config: bool,
    },
    /// Run an agent in the current view, sandboxed so it cannot act as you: it writes only its
    /// view, temp and its own Claude Code config, and cannot read the person key that approving
    /// kintrel.toml needs. Example: `kintrel agent -- claude`.
    Agent {
        /// More directories the agent may write.
        #[arg(long)]
        writable: Vec<PathBuf>,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<std::ffi::OsString>,
    },
    /// Bring new Git commits into the board: each becomes notes, merged into head like an
    /// agent's edits (Kintrel's own exports are skipped).
    Sync {
        /// The branch to follow (default: the branch Git's HEAD is on).
        #[arg(long)]
        branch: Option<String>,
        /// Start after this commit (a full id) instead of the last sync.
        #[arg(long)]
        since: Option<String>,
    },
    /// Commit the promoted root to a Git branch in the repository.
    Export {
        #[arg(long, default_value = kintrel::git::DEFAULT_BRANCH)]
        branch: String,
    },
}

#[derive(Subcommand)]
enum DaemonCmd {
    /// Run the daemon in the foreground.
    Run {
        #[arg(long, value_enum, default_value_t = DurabilityArg::Normal)]
        durability: DurabilityArg,
        /// Promote head at most this often (0: only on `kintrel promote`).
        #[arg(long, default_value_t = 1000)]
        promote_every_ms: u64,
    },
}

#[derive(Subcommand)]
enum SessionCmd {
    /// Open a session and print its token.
    Start {
        #[arg(long)]
        agent: String,
    },
}

#[derive(Subcommand)]
enum ViewCmd {
    /// Start a session and check out a view for it; prints the view's path.
    Create {
        #[arg(long)]
        agent: String,
    },
    /// Post local changes, then move the view to the current head.
    Refresh {
        #[arg(long)]
        view: Option<PathBuf>,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum DurabilityArg {
    Normal,
    Full,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Daemon { cmd: DaemonCmd::Run { durability, promote_every_ms } } => {
            let durability = match durability {
                DurabilityArg::Normal => Durability::Normal,
                DurabilityArg::Full => Durability::Full,
            };
            let promote_every = (promote_every_ms > 0).then(|| Duration::from_millis(promote_every_ms));
            exit_when_board_goes(&cli.repo)?;
            daemon::run(Options { repo: cli.repo, durability, promote_every }).await?;
        }
        Cmd::Agent { writable, command } => {
            // Only in a view: in the repository the agent could write `.git/hooks`, which the
            // person's Git would run.
            let (view, marker) = view::find(&std::env::current_dir()?).context("run `kintrel agent` inside a view (`kintrel view create`)")?;
            let repo = marker.repo;
            // The marker is the agent's to write, so its repository must match where the view
            // lives: views sit under their board's runtime directory, which no agent writes.
            let runtime = kintrel_core::board::runtime_dir(&repo)?;
            if !std::fs::canonicalize(&view)?.starts_with(std::fs::canonicalize(runtime.join("views"))?) {
                bail!("{} is not a view of {}", view.display(), repo.display());
            }
            // Start the daemon outside the sandbox: it writes the board and creates the person key.
            connect(&repo).await?;
            let config = runtime.join("agent-claude");
            if !config.exists() {
                kintrel_core::os::create_private_dir(&config)?;
                // Seeded with the person's settings (the Kintrel hooks); the agent can change only
                // its copy, which the person's own unsandboxed Claude Code never reads.
                if let Some(settings) = kintrel_core::os::home_dir().map(|h| h.join(".claude/settings.json")).filter(|p| p.exists()) {
                    std::fs::copy(settings, config.join("settings.json"))?;
                }
            }
            let policy = kintrel_ci::sandbox::AgentPolicy {
                view: &view,
                ours: std::slice::from_ref(&config),
                extra: &writable,
                board: &kintrel_core::board::data_dir(&repo),
                keys: &kintrel_core::board::keys_dir()?,
                socket: &daemon::socket_path(&repo)?,
            };
            let mut agent = kintrel_ci::sandbox::agent(&command, &policy).map_err(anyhow::Error::msg)?;
            if std::env::var_os("CLAUDE_CONFIG_DIR").is_none() {
                agent.env("CLAUDE_CONFIG_DIR", &config);
            }
            let status = agent.current_dir(std::env::current_dir()?).status().context("could not start the agent")?;
            std::process::exit(status.code().unwrap_or(1));
        }
        Cmd::Promote { approve_config } => {
            let (approve_config, approval) = match approve_config {
                true => {
                    let (trusted, theirs) = review_config(&cli.repo)?;
                    let key = std::fs::read_to_string(kintrel_core::board::person_key(&cli.repo)?)
                        .context("cannot read the person key; approving is a person's job, not an agent's")?;
                    (Some(theirs), Some(kintrel_core::board::approval(&key, trusted, theirs)))
                }
                false => (None, None),
            };
            let promote = proto::PromoteNow { approve_config, approval };
            let s: proto::StatusInfo = connect(&cli.repo).await?.call(proto::PROMOTE_NOW, &promote).await?;
            if !s.blocked.is_empty() {
                bail!("promotion blocked: {} do not parse", s.blocked.join(", "));
            }
            if !s.failing.is_empty() {
                bail!("promotion blocked: gate checks failing: {} (see `kintrel checks`)", s.failing.join(", "));
            }
            println!("promoted {}", s.promoted.map_or("none".into(), |h| h.short()));
        }
        Cmd::Session { cmd: SessionCmd::Start { agent } } => {
            let started = start_session(&cli.repo, agent).await?;
            if cli.json {
                println!("{}", serde_json::to_string(&started)?);
            } else {
                println!("session {} (rank {})\nexport KINTREL_SESSION={}", started.session, started.rank, started.token);
            }
        }
        Cmd::View { cmd: ViewCmd::Create { agent } } => {
            let started = start_session(&cli.repo, agent).await?;
            let marker = Marker {
                repo: std::fs::canonicalize(&cli.repo)?,
                session: started.session,
                token: started.token,
                base_root: started.base_root,
            };
            let path = view::materialize(&marker)?;
            if cli.json {
                println!("{}", serde_json::json!({ "view": path, "session": marker.session, "rank": started.rank }));
            } else {
                println!("{}", path.display());
            }
        }
        Cmd::View { cmd: ViewCmd::Refresh { view } } => {
            let (dir, mut marker) = view::find(&view.unwrap_or(std::env::current_dir()?))?;
            if let Some(p) = post_view(&dir, None).await? {
                println!("posted {} ({} atoms)", p.note_id.short(), p.atoms.len());
            }
            let r: proto::Refreshed =
                connect(&marker.repo).await?.call(proto::SESSION_REFRESH, &proto::Refresh { token: marker.token.clone() }).await?;
            marker.base_root = r.base_root;
            view::update(&dir, &marker)?;
            println!("view now at {}", r.base_root.short());
            for n in &r.lost {
                println!("conflicted: note {} is not in head (see `kintrel conflicts`)", n.short());
            }
        }
        Cmd::Post { paths, view } => {
            let start = match (&view, paths.first()) {
                (Some(v), _) => v.clone(),
                (None, Some(p)) => p.parent().filter(|d| !d.as_os_str().is_empty()).map_or(std::env::current_dir()?, Path::to_owned),
                (None, None) => std::env::current_dir()?,
            };
            let (dir, _) = view::find(&start)?;
            let rel = (!paths.is_empty())
                .then(|| paths.iter().map(|p| view::rel_path(&dir, p)).collect::<anyhow::Result<Vec<_>>>())
                .transpose()?;
            match post_view(&dir, rel.as_deref()).await? {
                Some(p) if cli.json => println!("{}", serde_json::to_string(&p)?),
                Some(p) => println!("posted {} (gseq {}): {}", p.note_id.short(), p.gseq, p.atoms.join(", ")),
                None => println!("nothing to post"),
            }
        }
        Cmd::Mcp { agent } => kintrel::mcp::serve(std::fs::canonicalize(&cli.repo)?, agent).await?,
        Cmd::Hook => {
            if let Err(e) = hook().await {
                // Exit code 2 shows the message to the agent (Claude Code hook protocol).
                eprintln!("kintrel: {e:#}");
                std::process::exit(2);
            }
        }
        Cmd::Read { path, at } => {
            let (repo, token, at) = match (view::find(&std::env::current_dir()?), at) {
                (Ok((_, m)), None) => (m.repo, m.token, None),
                (_, at) => {
                    let mut c = connect(&cli.repo).await?;
                    let at = resolve_root(&mut c, at.as_deref().unwrap_or("head")).await?;
                    (cli.repo.clone(), String::new(), Some(at))
                }
            };
            let f: proto::FileContent =
                connect(&repo).await?.call(proto::READ_FILE, &proto::ReadFile { token, path, at }).await?;
            print!("{}", f.content);
        }
        Cmd::Symbols { path } => {
            let mut c = connect(&cli.repo).await?;
            let head = resolve_root(&mut c, "head").await?;
            let f: proto::FileContent =
                c.call(proto::READ_FILE, &proto::ReadFile { token: String::new(), path: path.clone(), at: Some(head) }).await?;
            let bytes = f.content.as_bytes();
            for a in kintrel_lang::segment(&path, bytes).atoms() {
                let key = kintrel_lang::AtomKey { path: path.clone(), symbol: a.symbol };
                let line = a.ranges.first().map_or(1, |r| 1 + bytes[..r.start as usize].iter().filter(|&&b| b == b'\n').count());
                println!("{line:>6}  {key}");
            }
        }
        Cmd::Conflicts => {
            let st = kintrel_core::state::State::open_with_log(
                &kintrel_core::board::state_db(&cli.repo),
                &kintrel_core::board::log_db(&cli.repo),
            )?;
            let conflicts = st.open_conflicts()?;
            if cli.json {
                println!("{}", serde_json::to_string(&conflicts)?);
            } else if conflicts.is_empty() {
                println!("no open conflicts");
            } else {
                for c in conflicts {
                    let kept = c.kept.map_or("-".into(), |k| k.short());
                    println!("{}  losing {}  kept {}", c.atom, c.losing.short(), kept);
                }
            }
        }
        Cmd::Intent { message, atoms, view } => announce(proto::INTENT, view, message, atoms, None, cli.json).await?,
        Cmd::Claim { message, atoms, lease_ms, view } => announce(proto::CLAIM, view, message, atoms, lease_ms, cli.json).await?,
        Cmd::Revert { note, view } => {
            let (dir, mut marker) = view::find(&view.map_or_else(std::env::current_dir, Ok)?)?;
            post_view(&dir, None).await?; // the revert merges against the view as the daemon knows it
            let mut c = connect(&marker.repo).await?;
            let p: proto::Posted = c.call(proto::REVERT, &proto::Revert { token: marker.token.clone(), note }).await?;
            println!("posted {} reverting: {}", p.note_id.short(), p.atoms.join(", "));
            let r: proto::Refreshed = c.call(proto::SESSION_REFRESH, &proto::Refresh { token: marker.token.clone() }).await?;
            marker.base_root = r.base_root;
            view::update(&dir, &marker)?;
            println!("view now at {}", r.base_root.short());
        }
        Cmd::Release { id, view } => {
            let (_, marker) = view::find(&view.map_or_else(std::env::current_dir, Ok)?)?;
            let _: proto::Status = connect(&marker.repo).await?.call(proto::RELEASE, &proto::Release { token: marker.token, id }).await?;
            println!("released {id}");
        }
        Cmd::Checks { tier, at, view } => {
            let dir = view.map_or_else(std::env::current_dir, Ok)?;
            let (repo, token) = match view::find(&dir) {
                Ok((d, m)) if at.is_none() => {
                    post_view(&d, None).await?;
                    (m.repo.clone(), Some(m.token))
                }
                _ => (cli.repo.clone(), None),
            };
            let at = at.map(|a| a.parse::<Hash>()).transpose().map_err(|_| anyhow::anyhow!("--at takes a full root hash"))?;
            let r: proto::ChecksRan = connect(&repo).await?.call(proto::CHECKS_RUN, &proto::ChecksRun { token, at, tier }).await?;
            if cli.json {
                println!("{}", serde_json::to_string(&r)?);
            } else {
                for o in &r.outcomes {
                    let how = if o.cached { "cached".to_owned() } else { format!("{} ms", o.ms) };
                    println!("{} {} {} ({how})", if o.ok { "PASS" } else { "FAIL" }, o.check, o.target);
                    if !o.ok {
                        for line in o.output.lines().rev().take(20).collect::<Vec<_>>().into_iter().rev() {
                            println!("    {line}");
                        }
                    }
                }
                if r.outcomes.is_empty() {
                    println!("no checks (add a kintrel.toml)");
                }
            }
            if r.outcomes.iter().any(|o| !o.ok) {
                std::process::exit(1);
            }
        }
        Cmd::Packages { at } => {
            let st = kintrel_core::state::State::open_with_log(&kintrel_core::board::state_db(&cli.repo), &kintrel_core::board::log_db(&cli.repo))?;
            let root = match at {
                Some(a) => a.parse::<Hash>().map_err(|_| anyhow::anyhow!("--at takes a full root hash"))?,
                None => st.get_ref(kintrel_core::state::HEAD)?.context("no head")?,
            };
            let g = kintrel_ci::Ci::open(&cli.repo)?.packages(&st, root)?;
            for p in &g.packages {
                let deps: Vec<&str> = p.deps.iter().map(|d| g.packages[*d].name.as_str()).collect();
                let dir = if p.dir.is_empty() { "." } else { &p.dir };
                println!("{:<24} {:<28} -> {}", p.name, dir, if deps.is_empty() { "-".into() } else { deps.join(", ") });
            }
        }
        Cmd::Inbox { view, follow: true } => {
            // ponytail: polls every 500 ms; a `notices.subscribe` stream if latency matters.
            let (_, marker) = view::find(&view.map_or_else(std::env::current_dir, Ok)?)?;
            loop {
                for n in inbox(&marker).await? {
                    println!("{}", serde_json::to_string(&n)?);
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
        Cmd::Inbox { view, follow: false } => {
            let dir = view.map_or_else(std::env::current_dir, Ok)?;
            let (_, marker) = view::find(&dir)?;
            let n = inbox(&marker).await?;
            if cli.json {
                println!("{}", serde_json::to_string(&n)?);
            } else if n.is_empty() {
                println!("no notices");
            } else {
                println!("{}", kintrel::notices::render(&n));
            }
        }
        Cmd::Status => {
            let s: proto::StatusInfo = connect(&cli.repo).await?.call(proto::STATUS, &proto::Status {}).await?;
            if cli.json {
                println!("{}", serde_json::to_string(&s)?);
            } else {
                let root = |r: Option<Hash>| r.map_or("none".into(), |h| h.short());
                println!(
                    "sessions  {}\nnotes     {} ({} folded, last pass {} ms)\nhead      {}\npromoted  {}\nconflicts {}",
                    s.sessions, s.notes, s.folded, s.pass_ms, root(s.head), root(s.promoted), s.conflicts
                );
                if !s.blocked.is_empty() {
                    println!("promotion blocked: {} do not parse", s.blocked.join(", "));
                }
                if !s.failing.is_empty() {
                    println!("promotion blocked: gate checks failing: {}", s.failing.join(", "));
                }
            }
        }
        Cmd::Init { from_git } => {
            let root = match from_git {
                None => kintrel_core::board::init(&cli.repo)?,
                Some(git) => kintrel_core::board::init_with(&cli.repo, |st| {
                    kintrel::git::import_head(st, &git).map_err(|e| kintrel_core::Error::Invalid(format!("{e:#}")))
                })?,
            };
            exclude_from_git(&cli.repo)?;
            if cli.json {
                println!("{}", serde_json::json!({ "root": root }));
            } else {
                println!("initialized board at {} (root {})", cli.repo.display(), root.short());
            }
        }
        Cmd::Sync { branch, since } => {
            let s: proto::Synced = connect(&cli.repo).await?.call(proto::SYNC, &proto::Sync { branch, since }).await?;
            if cli.json {
                println!("{}", serde_json::to_string(&s)?);
            } else if s.commits.is_empty() {
                println!("{} is up to date", s.branch);
            } else {
                for c in &s.commits {
                    let lost = if c.lost.is_empty() { String::new() } else { format!(" ({} conflicted: see `kintrel conflicts`)", c.lost.len()) };
                    let n = c.notes.len();
                    println!("{} {} -> {n} note{}{lost}", &c.commit[..12], c.summary, if n == 1 { "" } else { "s" });
                }
            }
        }
        Cmd::Export { branch } => {
            let st = kintrel_core::state::State::open_with_log(
                &kintrel_core::board::state_db(&cli.repo),
                &kintrel_core::board::log_db(&cli.repo),
            )?;
            let root = st.get_ref(kintrel_core::state::PROMOTED)?.context("no promoted root")?;
            match kintrel::git::export(&st, &cli.repo, root, &branch)? {
                Some(commit) => println!("exported {} as {commit} on {branch}", root.short()),
                None => println!("{branch} already has {}", root.short()),
            }
        }
    }
    Ok(())
}

async fn start_session(repo: &Path, agent: String) -> anyhow::Result<proto::SessionStarted> {
    let start = proto::SessionStart { agent, rank_hint: None, base: None };
    Ok(connect(repo).await?.call(proto::SESSION_START, &start).await?)
}

/// Keeps `.kintrel/` out of `git status` via `.git/info/exclude` (local, never committed).
fn exclude_from_git(repo: &Path) -> anyhow::Result<()> {
    if !repo.join(".git").is_dir() {
        return Ok(());
    }
    let info = repo.join(".git/info");
    let exclude = info.join("exclude");
    let current = std::fs::read_to_string(&exclude).unwrap_or_default();
    if !current.lines().any(|l| l.trim() == ".kintrel/") {
        std::fs::create_dir_all(&info)?;
        let sep = if current.is_empty() || current.ends_with('\n') { "" } else { "\n" };
        std::fs::write(&exclude, format!("{current}{sep}.kintrel/\n"))?;
    }
    Ok(())
}

/// `head`, `promoted`, or a full root hash.
async fn resolve_root(c: &mut Client, name: &str) -> anyhow::Result<Hash> {
    let s: proto::StatusInfo = c.call(proto::STATUS, &proto::Status {}).await?;
    match name {
        "head" => s.head.context("no head"),
        "promoted" => s.promoted.context("no promoted root"),
        h => h.parse().map_err(|_| anyhow::anyhow!("--at takes head, promoted or a 64-character root hash")),
    }
}

/// Posts the view's changed files (only `paths` if given). `None` if nothing changed.
async fn post_view(dir: &Path, paths: Option<&[String]>) -> anyhow::Result<Option<proto::Posted>> {
    let (_, marker) = view::find(dir)?;
    let changes = view::changes(dir, paths)?;
    if changes.is_empty() {
        return Ok(None);
    }
    let files = changes
        .iter()
        .map(|c| proto::FileWrite { path: c.path.clone(), content: c.content.clone(), base: Some(c.base) })
        .collect();
    let post = proto::Post { token: marker.token.clone(), files, summary: None };
    let posted: proto::Posted = connect(&marker.repo).await?.call(proto::POST, &post).await?;
    view::mark_posted(dir, &changes)?;
    Ok(Some(posted))
}

async fn announce(
    method: &str,
    view: Option<PathBuf>,
    summary: String,
    targets: Vec<String>,
    lease_ms: Option<u64>,
    json: bool,
) -> anyhow::Result<()> {
    let (_, marker) = view::find(&view.map_or_else(std::env::current_dir, Ok)?)?;
    let req = proto::Announce { token: marker.token.clone(), summary, targets, lease_ms };
    let a: proto::Announced = connect(&marker.repo).await?.call(method, &req).await?;
    if json {
        println!("{}", serde_json::to_string(&a)?);
        return Ok(());
    }
    println!("{method} {}", a.id);
    for o in a.overlapping {
        println!("overlaps {} {} by {}: {} ({})", o.kind, o.id, o.agent, o.summary, o.targets.join(", "));
    }
    Ok(())
}

/// Exits the daemon process once the board is deleted or re-created: a daemon serving files
/// that are gone would answer for a new board at the same path with the old one's state.
/// (Not in `daemon::run`: tests run daemons in-process and delete their boards.)
fn exit_when_board_goes(repo: &Path) -> anyhow::Result<()> {
    let db = kintrel_core::board::state_db(repo);
    let ident = |m: std::fs::Metadata| kintrel_core::os::file_identity(&m);
    let opened = std::fs::metadata(&db).map(ident).with_context(|| format!("{} is not a kintrel board", repo.display()))?;
    std::thread::Builder::new().name("kintrel-watchdog".into()).spawn(move || loop {
        std::thread::sleep(Duration::from_secs(2));
        if std::fs::metadata(&db).map(ident).ok() != Some(opened) {
            eprintln!("kintrel: {} is gone or was replaced; exiting", db.display());
            std::process::exit(0);
        }
    })?;
    Ok(())
}

/// Shows head's kintrel.toml next to the promoted one and asks the person at the terminal to
/// approve it; returns the promoted config's and the reviewed blob's hashes. Agents run commands
/// without a terminal, so they cannot answer; agents run through `kintrel agent` cannot read the
/// person key either.
fn review_config(repo: &Path) -> anyhow::Result<(Option<Hash>, Hash)> {
    use std::io::{BufRead, IsTerminal, Write};
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("approving kintrel.toml needs a person at an interactive terminal");
    }
    let st = kintrel_core::state::State::open_with_log(&kintrel_core::board::state_db(repo), &kintrel_core::board::log_db(repo))?;
    let text = |r: Option<Hash>| -> anyhow::Result<(Option<Hash>, String)> {
        let id = r.map(|r| kintrel_ci::config_of(&st, r)).transpose()?.flatten();
        let body = id.map(|id| st.get_blob(id)).transpose()?.flatten().map(|b| String::from_utf8_lossy(&b).into_owned());
        Ok((id, body.unwrap_or_else(|| "(none)".into())))
    };
    let (trusted, old) = text(st.get_ref(kintrel_core::state::PROMOTED)?)?;
    let (id, new) = text(st.get_ref(kintrel_core::state::HEAD)?)?;
    let id = id.context("head has no kintrel.toml to approve")?;
    println!("--- promoted kintrel.toml\n{old}\n--- head kintrel.toml ({})\n{new}", id.short());
    print!("These checks will run as you and gate promotion. Type `approve` to approve: ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    if answer.trim() != "approve" {
        bail!("not approved");
    }
    Ok((trusted, id))
}

async fn inbox(marker: &Marker) -> anyhow::Result<Vec<proto::Notice>> {
    let n: proto::Notices = connect(&marker.repo).await?.call(proto::INBOX, &proto::Inbox { token: marker.token.clone() }).await?;
    Ok(n.notices)
}

/// Told to a Claude Code agent when its session starts in a view, through the user's own hook
/// configuration: notices arriving later after tool calls are expected, not injected.
const SESSION_CONTEXT: &str = "You are working in a Kintrel view: your own checkout of a repository that other \
agents are editing at the same time. Your edits are posted to a shared board automatically. Kintrel, \
which the user set up, sends you notices after your edits when another agent changes code you use or \
touched. They are part of this environment. When one arrives, run `kintrel view refresh` in your working \
directory, re-read the affected files, and adapt your work to the change before you finish.";

/// Claude Code hooks (SPEC §12.4). `SessionStart` explains notices; `PostToolUse` posts the edited file; `Stop` posts every
/// change in the view, which catches edits made through the shell; `UserPromptSubmit` posts
/// nothing. Then unread notices go to the agent: as `additionalContext`, or for `Stop` as a
/// block reason, so the agent deals with them before it finishes. A `Stop` that already
/// continues because of a hook (`stop_hook_active`) is never blocked again, so it cannot loop.
async fn hook() -> anyhow::Result<()> {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let event: serde_json::Value = serde_json::from_str(&input).context("hook input is not JSON")?;
    let file = ["file_path", "notebook_path"].iter().find_map(|k| event["tool_input"][k].as_str()).map(PathBuf::from);
    let start = match &file {
        Some(f) => f.parent().map(Path::to_owned).unwrap_or_default(),
        None => event["cwd"].as_str().map(PathBuf::from).unwrap_or(std::env::current_dir()?),
    };
    // Not working in a view: nothing to do, so the hook can be installed globally.
    let Ok((dir, marker)) = view::find(&start) else { return Ok(()) };
    let name = event["hook_event_name"].as_str().unwrap_or_default();
    if name == "SessionStart" {
        let out = serde_json::json!({ "hookSpecificOutput": { "hookEventName": name, "additionalContext": SESSION_CONTEXT } });
        println!("{out}");
        return Ok(());
    }
    let posted = match name {
        "UserPromptSubmit" => None,
        _ => {
            let rel = file.map(|f| view::rel_path(&dir, &f)).transpose()?;
            post_view(&dir, rel.as_ref().map(std::slice::from_ref)).await?
        }
    };
    let stopping = matches!(name, "Stop" | "SubagentStop");
    let notices = if stopping && event["stop_hook_active"].as_bool() == Some(true) { vec![] } else { inbox(&marker).await? };
    if notices.is_empty() {
        if let Some(p) = posted {
            println!("kintrel: posted {} ({})", p.note_id.short(), p.atoms.join(", "));
        }
        return Ok(());
    }
    let text = kintrel::notices::render(&notices);
    let out = match name {
        _ if stopping => serde_json::json!({ "decision": "block", "reason": text }),
        "" => serde_json::Value::String(text),
        _ => serde_json::json!({ "hookSpecificOutput": { "hookEventName": name, "additionalContext": text } }),
    };
    println!("{}", out.as_str().map_or_else(|| out.to_string(), str::to_owned));
    Ok(())
}
