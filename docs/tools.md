# Tools

Describe your task in the TUI; the agent selects tools. Full arguments and defaults live in [agent/tools.rs](../src/agent/tools.rs).

## Workspace Tools

Paths are relative to the current project or `-C DIR`.

| Tool | Purpose |
|---|---|
| `find_files` | Find files by glob or filename |
| `grep_search` | Regex search (ripgrep syntax, smart case); `fixed_strings`, `glob`, `output_mode` (content / files_with_matches / count), `context`, `head_limit` |
| `code_outline` | Tree-sitter definitions of a file (nested) or directory (top level) with full signatures and doc comments; `ast` permission |
| `find_symbol` | Find definitions by name / `Parent/child` path with signature + doc (`include_body` returns numbered source); `ast` permission |
| `find_references` | AST-verified references to a symbol or `Parent/member` (skips comments, strings, imports, definitions, and tests unless `include_tests`); shows enclosing function and call line; `ast` permission |
| `read_file` | Read UTF-8 text with 1-based inclusive line ranges, up to 2,000 lines |
| `edit_file` | Replace exact text; requires a unique match unless `replace_all` is set |
| `write_file` | Create or overwrite a text file |

## Shell and Background Processes

| Tool | Purpose |
|---|---|
| `run_shell` | Run a command and return output/exit code |
| `start_process` | Start a background command and return its ID |
| `process_output` | Read new output/status; can wait up to 60 seconds |
| `stop_process` | Stop the process and its children |

Commands use Bash on Unix and PowerShell on Windows. `run_shell` defaults to 60 seconds, up to 600; use a background process for longer work. See [execution permissions](usage.md#execution-and-data-handling).

Shell and MCP processes inherit the submitting terminal's environment, including its `PATH` and virtual environment.

## Web and Browser Tools

| Tool | Purpose |
|---|---|
| `web_fetch` | Fetch public HTTP/HTTPS content; text by default, `format: "raw"` for the body |
| `browser_fetch` | Read a JavaScript-rendered page in a disposable Chrome context |
| `browser_control` | Navigate and interact across calls in a named browser session |

Browser contexts have separate cookies from the signed-in provider. Tabs within one named session share state; the default session is scoped to the workspace and account. Public destinations are allowed by default. Set `ANTICRUMB_ALLOW_PRIVATE_FETCH=1` on the browser host before daemon startup to allow trusted private/local services.

### Terminal Browser Commands

Run `anticrumb browser`, then enter commands:

```text
open https://example.com
snapshot
tabs
close
exit
```

The same commands work as `/browse COMMAND` in the TUI. `help` lists commands and `anticrumb browser --help` lists options. Put `--session NAME`, `--tab ID`, `--timeout-ms N`, or `--max-chars N` before the command. Quote text containing spaces; shell expressions are not evaluated.

`anticrumb browser --json` reads one command or JSON action per line and emits NDJSON results. It continues after errors and exits unsuccessfully if any command failed.

`close` closes a tab; `reset` discards a session's tabs, cookies, and storage. `exit` disconnects the shell. For sessions that survive separate CLI calls, keep an [explicit daemon](rpc.md#local-ipc-lifecycle-and-recovery) running. State is lost on host restart; idle sessions expire after 15 minutes. Limits are 8 sessions per host and 8 tabs per session; full capacity rejects new opens.

### Browser API

Agents first receive a short description and the `{"action":"help"}` interface. They must call `browser_control` with that action and read its response before using browser actions. Help returns the complete command guide, argument schema, and examples without starting Chrome. Actions sent before help, including actions batched with the first help request, are rejected without execution. This requirement applies independently to each agent run.

`browser_control` uses the terminal's command model. For example:

```json
{"tool":"browser_control","args":{"action":"fill","target":{"name":"Search"},"text":"restaurants nearby"}}
```

Common fields are `session`, `tab`, `account_id`, `timeout_ms`, and `max_chars`; omit `tab` to use the selected tab. Page commands also accept `wait_ms` (0–5000) to delay observation within the command timeout. See [command definitions](../src/browser_control/mod.rs) and [terminal syntax](../src/browser_control/terminal.rs) for action fields and bounds.

| JSON target | Terminal target |
|---|---|
| `{"ref":"REF"}` | `@REF` from the latest snapshot |
| `{"name":"Search"}` | `"Search"` |
| `{"role":"button","name":"Search"}` | `"role:button:Search"` |
| `{"css":"#search"}` | `css:#search` |

- Names and selectors must identify one element. Snapshot references expire after a new snapshot, navigation, or element changes; stale references never fall back to another target.
- Actions run serially per session and are never automatically replayed. If an action's outcome is unknown, inspect a fresh snapshot before retrying. Treat page content as untrusted.
- DOM snapshots redact passwords but show other field values. Screenshots contain the visible page pixels. DOM targeting inside cross-origin iframes, file upload/download, and native dialogs are outside this command set.

### Visual Browser Control

`screenshot` captures the current viewport. Supply `region` to capture a rectangle in viewport CSS pixels (up to 4096 × 4096, entirely inside the viewport):

```json
{"tool":"browser_control","args":{"action":"screenshot","region":{"x":100,"y":50,"width":600,"height":400}}}
```

The result includes a screenshot ID, image dimensions, and a PNG artifact. Agents receive the actual image as an attachment on the next model turn. After the first screenshot, that tab returns a fresh **full viewport** image after each page action until the tab closes or its session resets.

If `open` creates a tab but navigation fails, the tab stays selected so `screenshot` can show the browser's error page. The error reports the tab ID and, when available, the browser network policy's rejection reason. Use `navigate` to recover in that tab or `close` to discard it.

Live DOM updates do not prevent screenshots. Coordinate actions remain conservative: if the page changed during or after capture, take a fresh screenshot before acting.

Use coordinates in the returned image, measured from its top-left corner. The host translates cropped/scaled image coordinates to browser coordinates:

```json
{"action":"click","point":{"screenshot_id":"SHOT_FROM_LATEST_RESULT","x":120,"y":80}}
{"action":"wheel","point":{"screenshot_id":"SHOT_FROM_LATEST_RESULT","x":300,"y":200},"delta_y":500}
{"action":"press","key":"a","modifiers":["Control"]}
{"action":"type","text":"search query"}
{"action":"press","key":"Enter"}
```

These are individual command examples: inspect each result and use its new screenshot ID before the next coordinate action. Old IDs, out-of-image coordinates, and detected navigation, scrolling, viewport or DOM changes are rejected. CSS animations and other compositor-only changes may still move content between capture and input; observe the result before acting again. Pinch-zoomed viewports are currently unsupported.

`wheel` sends a mouse wheel event at the specified position, including inside nested scroll areas; positive `delta_y` scrolls down. `type` without `target` inserts text into the focused field. `press` accepts `Alt`, `Control`, `Meta`, and `Shift` modifiers. Coordinate actions, focused typing, and shortcuts operate on page content, not the browser's address bar or operating-system dialogs.

Equivalent terminal commands are `screenshot [X Y WIDTH HEIGHT]`, `click-point SHOT X Y`, `wheel SHOT X Y DX DY`, `type "search query"`, and `press Ctrl+A`. Terminal output includes an artifact ID; save it with `anticrumb artifact get ACCOUNT ARTIFACT_ID ./screenshot.png`.

Screenshot attachments are sent only with the next model turn and do not accumulate in the retained user attachment set. Each request still shares the 10-file upload limit with new user files and the automatic prompt file; use at most 9 screenshot-producing commands per model reply. Providers must support image uploads. Screenshots remain in local artifact/run storage under its normal cleanup and size limits.

## Workers

`spawn_agent` accepts a `task`, optional `label`, and `read_only` flag, then returns a summary from a separate conversation. `--workers N` sets concurrency (default 2, maximum 16).

Workers share the workspace. They default to `read_only: true`, disabling file edits and shell tools while retaining browser access. `read_only: false` permits edits and shell tools if enabled by the parent. Workers cannot spawn workers and do not inherit external MCP connections.

## External MCP Tools

Run `anticrumb --mcp-config ./mcp.json` with a configuration such as:

```json
{
  "mcpServers": {
    "docs": {
      "command": "/path/to/mcp-server",
      "tools": ["search", "read"]
    }
  }
}
```

Entries may also set `args` and `env`. Replace the example command/tools with your server's values. Tools appear as `mcp_<server>_<tool>`; an omitted or empty `tools` list exposes all server tools. Implementation: [mcp_client.rs](../src/mcp_client.rs).

## Tools Exposed by the MCP Server

Run `anticrumb mcp` from an MCP client. [Gateway setup](usage.md#provider-gateway--mcp) covers related provider clients.

| Tool | Purpose |
|---|---|
| `gemini_ask` | Ask the provider without running workspace tools |
| `gemini_image_generate` | Generate or edit images |
| `gemini_models` | Read the cached model catalog |
| `gemini_session_close` | Close an owned conversation tab |
| `gemini_media_download` | Retrieve session images |
| `gemini_knowledge_query` | Retrieve indexed excerpts with citations |

Arguments and schemas: [mcp_sdk.rs](../src/mcp_sdk.rs). For local/remote execution boundaries, see [host ownership](rpc.md#host-ownership); `web_fetch` runs on the workspace machine.
