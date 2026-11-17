# Usage and Configuration

## Quick Start

[Install anticrumb](building.md), then run in your project directory:

```sh
anticrumb
```

Enter `/browser login` and sign in to Google in Chrome. Models load automatically; type your task to begin. For Claude or ChatGPT, see [provider setup](../providers/README.md).

## CLI & Non-Interactive Commands

```sh
anticrumb -C ./project 'Investigate the failing tests'
anticrumb exec --json -C ./project 'Fix the reported bug'
anticrumb resume --last -C ./project
anticrumb exec resume AGENT_ID 'Add a regression test'
anticrumb agents list
```

Common flags: `-m/--model`, `--thinking`, `-C/--cd`, `-c/--continue`, `-r/--resume`, `--workers`, `--no-shell`, `--json`. Use `anticrumb --help` or a subcommand's `--help` for full options.

For browser commands, worker delegation, and external MCP setup, see [Tools](tools.md).

## Terminal Controls

| Command or key | Action |
|---|---|
| `/model` | Pick a cached model (Up/Down) and thinking level (Left/Right) |
| `/model refresh` | Discover models through Chrome |
| `/provider` | Select or set up a provider account |
| `/browser` | Inspect browser and sign-in state |
| `/browser login`, `/browser refresh` | Open sign-in or verify the current session |
| `/browse COMMAND` | Control web pages; `/browse help` lists commands |
| `/usage` | Read Gemini account limits and reset times |
| `/cache` | Show workspace and browser cache usage, reclaimable space, and last cleanup |
| `/cache prune [DAYS] [--dry-run]` | Clean eligible cache files; default retention is 7 days |
| `/status` | Refresh daemon and browser health |
| `/resume [ID]` | Browse or resume saved conversations |
| `/browser clear-history`, `/resume clear-history` | Delete web chats after typing `DELETE`; local saved entries remain |
| `/new`, `/clear` | Start a conversation; `/clear` also clears the transcript |
| `/permissions` | Toggle shell tools for the next turn |
| `/theme [ID]` | Pick and save a [theme](themes.md) |
| `/copy`, left mouse drag | Copy the last response or selected transcript text |
| Enter while running | Queue a steering message, optionally with `@file` |
| Esc / Ctrl-C | Cancel work or selection; Ctrl-C clears an idle draft, then exits on a second press |
| Tab | Complete commands and file paths |
| Ctrl/Alt-Left or Right, Alt-B/F | Move by word or path component |
| Alt-Enter / Shift-Enter / Ctrl-J | Insert a newline |
| Alt-E | Expand folded pasted text |

Command handling: [commands.rs](../src/tui/commands.rs). Input and rendering: [tui/](../src/tui/).

## Native Attachments and Images

```sh
anticrumb -C ./project --file ./notes.txt
anticrumb exec -C ./project 'Implement @docs/spec.md and @"design notes.txt"'
anticrumb exec -C ./project 'Review @git:diff and suggest tests'
anticrumb image 'Draw a minimalist rust gear icon'
anticrumb artifact get default ARTIFACT_ID ./result.webp
```

- `@path`, `@git:diff`, `@git:staged`, `--file PATH`, and `-i/--image PATH` upload through the provider: 32 MiB per file, 10 files per conversation. `@git:diff` and `@git:staged` generate temporary patch attachments (`git-diff.patch` / `git-staged.patch`) directly from the workspace git repository.
- When uploads are supported, the first agent prompt is a text attachment. Later turns send only new content; messages above the [provider threshold](../providers/README.md#prompt-attachment-threshold) also become attachments. Providers without uploads use inline text.
- An automatic prompt file uses one upload slot for that turn but is not retained as a user attachment. It has the same 32 MiB limit. Conversations keep their session; file transport does not increase model context capacity.
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

## Remote Chrome Daemon

Follow [remote TLS setup](rpc.md#network-grpc--tls-setup) to run Chrome on another machine while keeping workspace tools local.

## Provider Gateway & MCP

```sh
# Terminal A
anticrumb daemon --listen 127.0.0.1:50051
# Terminal B: choose a client below.
export ANTICRUMB_TOKEN='TOKEN_PRINTED_BY_DAEMON'
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
| `ANTICRUMB_TOKEN` | Network daemon/connection token; ignored by local IPC |
| `CHROME_BIN` | Chrome/Chromium executable |
| `ANTICRUMB_ALLOW_PRIVATE_FETCH` | `1` enables [private/local browsing](tools.md#web-and-browser-tools) |

Set browser environment variables on the browser host before starting its daemon. Client-side changes do not reconfigure a running browser host.

## Execution and Data Handling

Experimental software: browser changes, rate limits, verification challenges, or account restrictions can interrupt tasks.

- **Execution:** File edits and shell commands need no per-action approval. `--no-shell` and `/permissions` control built-in shell tools; file writes and configured MCP tools remain available. Use an isolated disposable workspace; a git worktree is not a sandbox.
- **Provider data:** Prompts, files read by tools, command output, and attachments may be sent to the selected web service under its account settings and data policies. Keep secrets and unauthorized data out of the workspace.
- **Local records:** History, prompts, tool results, artifacts, and browser profiles are stored under `ANTICRUMB_HOME`; remote mode stores state on both hosts. Debug logs may include prompts and output. Redact shared records; never share profiles, cookies, or daemon tokens.

## Service Terms

Unofficial and unaffiliated with Google, OpenAI, or Anthropic. Provided as-is, with no warranty or author liability; you are responsible for provider-term compliance and any consequences.
