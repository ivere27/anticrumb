# Runtime and RPC

[anticrumb.proto](../proto/anticrumb.proto) defines the shared contract. Local clients use protobuf/gRPC over a Unix socket or Windows named pipe; remote browser calls use TLS gRPC. Services inside the daemon use Synurang FFI.

## Host Ownership

| Command | Behavior |
|---|---|
| `anticrumb` | Opens a TUI and starts or joins the local daemon |
| `anticrumb daemon` | Runs a persistent local daemon without a TCP listener |
| `anticrumb server [IP:PORT]` | Exposes authenticated browser RPC and provider HTTPS APIs (defaults to `0.0.0.0:0`) |
| `anticrumb connect` | Uses the local daemon for workspace tools and a remote server for Chrome |

File, shell, and external MCP tools stay on the workspace machine. Network listeners do not expose `WorkspaceService`. Attachments cross hosts as content-addressed artifacts.

## Local IPC Lifecycle and Recovery

Clients with the same OS user and `ANTICRUMB_HOME` share one daemon across project directories. Local endpoints allow only that user and need no token or TLS.

- An automatic daemon stops after its last IPC client disconnects, including crashed clients. Browser shells and stdio MCP clients count; internal agent workers do not.
- An explicit daemon runs until stopped. Close existing automatic-daemon clients before starting one.
- Startup and lifetime locks prevent duplicate daemons and reclaim stale endpoints after their owner exits.
- `WorkspaceService.ConnectClient` checks the schema revision and binds each connection to its browser backend; reconnects repeat this binding.
- After a protocol mismatch, restart the daemon with the client's binary. Keep profiles and history intact. Use a separate `ANTICRUMB_HOME` in both daemon and clients for isolation.

State under `ANTICRUMB_HOME`:

| Path | Purpose |
|---|---|
| `run/` | Runtime locks, endpoint, `daemon.json`, and `daemon.log` |
| `workspace/` | Local-browser workspace store and saved remote `connection.json` |
| `workspaces/<server-key>/` | Workspace stores for remote browser backends |
| `server/` | Browser-host store; network token and TLS credentials when enabled |

## Runtime Status

`PlatformService.GetStatus` reports local daemon health separately from browser connectivity. Its local IPC `runtime` record includes PID, version, lifetime, endpoint, uptime, and connected clients. Agent and queued/running turn counts describe the selected workspace store; sub-agents remain part of their parent run.

Status does not start Chrome, send prompts, or establish sign-in. See [runtime health and logs](usage.md#runtime-health-and-debugging) for `/status`, `doctor`, and debugging.

## Network gRPC & TLS Setup

Replace the example address with your Chrome host's address:

```sh
# Chrome host
anticrumb server 192.0.2.10:50051 --workers 3

# Workspace machine
anticrumb connect https://192.0.2.10:50051 --token TOKEN_PRINTED_BY_SERVER
```

Compare the displayed certificate fingerprints on first connection. The client saves the endpoint and pinned certificate; later `anticrumb connect` reuses them. Plain `anticrumb` uses the local browser. `connect status` inspects the saved profile; `connect local` removes it.

The server persists a generated token, a long-lived Ed25519 internal Root CA (`ca-certificate.pem`, valid for 5 years), and a short-lived Ed25519 leaf certificate (`certificate.pem`, valid for 3 months and automatically renewed and hot-reloaded 3 days before expiration). Clients pin the internal Root CA so leaf rotation and SAN updates require no client re-trust. `--token` or `ANTICRUMB_TOKEN` overrides the token, `--regenerate-token` rotates the generated token, `--tls-san DNS_OR_IP` adds a hostname/address, `--regenerate-tls` rotates the Root CA and leaf identity, and client `--tls-pin SHA256` supports non-interactive trust setup. Bare `anticrumb server` (or `0.0.0.0:PORT` / `[::]:PORT`) automatically includes the machine's hostname and active matching-family interface IPs in the generated certificate SANs. Even loopback TCP requires TLS and a bearer token. Local IPC remains available.

On Windows, if the firewall prompt was cancelled or virtual VPN adapters are classified under the Public profile, allow inbound connections for a VPN/virtual private range (such as `100.64.0.0/10` or `10.0.0.0/8`) in Administrator PowerShell:

```powershell
$exe = "$env:USERPROFILE\.local\bin\anticrumb.exe"
Get-NetFirewallApplicationFilter -Program $exe -ErrorAction SilentlyContinue | Get-NetFirewallRule | Remove-NetFirewallRule
New-NetFirewallRule -DisplayName 'anticrumb server (VPN)' -Direction Inbound -Program $exe -LocalAddress 100.64.0.0/10 -RemoteAddress 100.64.0.0/10 -Action Allow -Profile Any
```

## Key RPC Behaviors

- **Catalog:** `GetCatalog` reads cached SQLite data; `RefreshCatalog` discovers models through Chrome.
- **Submission:** `CreateTurn` records an idempotency key before sending. A disconnect after possible submission returns `submission_outcome_unknown` and is never retried automatically.
- **Verification:** `awaiting_human` retains the sign-in or CAPTCHA tab. Resolve it on the Chrome host, then run `/browser refresh` or `/model refresh`.
- **Browser actions:** Commands are typed, validated, serialized per session, and never replayed automatically. See the [browser interface](tools.md#browser-api); direct RPC callers must supply valid bounds from the schema.

## Code Generation (`make proto`)

`make proto` runs the pinned Synurang generator and updates [generated bindings](../src/rpc/generated/). [build.rs](../build.rs) checks the schema hash and rejects stale output.
