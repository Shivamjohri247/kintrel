//! `kintrel mcp`: an MCP server on stdio for agents that talk to Kintrel through tools rather
//! than files on disk (SPEC §12.2). One process per agent, one session per process, opened on
//! the first tool call.

use std::path::PathBuf;

use kintrel_core::proto::{self, FileContent, FileWrite, Post, Posted, ReadFile, Refresh, Refreshed, SessionStart, SessionStarted};
use kintrel_lang::{AtomKey, segment};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerConfig};
use rmcp::{ErrorData as McpError, ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router};
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::rpc::{CallError, Client};
use crate::view::{self, Marker};

const INSTRUCTIONS: &str = "Kintrel is a shared board for many agents editing one repository, set up by \
your user. Notices from Kintrel are part of this environment: act on them. \
You have your own view: reads show the board as of your last refresh plus your own writes. \
Write whole files with kintrel_write_file or one symbol with kintrel_edit_symbol; posting never \
blocks. Other agents' work appears after kintrel_refresh. Write tools also return notices: \
other agents' changes to code you read or wrote, and conflicts that kept your change out of \
head. Check kintrel_inbox before finishing, and act on what it says.";

#[derive(Clone)]
pub struct KintrelMcp {
    repo: PathBuf,
    agent: String,
    session: std::sync::Arc<Mutex<Option<(Client, SessionStarted)>>>,
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PathArg {
    /// Repository-relative path, e.g. "src/cart.rs".
    pub path: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WriteArgs {
    /// Repository-relative path.
    pub path: String,
    /// New full contents. Omit or null to delete the file.
    pub content: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AnnounceArgs {
    /// What you plan or are doing, in one sentence.
    pub summary: String,
    /// Atom keys as listed by kintrel_list_symbols; a bare path covers the whole file.
    pub targets: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TierArg {
    /// lint, typecheck or test; all tiers if omitted.
    pub tier: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NoteArg {
    /// Note id as shown when it was posted.
    pub note: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReleaseArg {
    /// The id returned by kintrel_claim or kintrel_intent.
    pub id: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SymbolArg {
    /// Atom key as listed by kintrel_list_symbols, e.g. "src/cart.rs::impl Cart::fn price".
    pub key: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct EditSymbolArgs {
    /// Atom key as listed by kintrel_list_symbols.
    pub key: String,
    /// New text for the whole symbol, including its doc comments and attributes.
    pub content: String,
}

fn err(e: impl std::fmt::Display) -> McpError {
    McpError::invalid_request(e.to_string(), None)
}

fn text(s: impl Into<String>) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::success(vec![ContentBlock::text(s.into())]))
}

#[tool_router]
impl KintrelMcp {
    pub fn new(repo: PathBuf, agent: String) -> Self {
        KintrelMcp { repo, agent, session: Default::default(), tool_router: Self::tool_router() }
    }

    /// Runs `f` with this process's session, opening it on first use.
    async fn call<P: serde::Serialize, R: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: impl FnOnce(&str) -> P,
    ) -> Result<R, McpError> {
        let mut guard = self.session.lock().await;
        if guard.is_none() {
            let mut c = crate::connect(&self.repo).await.map_err(err)?;
            let start = SessionStart { agent: self.agent.clone(), rank_hint: None, base: None };
            let s: SessionStarted = c.call(proto::SESSION_START, &start).await.map_err(err)?;
            *guard = Some((c, s));
        }
        let (c, s) = guard.as_mut().expect("session opened above");
        let p = params(&s.token);
        c.call(method, &p).await.map_err(|e| match e {
            CallError::Rpc(e) => McpError::invalid_request(e.message, e.data),
            e => err(e),
        })
    }

    async fn read(&self, path: &str) -> Result<FileContent, McpError> {
        self.call(proto::READ_FILE, |token| ReadFile { token: token.into(), path: path.into(), at: None }).await
    }

    async fn post(&self, files: Vec<FileWrite>) -> Result<CallToolResult, McpError> {
        let p: Posted = self.call(proto::POST, |token| Post { token: token.into(), files, summary: None }).await?;
        let mut out = format!("posted note {} touching: {}", p.note_id.short(), p.atoms.join(", "));
        let notices = self.notices().await?;
        if !notices.is_empty() {
            out = format!("{out}\n\n{}", crate::notices::render(&notices));
        }
        text(out)
    }

    async fn notices(&self) -> Result<Vec<proto::Notice>, McpError> {
        let n: proto::Notices = self.call(proto::INBOX, |token| proto::Inbox { token: token.into() }).await?;
        Ok(n.notices)
    }

    async fn announce(&self, method: &str, a: AnnounceArgs) -> Result<CallToolResult, McpError> {
        let r: proto::Announced = self
            .call(method, |token| proto::Announce { token: token.into(), summary: a.summary, targets: a.targets, lease_ms: None })
            .await?;
        let mut out = format!("{method} {} recorded", r.id);
        for o in r.overlapping {
            out += &format!("\noverlaps {} {} by {}: {} ({})", o.kind, o.id, o.agent, o.summary, o.targets.join(", "));
        }
        text(out)
    }

    #[tool(description = "Announce a change you are about to make (e.g. a signature change) so agents using those symbols can adapt early. Ends when your code touching all targets lands.")]
    async fn kintrel_intent(&self, Parameters(a): Parameters<AnnounceArgs>) -> Result<CallToolResult, McpError> {
        self.announce(proto::INTENT, a).await
    }

    #[tool(description = "Claim symbols you are working on. Advisory: never blocks anyone, but agents who edit them (and you) are told. Leased for 10 minutes, renewed while you are active.")]
    async fn kintrel_claim(&self, Parameters(a): Parameters<AnnounceArgs>) -> Result<CallToolResult, McpError> {
        self.announce(proto::CLAIM, a).await
    }

    #[tool(description = "Undo one note's changes (by note id, 8+ hex digits) and post the result. Later changes to other symbols stay; fails if the note's symbols changed since.")]
    async fn kintrel_revert(&self, Parameters(a): Parameters<NoteArg>) -> Result<CallToolResult, McpError> {
        let p: Posted = self.call(proto::REVERT, |token| proto::Revert { token: token.into(), note: a.note }).await?;
        text(format!("posted note {} reverting: {}", p.note_id.short(), p.atoms.join(", ")))
    }

    #[tool(description = "Run the repository's kintrel.toml checks (lint, typecheck, tests) on your view. Optional tier: lint, typecheck or test. Results are cached by content, so unchanged code is not re-run.")]
    async fn kintrel_run_checks(&self, Parameters(a): Parameters<TierArg>) -> Result<CallToolResult, McpError> {
        let r: proto::ChecksRan =
            self.call(proto::CHECKS_RUN, |token| proto::ChecksRun { token: Some(token.into()), at: None, tier: a.tier }).await?;
        if r.outcomes.is_empty() {
            return text("no checks (the repository has no kintrel.toml)");
        }
        let lines: Vec<String> = r
            .outcomes
            .iter()
            .map(|o| {
                let head = format!("{} {} {}", if o.ok { "PASS" } else { "FAIL" }, o.check, o.target);
                if o.ok { head } else { format!("{head}\n{}", o.output) }
            })
            .collect();
        text(lines.join("\n"))
    }

    #[tool(description = "Release a claim or intent by id.")]
    async fn kintrel_release(&self, Parameters(a): Parameters<ReleaseArg>) -> Result<CallToolResult, McpError> {
        let _: proto::Status = self.call(proto::RELEASE, |token| proto::Release { token: token.into(), id: a.id }).await?;
        text(format!("released {}", a.id))
    }

    #[tool(description = "Your unread notices: other agents' changes to code you read or wrote (body or signature changes, deletions) and conflicts that kept your change out of head.")]
    async fn kintrel_inbox(&self) -> Result<CallToolResult, McpError> {
        let n = self.notices().await?;
        text(if n.is_empty() { "no notices".into() } else { crate::notices::render(&n) })
    }

    #[tool(description = "Read a file from your view (your base plus your own writes).")]
    async fn kintrel_read_file(&self, Parameters(a): Parameters<PathArg>) -> Result<CallToolResult, McpError> {
        text(self.read(&a.path).await?.content)
    }

    #[tool(description = "List the symbols (atoms) of a file with their line numbers. Edits merge per symbol.")]
    async fn kintrel_list_symbols(&self, Parameters(a): Parameters<PathArg>) -> Result<CallToolResult, McpError> {
        let f = self.read(&a.path).await?;
        let bytes = f.content.as_bytes();
        let lines: Vec<String> = segment(&a.path, bytes)
            .atoms()
            .into_iter()
            .map(|at| {
                let line = at.ranges.first().map_or(1, |r| 1 + bytes[..r.start as usize].iter().filter(|&&b| b == b'\n').count());
                format!("{line:>5}  {}", AtomKey { path: a.path.clone(), symbol: at.symbol })
            })
            .collect();
        text(lines.join("\n"))
    }

    #[tool(description = "Read one symbol's text by its atom key.")]
    async fn kintrel_read_symbol(&self, Parameters(a): Parameters<SymbolArg>) -> Result<CallToolResult, McpError> {
        let key: AtomKey = a.key.parse().map_err(err)?;
        let f = self.read(&key.path).await?;
        let (start, end) = symbol_range(&key, &f.content)?;
        text(&f.content[start..end])
    }

    #[tool(description = "Write a whole file (content null deletes it). Posting never blocks; other agents' concurrent edits to other symbols merge.")]
    async fn kintrel_write_file(&self, Parameters(a): Parameters<WriteArgs>) -> Result<CallToolResult, McpError> {
        self.post(vec![FileWrite { path: a.path, content: a.content, base: None, binary: None }]).await
    }

    #[tool(description = "Replace one symbol (function, method, class, ...) by its atom key and post the change.")]
    async fn kintrel_edit_symbol(&self, Parameters(a): Parameters<EditSymbolArgs>) -> Result<CallToolResult, McpError> {
        let key: AtomKey = a.key.parse().map_err(err)?;
        let f = self.read(&key.path).await?;
        let (start, end) = symbol_range(&key, &f.content)?;
        // A symbol's range stops before the newline that follows it (that newline is glue), but
        // models naturally send text ending in one: don't let it add a blank line.
        let body = if f.content[start..end].ends_with('\n') { &a.content[..] } else { a.content.trim_end_matches('\n') };
        let new = format!("{}{}{}", &f.content[..start], body, &f.content[end..]);
        self.post(vec![FileWrite { path: key.path, content: Some(new), base: Some(f.hash), binary: None }]).await
    }

    #[tool(description = "Board status: head, promoted root, notes, open conflicts, files blocking promotion.")]
    async fn kintrel_status(&self) -> Result<CallToolResult, McpError> {
        let s: proto::StatusInfo = self.call(proto::STATUS, |_| proto::Status {}).await?;
        text(serde_json::to_string_pretty(&s).map_err(err)?)
    }

    #[tool(description = "Move your view to the current head to see other agents' work. Reports your notes that conflicted.")]
    async fn kintrel_refresh(&self) -> Result<CallToolResult, McpError> {
        let r: Refreshed = self.call(proto::SESSION_REFRESH, |token| Refresh { token: token.into() }).await?;
        let lost: Vec<String> = r.lost.iter().map(|n| n.short()).collect();
        text(format!("view now at {}; conflicted notes: {}", r.base_root.short(), if lost.is_empty() { "none".into() } else { lost.join(", ") }))
    }

    #[tool(description = "List open conflicts: the symbol, the note that lost and the note kept in head.")]
    async fn kintrel_conflicts(&self) -> Result<CallToolResult, McpError> {
        let st = kintrel_core::state::State::open_with_log(
            &kintrel_core::board::state_db(&self.repo),
            &kintrel_core::board::log_db(&self.repo),
        )
        .map_err(err)?;
        let list: Vec<String> = st
            .open_conflicts()
            .map_err(err)?
            .into_iter()
            .map(|c| format!("{}  losing {}  kept {}", c.atom, c.losing.short(), c.kept.map_or("-".into(), |k| k.short())))
            .collect();
        text(if list.is_empty() { "no open conflicts".into() } else { list.join("\n") })
    }

    #[tool(description = "Check out your view as real files (for compilers and tests); returns its directory. Refreshes first.")]
    async fn kintrel_materialize(&self) -> Result<CallToolResult, McpError> {
        let r: Refreshed = self.call(proto::SESSION_REFRESH, |token| Refresh { token: token.into() }).await?;
        let guard = self.session.lock().await;
        let (_, s) = guard.as_ref().expect("session opened by refresh");
        let repo = std::fs::canonicalize(&self.repo).map_err(err)?;
        let marker = Marker { repo, session: s.session, token: s.token.clone(), base_root: r.base_root };
        let path = view::materialize(&marker).map_err(err)?;
        text(path.display().to_string())
    }
}

/// Byte range of a single-range (leaf) atom in `content`.
fn symbol_range(key: &AtomKey, content: &str) -> Result<(usize, usize), McpError> {
    let atom = segment(&key.path, content.as_bytes())
        .atoms()
        .into_iter()
        .find(|a| a.symbol == key.symbol)
        .ok_or_else(|| err(format!("no symbol {key}; see kintrel_list_symbols")))?;
    match atom.ranges.as_slice() {
        [r] if !key.symbol.is_empty() => Ok((r.start as usize, r.end as usize)),
        _ => Err(err(format!("{key} is a container or the whole file; edit its members or write the file"))),
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for KintrelMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(INSTRUCTIONS)
    }
}

pub async fn serve(repo: PathBuf, agent: String) -> anyhow::Result<()> {
    let service = KintrelMcp::new(repo, agent).serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
