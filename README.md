# Kintrel

Version control for AI agents. Many agents edit one repository at once. Each edit is posted as a note, and posting never blocks. A combiner folds the notes into one head, merging per symbol (function, method, class member). What cannot be merged is kept as an explicit conflict.

**Status:** Phases 1–4 are done.
- **Phase 1:** the board, symbol-level merge, views, the Claude Code hook, MCP, and Git import/export.
- **Phase 2:** notices, intents, claims, conflict resolution and revert.
- **Phase 3:** CI from `kintrel.toml`:
  - checks cached by content, including across agents;
  - a gate that rejects the note that broke it, found by bisection;
  - lint feedback in milliseconds;
  - deploy hooks on promoted roots only.
- **Phase 4:** sandboxed checks and sandboxed agents on macOS, Linux and Windows (agents cannot approve their own config changes), LLM merge proposals, Git sync, and observability (`kintrel log`, metrics, `KINTREL_LOG`).

Symbol-level support exists for Rust, Python, TypeScript/TSX, JavaScript, Go and Java; every other text file merges at line level, and binary files are kept byte for byte. Untracked files that `.gitignore` excludes are never posted from a view.

To have Claude propose a merge for each conflict, add this to `kintrel.toml`:

```toml
[merge]
tier3 = true   # uses ANTHROPIC_API_KEY, else the logged-in `claude` CLI; tier3_command = [...] for another model
```

Each proposal costs one model call, and it reaches head only through the same gate as any other note.

## Platforms

- **macOS and Linux:** fully supported; the test suite passes on both. Checks run in `sandbox-exec` on macOS and in bubblewrap (`bwrap`) on Linux, so install bubblewrap there.
- **Windows:** the test suite passes on GitHub Actions' Windows runners. `kintrel agent` runs the agent in an AppContainer of its own. The daemon talks over a named pipe, and checks run in `cmd` inside an AppContainer. Checks see no files in your profile unless their `kintrel.toml` entry lists them under `readable`, and no network unless `network = true`. Two `cmd` limits apply inside the Windows sandbox, for checks and agents alike: redirecting to `nul` fails (the AppContainer cannot open it), and so does starting a program by an absolute path such as `C:\tools\x.exe`. Redirect to a file instead, and name programs by bare name on `PATH` or by a relative path.

## Build

```sh
cargo build --release      # Rust 1.94 (pinned in rust-toolchain.toml)
export PATH="$PWD/target/release:$PATH"
```

## Quickstart

```sh
cd my-repo
kintrel init                          # import the directory as the first root
VIEW=$(kintrel view create --agent worker-1)   # starts the daemon if needed
cd "$VIEW"                            # a full checkout, outside the repo
# ...edit files...
kintrel post                          # post every change in this view
kintrel view refresh                  # pull in everyone else's work
kintrel inbox [--follow]              # notices: others' changes to code you touched
kintrel claim -m "reworking pricing" "src/cart.rs::impl Cart"  # advisory, leased
kintrel intent -m "price() takes a currency" "src/cart.rs::impl Cart::fn price"
kintrel status                        # head, promotion lag, open conflicts, checks pending
kintrel log [--author worker-1]       # who wrote what, and what became of it
kintrel conflicts                     # what could not be merged (redo the change to resolve)
kintrel revert <note>                 # undo one note's symbols, keep later work
kintrel checks [--tier lint]          # run kintrel.toml checks on your view (cached by content, sandboxed)
kintrel packages                      # packages and the dependencies Kintrel infers
kintrel read src/lib.rs               # a file at head (or your view, inside one)
kintrel symbols src/lib.rs            # the atoms Kintrel tracks in a file
kintrel daemon metrics                # Prometheus text: posts, merges by tier, notices, checks, gate
```

The daemon logs to `.kintrel/daemon.log`; set `KINTREL_LOG=debug` (or `kintrel=debug`, any `tracing` filter) before it starts for a line per request and per combiner pass. A session may post 200 times a second; more are answered `BUSY` with a retry time.

## With Claude Code

Give each Claude Code instance its own view, and let a hook post every edit:

```sh
cd "$(kintrel --repo /path/to/repo view create --agent claude-1)" && kintrel agent -- claude
```

`kintrel agent` runs the agent sandboxed (Seatbelt on macOS, bubblewrap on Linux, an AppContainer on Windows), and only inside a view, so it cannot act as you. It writes only its view, temp files and its own Claude Code config. It cannot read the key that approving `kintrel.toml` needs, and it cannot reach other local services that would run code for it (tmux, `open`, `osascript`, `systemd-run --user`, Docker). Agents get their own Claude Code config (`CLAUDE_CONFIG_DIR`), so a planted hook never reaches yours: log in once inside with `/login` (it cannot open your browser; open the URL it prints), or set `ANTHROPIC_API_KEY`. Use `--writable DIR` for directories a tool must write, such as a build cache.

Add this to `~/.claude/settings.json`. `kintrel hook` does nothing outside a view, so it is safe to install globally:

```json
{
  "hooks": {
    "PostToolUse": [
      { "matcher": "Edit|Write|MultiEdit|NotebookEdit", "hooks": [{ "type": "command", "command": "kintrel hook" }] }
    ],
    "SessionStart": [
      { "hooks": [{ "type": "command", "command": "kintrel hook" }] }
    ],
    "UserPromptSubmit": [
      { "hooks": [{ "type": "command", "command": "kintrel hook" }] }
    ],
    "Stop": [
      { "hooks": [{ "type": "command", "command": "kintrel hook" }] }
    ]
  }
}
```

`PostToolUse` posts the file the tool just edited. `Stop` posts anything else that changed in the view during the turn, such as edits made through shell commands. Every hook also hands the agent its unread **notices**. A notice reports another agent's change to code this agent read or wrote, such as a changed body or signature, a deleted symbol, or a conflict that kept this agent's change out of head. `Stop` blocks once if notices are waiting. `SessionStart` tells the agent up front that it works in a shared view and that notices are expected. Without it, agents may rightly ignore notices that appear in tool output as unrequested instructions (seen in B4). The notices tell the agent to run `kintrel view refresh`, so allow the commands agents need, and only those: for example `--allowedTools "Bash(kintrel view refresh)" "Bash(kintrel inbox)" "Bash(kintrel checks:*)"`. Do not allow `Bash(kintrel:*)`. Promotion, and approving a changed `kintrel.toml` (`kintrel promote --approve-config`, which runs new commands as you), are a person's decisions. The CLI refuses approval without an interactive terminal. An agent started without `kintrel agent` runs as you and can do anything you can, so keep its shell access narrow.

## With other agents (MCP)

`kintrel mcp` is an MCP server on stdio. Each process is one agent session, and it reads and writes through the board without files on disk:

```json
{ "mcpServers": { "kintrel": { "command": "kintrel", "args": ["--repo", "/path/to/repo", "mcp", "--agent", "worker-1"] } } }
```

Tools: `kintrel_read_file`, `kintrel_list_symbols`, `kintrel_read_symbol`, `kintrel_write_file`, `kintrel_edit_symbol`, `kintrel_inbox`, `kintrel_intent`, `kintrel_claim`, `kintrel_release`, `kintrel_revert`, `kintrel_run_checks`, `kintrel_status`, `kintrel_refresh`, `kintrel_conflicts`, `kintrel_materialize` (real files, for running compilers and tests). The write tools also return unread notices.

## Git

```sh
kintrel init --from-git .     # the board starts at Git's HEAD
kintrel sync                  # bring in commits people made on that branch since
kintrel export                # commit the promoted root to kintrel/main, naming each agent's note
```

`kintrel sync` merges each new commit into head like an agent's edit; a commit that conflicts with agents' work is reported, and `kintrel conflicts` shows it. Merging `kintrel/main` back into your branch is safe to sync: only what you added comes in.

