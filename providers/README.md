# Web Providers

The three providers below are bundled with their manifests and DOM scripts. Claude and ChatGPT are experimental.

| ID | Multi-account | Support |
|---|---|---|
| `gemini-web` | Yes (Google Multi-Login `/u/{slot}/app`) | Native Gemini integration: multi-account discovery, uploads, images, usage, and conversation management |
| `claude-web` | No (single account per Chrome profile) | Model selection, usage, and `--thinking low\|medium\|high\|extra\|max` |
| `chatgpt-web` | No (single account per Chrome profile) | Free Auto/Think (`--thinking standard\|think`) and paid model selection |

## Sign In and Run

```sh
anticrumb login gemini-web      # signs in and discovers all signed-in Google accounts
anticrumb login claude-web
anticrumb --provider claude-web
anticrumb accounts list
anticrumb accounts use alice@gmail.com
anticrumb accounts refresh
```

Use any provider ID above, or `--account TARGET` (account ID, email, slot like `u/1`, or unique email prefix). Each provider uses one Chrome profile (`<ANTICRUMB_HOME>/profiles/<provider_id>/`). In the TUI, `/provider` selects or sets up a provider, and `/account` switches or refreshes signed-in accounts on multi-account providers. For browser window or port controls, see `anticrumb accounts --help`.

Future multi-account scripted providers can enable `capabilities.multi_account = true` in `provider.json` along with `entry_url_template` and `usage_url_template` (supporting `{slot}` substitution).

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
"inline_prompt_chars": 8000
```

The default is 8,000 Unicode characters. Follow-ups above it become text attachments; an exact-length message stays inline. `0` attaches every nonempty follow-up. The initial prompt is always attached when uploads are supported; providers without uploads use inline text.

For Gemini, copy [gemini-web/provider.json](gemini-web/provider.json) into the override directory before editing. New agent runs, including resumed chats, receive the setting. See [attachment limits](../docs/usage.md#native-attachments-and-images) and the [manifest schema](../src/provider/manifest.rs).
