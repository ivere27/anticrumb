# Web Providers

The three providers below are bundled with their manifests and DOM scripts. Claude and ChatGPT are experimental.

| ID | Support |
|---|---|
| `gemini-web` | Native Gemini integration: uploads, images, usage, and conversation management |
| `claude-web` | Model selection, usage, and `--thinking low\|medium\|high\|extra\|max` |
| `chatgpt-web` | Free Auto/Think (`--thinking standard\|think`) and paid model selection |

## Sign In and Run

```sh
anticrumb login claude-web
anticrumb --provider claude-web
```

Use any provider ID above, or `--account ID` for a saved account. In the TUI, `/provider` selects or sets up an account. For manual account/port control, see `anticrumb accounts --help`.

## Updating DOM Scripts Without Rebuilding

Place overrides on the browser host under `<ANTICRUMB_HOME>/providers/<id>/` (default `~/.anticrumb/providers/`):

```text
~/.anticrumb/providers/gemini-web/
  provider.json
  scripts/driver.js
```

Only changed files are needed; a whole provider directory can also be copied or symlinked here.

| Change | Loading behavior |
|---|---|
| DOM scripts | Read on use; no rebuild or restart. Lookup: `scripts/<name>`, then `<name>`, then bundled file |
| `provider.json` | Read at daemon startup; restart after editing. Precedence: external directory, persisted registration, bundled defaults |

Explicit manifest script directories and inline scripts keep their priority. `anticrumb providers add PATH --replace --login` registers a scripted provider from another directory, but stores a manifest snapshot; later source-file edits do not update it.

Gemini keeps its native implementation. Use the startup override directory for its settings; only `inline_prompt_chars` configures built-in Gemini.

## Prompt Attachment Threshold

Set this top-level field in the provider's `provider.json`, then restart the browser daemon:

```json
"inline_prompt_chars": 2000
```

The default is 2,000 Unicode characters. Follow-ups above it become text attachments; an exact-length message stays inline. `0` attaches every nonempty follow-up. The initial prompt is always attached when uploads are supported; providers without uploads use inline text.

For Gemini, copy [gemini-web/provider.json](gemini-web/provider.json) into the override directory before editing. New agent runs, including resumed chats, receive the setting. See [attachment limits](../docs/usage.md#native-attachments-and-images) and the [manifest schema](../src/provider/manifest.rs).
