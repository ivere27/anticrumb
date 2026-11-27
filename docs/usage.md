# Usage and Configuration

## Quick Start

[Install anticrumb](building.md), then run in your project directory:

```sh
anticrumb          # Chat mode (default: direct chat, no tool overhead)
anticrumb --temp   # Anonymous temporary chat for this run (no server history)
anticrumb --agent  # Agent mode (coding tools, respects /permissions)
anticrumb --yolo   # Agent mode with all permissions (file, shell & sub-agents)
```

Enter `/browser login` and sign in to Google in Chrome. Models load automatically; type your prompt to begin. Press `Shift+Tab` to toggle between Chat and Agent mode (or Chat and Agent `--yolo` when explicitly launched with `--yolo` or switched via `/mode yolo`), or use `/mode` (`/mode chat`, `/mode agent`, or `/mode yolo`) and `/temp` (`/temp on` / `/temp off`, persisted across launches) to switch modes at runtime. For Claude or ChatGPT, see [provider setup](../providers/README.md).

## CLI & Non-Interactive Commands

```sh
anticrumb --agent -C ./project 'Investigate the failing tests'
anticrumb exec --yolo --json -C ./project 'Fix the reported bug'
anticrumb --last -C ./project
anticrumb exec --resume AGENT_ID 'Add a regression test'
anticrumb agents list
```

Common flags: `--agent` (agent mode respecting `/permissions`), `--yolo` (agent mode with all permissions), `--chat` (chat mode, default), `--temp` (enable anonymous temporary chat for this run), `-m/--model`, `--thinking`, `-C/--cd`, `-c/--continue`, `--resume`, `--workers`, `--no-shell`, `--json`. Use `anticrumb --help` or a subcommand's `--help` for full options.

For browser commands, worker delegation, and external MCP setup, see [Tools](tools.md).

## Terminal Controls

| Command or key | Action |
|---|---|
| `/mode [chat\|agent\|yolo]` | Switch between Chat mode (default) and Agent (`--yolo`) mode |
| `/temp [on\|off]` | Enable or disable anonymous temporary chat (no server history; saved across restarts) |
| `/model` | Pick a cached model (Up/Down) and thinking level (Left/Right) |
| `/model refresh` | Discover models through Chrome |
| `/provider` | Select or set up a provider |
| `/account [TARGET\|refresh]` | Switch between signed-in accounts on a multi-account provider (e.g. Gemini Multi-Login) or rescan Chrome |
| `/browser` | Inspect provider browser and sign-in state |
| `/browser login`, `/browser refresh` | Open provider sign-in (discovers accounts) or verify the current account |
| `/browse COMMAND` | Control web pages; `/browse help` lists commands |
| `/usage` | Read web usage limits and reset times for the active account |
| `/cache` | Show workspace and browser cache usage, reclaimable space, and last cleanup |
| `/cache prune [DAYS] [--dry-run]` | Clean eligible cache files; default retention is 7 days |
| `/status` | Refresh daemon and browser health |
| `/resume [ID]` | Browse or resume saved conversations |
| `/browser clear-history`, `/resume clear-history` | Delete web chats after typing `DELETE`; local saved entries remain |
| `/new`, `/clear` | Start a conversation; `/clear` also clears the transcript |
| `/permissions [readonly\|edit\|all]` | Toggle tool permission groups (`read`, `edit`, `shell`, `browser`, `subagent`; default: `read` only) |
| `/theme [ID]` | Pick and save a [theme](themes.md) |
| `/copy`, left mouse drag | Copy the last response or selected transcript text |
| Enter while running | Queue a steering message, optionally with `@file` |
| Esc / Ctrl-C | Cancel work or selection; Ctrl-C clears an idle draft, then exits on a second press |
| Tab | Complete commands and file paths |
| Shift-Tab | Toggle Chat / Agent mode (Chat / Agent `--yolo` only when explicitly launched with `--yolo` or set via `/mode yolo`) |
| Ctrl/Alt-Left or Right, Alt-B/F | Move by word or path component |
| Alt-Enter / Shift-Enter / Ctrl-J | Insert a newline |
| Alt-E | Expand folded pasted text |

Command handling: [commands.rs](../src/tui/commands.rs). Input and rendering: [tui/](../src/tui/).

## Native Attachments and Images

```sh
anticrumb -C ./project --file ./notes.txt
anticrumb exec -C ./project 'Implement @docs/spec.md and @"design notes.txt"'
anticrumb exec -C ./project 'Review @git:diff and suggest tests'
anticrumb exec -C ./project 'Review @git:diff+ and suggest tests'
anticrumb exec -C ./project 'Review @git:HEAD~1 and check for regressions'
anticrumb exec -C ./project 'Review @git:HEAD~1+ with AST context'
anticrumb image 'Draw a minimalist rust gear icon'
anticrumb artifact get gemini ARTIFACT_ID ./result.webp
anticrumb accounts list
anticrumb accounts use alice@gmail.com
anticrumb accounts refresh
```

- `@path`, `@git:diff`, `@git:diff+`, `@git:staged`, `@git:staged+`, `@git:<commit>`, `@git:<commit>+`, `--file PATH`, and `-i/--image PATH` upload through the provider: 100 MiB per file, 10 files per conversation. `@git:diff`, `@git:staged`, and `@git:<commit>` (commit SHA, `HEAD`, `HEAD~1`, branch, or tag) generate temporary patch attachments (`git-diff.patch` / `git-staged.patch` / `git-<sha>.patch`) directly from the workspace git repository. `@git:diff+`, `@git:staged+`, and `@git:<commit>+` run the **[3-Layer Hybrid Git+AST Diff pipeline](git-diff-hybrid.md)** (`git-diff+.patch`, `git-staged+.patch`, or `git-<sha>+.patch`) without truncating production code (logging an advisory warning when output exceeds `512 KiB`, or `ANTICRUMB_PATCH_BUDGET_KIB`):
  1. **Layer 1 (Upfront Commit Symbol Index):** Compact per-file churn, compaction tags, and modified symbol table of contents at the top of multi-file diffs.
  2. **Layer 2 (Refined Inline `git+ast` Diff):** Tree-sitter expansion of modified hunks to enclosing AST definitions, visibility-only `[sig-only]` body folding, `[clamped]` full parameter preservation (up to 200 chars), named `[+ sym1, sym2]` merged hunk headers, `" › "` scope separators, and decorator/annotation preservation.
  3. **Layer 3 (External 1-Hop AST Context):** LSP-free extraction of unmodified same-file types/constants and snapshot-safe (`git cat-file --batch`) 1-hop cross-file types, body-stripped function signatures, and concise doc comments (`// doc: ...`) directly referenced by the diff.
  Plain documentation/config files in `plain_diff` (`.md`, `.txt`, `.yml`, `.toml`, etc.) attach cleanly as `-U3` diffs without warnings; other unsupported languages log a warning and fall back to `git diff/show -W`. When a `@git:<rev>+` mention ends with `+`, anticrumb tries AST expansion on `<rev>` first and falls back to the literal ref `<rev>+` if `<rev>` does not resolve (so if both `foo` and `foo+` exist, `@git:foo+` expands `foo`; use the commit SHA to attach `foo+` directly). If an `@` mention in prompt text cannot be attached (for example, missing prose like `@username`, a directory, an oversized file, or an unresolvable `@git:...`), anticrumb logs a warning and keeps the `@...` text literal so the prompt still sends.
- When uploads are supported, the first agent prompt is a text attachment. Later turns send only new content; messages above the [provider threshold](../providers/README.md#prompt-attachment-threshold) also become attachments. Providers without uploads use inline text.
- An automatic prompt file uses one upload slot for that turn but is not retained as a user attachment. It has the same 100 MiB limit. Conversations keep their session; file transport does not increase model context capacity.
- [Browser screenshots](tools.md#visual-browser-control) are also sent only with the next model turn. They share that turn's upload slots but do not count toward the retained user attachment set.

## Local Daemon

Run `anticrumb` in each project directory; clients share a local daemon. Use `anticrumb daemon` to keep browser sessions alive between client connections. See [lifetime, state, and recovery](rpc.md#local-ipc-lifecycle-and-recovery).

### Runtime Health and Debugging

`/status` refreshes daemon PID/version, lifetime, endpoint, clients, workspace jobs, and browser connectivity. Failed requests retain values marked as stale or last known. `anticrumb doctor` returns status as JSON and starts a daemon if needed. Daemon health does not establish provider sign-in; use `/browser refresh` for that.

To debug, stop the existing daemon (or close all automatic-daemon clients), then run the same binary in two terminals:

```sh
# Terminal A: keep running; attach a debugger to this daemon's PID.
anticrumb daemon
# Terminal B
anticrumb -C /path/to/project
```

Use `/debug info` or `/debug trace` for TUI logs; `/debug off` closes the log. Restart the daemon after rebuilding if a protocol mismatch occurs.

| Diagnostic | Location |
|---|---|
| Automatic daemon stderr | `<ANTICRUMB_HOME>/run/daemon.log`; replaced on the next start |
| Explicit daemon output | Its terminal |
| Runtime record | `<ANTICRUMB_HOME>/run/daemon.json`; `/status` reports live values |
| TUI debug log | OS temporary directory; exact path in `/status` |

### Cache Retention

`/cache` reports the workspace and connected browser stores separately. The same operations are available from the CLI:

```sh
anticrumb cache
anticrumb cache prune --dry-run
anticrumb cache prune
anticrumb cache prune 14
```

The default retention is **7 days**; an explicit duration accepts 1–3650 days. `--dry-run` previews eligible files and reclaimable bytes without deleting files or updating the cleanup timestamp. A disconnected browser is reported as a partial failure; local results remain visible.

Each owning daemon checks its store at startup and every six hours. An attachment write also checks for eligible cleanup when blob storage approaches 80% of its 1 GiB limit, at most once per minute. Limits never override reference protection.

- Upload/download staging directories are removed when their operation finishes. Abandoned application-owned directories and incomplete or unregistered blobs become eligible after 24 hours. Live directory leases protect ongoing operations, including operations in another process.
- Unreferenced attachments and screenshots become eligible after seven days. Saved records and journal events protect their referenced artifacts, including damaged files. Referenced corruption is reported when the artifact is read; cleanup does not discard it. Shared blob bytes remain while any artifact still needs them.
- While an agent or provider turn is active, attachment and orphan-blob deletion is deferred. Unleased legacy temporary directories are also retained until the store is idle.
- Expired cache entries are removed. Cache cleanup retains conversation, run, and session records, Chrome profiles, account credentials, settings, and provider files. Files outside the owned cache directories and symbolic links are excluded.

If protected files keep the store full, `/cache` reports usage and the upload error reports the additional space required. Cleanup does not shorten retention or remove referenced files to make an upload fit.

## Remote Chrome Server

Follow [remote TLS setup](rpc.md#network-grpc--tls-setup) to run Chrome on another machine while keeping workspace tools local.

## Provider Gateway & MCP

```sh
# Terminal A
anticrumb server 127.0.0.1:50051
# Terminal B: choose a client below.
export ANTICRUMB_TOKEN='TOKEN_PRINTED_BY_SERVER'
anticrumb connect opencode --install ./opencode.json
anticrumb connect codex
anticrumb connect claude
anticrumb connect antigravity
anticrumb mcp
```

The gateway exposes OpenAI Chat/Responses, Anthropic Messages, and Gemini APIs on the same TLS port with bearer authentication. Route definitions are in [http.rs](../src/http.rs). `anticrumb mcp` exposes the [stdio MCP tools](tools.md#tools-exposed-by-the-mcp-server).

## Environment Variables

| Variable | Purpose |
|---|---|
| `ANTICRUMB_HOME` | State directory and local daemon identity; default `~/.anticrumb` |
| `ANTICRUMB_TOKEN` | Network server/connection token; ignored by local IPC |
| `CHROME_BIN` | Chrome/Chromium executable |
| `ANTICRUMB_ALLOW_PRIVATE_FETCH` | `1` enables [private/local browsing](tools.md#web-and-browser-tools) |
| `ANTICRUMB_PATCH_BUDGET_KIB` | Advisory KiB warning threshold for `@git:diff+`/`@git:staged+`/`@git:<commit>+` (default `512`, clamped `16`–`16384`; warns without truncating) |
| `ANTICRUMB_NO_NOTICE` | `1` or `true` disables optional startup tips and author project notices in the interactive TUI |

Set browser environment variables on the browser host before starting its daemon. Client-side changes do not reconfigure a running browser host.

## Execution and Data Handling

Experimental software: browser changes, rate limits, verification challenges, or account restrictions can interrupt tasks.

- **Execution:** File edits and shell commands need no per-action approval. `--no-shell` and `/permissions` control built-in shell tools; file writes and configured MCP tools remain available. Use an isolated disposable workspace; a git worktree is not a sandbox.
- **Provider data:** Prompts, files read by tools, command output, and attachments may be sent to the selected web service under its account settings and data policies. Keep secrets and unauthorized data out of the workspace.
- **Local records:** History, prompts, tool results, artifacts, and browser profiles are stored under `ANTICRUMB_HOME`; remote mode stores state on both hosts. Debug logs may include prompts and output. Redact shared records; never share profiles, cookies, or daemon tokens.

## Service Terms

Unofficial and unaffiliated with Google, OpenAI, or Anthropic. Provided as-is, with no warranty or author liability; you are responsible for provider-term compliance and any consequences.
