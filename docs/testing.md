# Testing and Verification

## Automated Checks

Run from the repository root; these checks need no provider login:

```sh
make check
make test
make test-transports PROFILE=dev
make test-tui PROFILE=dev       # Linux / macOS
make package
```

`make check` runs formatting and Clippy. `make test` runs all workspace targets, including installer/publisher contracts. Transport and TUI commands exercise the executable with local fixtures.

| Area | Tests |
|---|---|
| Agent, tools, and persistence | [agent/tests.rs](../src/agent/tests.rs), [service_contract.rs](../tests/service_contract.rs) |
| Local daemon and connections | [local_daemon_contract.rs](../tests/local_daemon_contract.rs), [connection_contract.rs](../tests/connection_contract.rs) |
| RPC and gateway | [rpc_contract.rs](../tests/rpc_contract.rs), [daemon_gateway_contract.rs](../tests/daemon_gateway_contract.rs) |
| Terminal UI | [tui_contract.rs](../tests/tui_contract.rs) |
| Packaging and installers | [xtask/tests/](../xtask/tests/) |

Run transport/platform checks natively: cross compilation does not verify runtime behavior. [Native CI](../.github/workflows/build.yml) covers Linux, Windows, and macOS. For manual debugging, see [runtime health](usage.md#runtime-health-and-debugging).

## Browser Fixtures

These opt-in tests need Chrome but do not send model prompts or delete account conversations.

The browser-control test launches a temporary Chrome profile and a local page; set `CHROME_BIN` if needed:

```sh
cargo test --locked --test browser_control_contract -- --ignored --nocapture
```

To diagnose a public page without sending a model prompt, run `cargo run --locked --example browser_control_diagnostic -- https://example.com`. It opens an isolated browser, reports each command result, and saves a PNG in the temporary directory. The diagnostic uses the default public browsing policy and closes its tabs when finished.

Provider DOM fixtures use Chrome already running with `--remote-debugging-port=9222`:

```sh
ANTICRUMB_TEST_PORT=9222 cargo test --locked --test browser_contract -- --ignored
ANTICRUMB_TEST_PORT=9222 cargo test --locked --test browser_network_contract -- --ignored --test-threads=1
```

Other fixtures using `ANTICRUMB_TEST_PORT`: `browser_usage_contract`, `browser_delete_contract`, `browser_history_contract`, and `browser_media_contract`. DOM fixtures use synthetic pages; network checks also access a public example page.

## Live Provider Tests

These tests send real prompts using an already signed-in Chrome account. They create test conversations, close only their own tabs, and leave saved web conversations intact. Ordinary test runs skip them.

```sh
ANTICRUMB_LIVE_PORT=9222 cargo test --locked --test live_contract -- --ignored --nocapture
ANTICRUMB_LIVE_PORT=9222 cargo test --locked --test large_input_contract -- --ignored --nocapture
```

The first checks responses, follow-up context, and concurrent workers; the second checks a large multiline prompt. Optional environment variables:

| Variable | Purpose |
|---|---|
| `ANTICRUMB_LIVE_IMAGES=1` | Also test image generation and recovery after tab reuse |
| `ANTICRUMB_LIVE_MODEL` | Select a model from the account's current menu |
| `ANTICRUMB_LIVE_OUTPUT` | Retain image artifacts for inspection |

Exact assertions and fixture setup live in the [test sources](../tests/).
