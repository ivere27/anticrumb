//! Local-service commands. Configuration stays local; Google credentials stay in Chrome.
use crate::{domain::*, store::Store};
use fs2::FileExt;
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

pub fn app_dir() -> PathBuf {
    crate::browser_host::anticrumb_home()
}
pub fn root() -> PathBuf {
    crate::rpc::local::workspace_root(&crate::rpc::workspace_client::selected_connection())
}
pub(crate) fn config_root() -> PathBuf {
    app_dir().join("workspace")
}
pub(crate) fn token(root: &Path) -> Result<String> {
    token_with_regenerate(root, false)
}
pub(crate) fn regenerate_token(root: &Path) -> Result<String> {
    token_with_regenerate(root, true)
}
fn token_with_regenerate(root: &Path, regenerate: bool) -> Result<String> {
    fs::create_dir_all(root).map_err(ServiceError::storage)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))
            .map_err(ServiceError::storage)?;
    }
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join("token.lock"))
        .map_err(ServiceError::storage)?;
    lock.lock_exclusive().map_err(ServiceError::storage)?;
    let path = root.join("token");
    if regenerate || !path.exists() {
        let secret = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        crate::rpc::private_write(&path, secret.as_bytes())?;
    }
    let secret = fs::read_to_string(path).map_err(ServiceError::storage)?;
    if secret.len() != 64 {
        return Err(ServiceError::invalid("local token file is invalid"));
    }
    Ok(secret)
}

pub(crate) fn poll_until<T>(
    interval: Duration,
    cancel: Option<&crate::agent::CancelToken>,
    mut step: impl FnMut() -> Result<std::ops::ControlFlow<T, ()>>,
) -> Result<T> {
    loop {
        if let std::ops::ControlFlow::Break(value) = step()? {
            return Ok(value);
        }
        match cancel {
            Some(token) => {
                token.sleep_or_cancelled(interval);
            }
            None => std::thread::sleep(interval),
        }
    }
}

pub use crate::remote_backend::RemoteBackend;
pub use crate::rpc::workspace_client::Client;
impl Client {
    pub fn clear_web_history(&self, account: &str) -> Result<Value> {
        self.runtime.block_on(self.clear_web_history_async(account))
    }
    pub fn web_history_cleanup(&self, account: &str) -> Result<Value> {
        self.runtime
            .block_on(self.web_history_cleanup_async(account))
    }
    pub fn submit(&self, request: &TurnRequest) -> Result<String> {
        self.create_turn(request)
    }
    pub fn wait(&self, id: &str, cancel: &crate::agent::CancelToken) -> Result<TurnResult> {
        let mut cancellation_sent = false;
        poll_until(Duration::from_millis(100), Some(cancel), || {
            if cancel.is_cancelled() && !cancellation_sent {
                self.cancel_turn(id)?;
                cancellation_sent = true;
            }
            let run = self.get_turn(id)?;
            if let Some(error) = run.error {
                return Err(error);
            }
            if let Some(result) = run.result {
                return Ok(std::ops::ControlFlow::Break(result));
            }
            Ok(std::ops::ControlFlow::Continue(()))
        })
    }
}

fn is_env_truthy(var: &str) -> bool {
    std::env::var(var)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn non_empty_env(var: &str) -> Option<String> {
    std::env::var(var)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(unix)]
fn os_hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    let res = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if res == 0 {
        let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        std::str::from_utf8(&buf[..len])
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    } else {
        None
    }
}

#[cfg(not(unix))]
fn os_hostname() -> Option<String> {
    None
}

pub(crate) fn local_hostname() -> Option<String> {
    non_empty_env("HOSTNAME")
        .or_else(|| non_empty_env("COMPUTERNAME"))
        .or_else(os_hostname)
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .or_else(|| {
            std::fs::read_to_string("/proc/sys/kernel/hostname")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
}

pub(crate) fn client_identity() -> String {
    if is_env_truthy("ANTICRUMB_ANONYMOUS_CLIENT") || is_env_truthy("DO_NOT_TRACK") {
        return format!("anonymous ({})", std::env::consts::OS);
    }
    let user = non_empty_env("USER")
        .or_else(|| non_empty_env("USERNAME"))
        .unwrap_or_else(|| "user".into());
    let host = local_hostname().unwrap_or_else(|| "host".into());
    let os = std::env::consts::OS;
    let raw = format!("{user}@{host} ({os})");
    raw.chars()
        .map(|c| {
            if c.is_ascii() && !c.is_ascii_control() {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub fn run(args: &[String]) -> Result<()> {
    crate::update::cleanup_old_executables();
    if let Some((connection, args)) = crate::rpc::connection::inline(args)? {
        return crate::rpc::workspace_client::with_connection(connection, || run(&args));
    }
    let command = args.first().map(String::as_str).unwrap_or("");
    match command {
        "--version" | "-V" => {
            println!("{}", crate::build_info::cli_version());
            Ok(())
        }
        "update" => crate::update::run(&args[1..]),
        "help" | "--help" | "-h" => {
            println!("{}\n", crate::build_info::help_banner());
            println!(
                r#"anticrumb [OPTIONS] [PROMPT]              interactive coding agent
  exec [OPTIONS] PROMPT                    run a task and print the answer
  resume [ID | --last]                     continue a conversation
  exec resume [ID | --last] PROMPT         follow up without the TUI

  -m, --model ID      -C, --cd DIR          model and working directory
  --agent             --yolo                agent mode (respects /permissions) or yolo (all permissions)
  --temp                                    anonymous temporary chat for this run
  -p, --print                              noninteractive final answer
  -c, --continue     -r, --resume ID        reopen a conversation
  --file PATH        -i, --image PATH       native Gemini attachment (repeatable)
  --json                                   NDJSON events for exec/--print
  --thinking LEVEL --pace LEVEL --no-shell --mcp-config FILE --knowledge ID
  --provider ID --account ID --workers N
  --connect https://HOST:PORT --token STRING [--tls-pin SHA256]

Type @path or @"path with spaces" to upload a file, never inline its contents.
Interactive: /help /mode /temp /provider /model /usage /copy /theme /browser /browse /status /resume /new /clear /permissions /pace /prompts /exit
Enter sends; Esc interrupts; Tab completes; Alt+Enter inserts a newline.

Service and integration commands:
  update [--check] [--nightly] [--force]  install or check stable or nightly releases
  daemon [OPTIONS]                         persistent local IPC daemon (see daemon --help)
  server [IP:PORT] [OPTIONS]               network browser RPC and HTTPS /v1 server (see server --help)
  browser [OPTIONS] [COMMAND]               stateful browser commands / interactive shell
  chat [--session ID] [--file PATH] PROMPT
  image [--file PATH] PROMPT
  agents list|show|cancel|steer|resume|follow ID
  runs list|show|cancel ID     sessions list|show|close|media ID
  login [ACCOUNT|PROVIDER] [--provider ID] sign in (auto-creates account & port if needed)
  providers list|show ID|add DIR_OR_FILE [--replace] [--login]|remove ID
  accounts list|add [ID] [PORT] [--provider ID] [--login]|attach ID PORT|show|hide|login|resolve|pause ID
  models [--refresh] [--account ID]     doctor [--live]
  usage [--account ID]                     web account limits and reset times
  knowledge add ID PATH [NOTEBOOK_URL]|sync ID|list|query TEXT
  artifact get ACCOUNT ID DEST
  connect opencode|codex|claude|antigravity|mcp [--install PATH]
  connect [https://HOST:PORT --token STRING] trust and open a network TUI
  connect status | connect local            inspect or forget the saved connection
  cache [stats|prune [DAYS] [--dry-run]] (default: 7 days)     mcp (stdio server)

Default: automatic local daemon over a Unix socket or named pipe. Remote: server / connect.
Home: ANTICRUMB_HOME (default ~/.anticrumb: profile/, accounts/, workspace/, server/, prompts/, providers/, AGENTS.md)"#
            );
            Ok(())
        }
        "daemon" => crate::rpc::daemon::run(&args[1..]),
        "server" => crate::rpc::server_cmd::run(&args[1..]),
        "browser" => crate::browser_control::terminal::run(&args[1..]),
        "mcp" => {
            let client = Client::connect()?;
            let result = client
                .runtime
                .block_on(crate::mcp_sdk::serve(client.rpc.clone()));
            client.close()?;
            result
        }
        "resume" => launch_resume(&args[1..], false),
        "connect"
            if matches!(
                args.get(1).map(String::as_str),
                Some("--help" | "-h" | "help")
            ) =>
        {
            println!("anticrumb connect https://HOST:PORT --token STRING\n  First connection: compare the SHA256 and emoji fingerprint with the server, then confirm.\n  --tls-pin SHA256    preverified certificate for noninteractive setup\n  --ca-file PATH      use an existing trusted CA\n  --server-name NAME override the expected TLS hostname\nanticrumb connect status | local\nanticrumb --connect URL --token STRING [TUI options]\nProvider integrations: connect opencode|codex|claude|antigravity|mcp");
            Ok(())
        }
        "connect" if crate::rpc::connection::handles(&args[1..]) => {
            if let Some(connection) = crate::rpc::connection::command(&args[1..])? {
                crate::rpc::workspace_client::with_connection(connection, || run(&[]))
            } else {
                Ok(())
            }
        }
        "connect" => connect_config(&args[1..]),
        "login" => login_command(&args[1..]),
        "providers" => provider_command(&args[1..]),
        "accounts" => account_command(&args[1..]),
        "knowledge" => knowledge_command(&args[1..]),
        "cache" => {
            if matches!(
                args.get(1).map(String::as_str),
                Some("--help" | "-h" | "help")
            ) {
                println!("{}", crate::store::cache::USAGE);
                return Ok(());
            }
            let options = crate::store::cache::Options::parse(
                &args[1..].iter().map(String::as_str).collect::<Vec<_>>(),
            )?;
            let client = Client::connect()?;
            let report = client.cache(options);
            client.close()?;
            let report = report?;
            print_json(&json!(report));
            if report.stores.iter().any(|store| !store.error.is_empty()) {
                return Err(ServiceError::storage(
                    "cache maintenance was incomplete; see each store's error",
                ));
            }
            Ok(())
        }
        "usage" => {
            let mut options = PromptOptions::parse(&args[1..])?;
            if !options.words.is_empty() {
                return Err(ServiceError::invalid(
                    "usage [--provider ID] [--account ID]",
                ));
            }
            let client = Client::connect()?;
            options.resolve_account(&client)?;
            print_json(&client.request(
                "GET",
                &format!("/api/accounts/{}/usage", options.account),
                None,
            )?);
            Ok(())
        }
        "doctor" | "models" => {
            let mut options = PromptOptions::parse(&args[1..])?;
            let refresh = options
                .words
                .iter()
                .any(|s| s == "--live" || s == "--refresh");
            if command == "models" && !refresh && options.provider.is_none() {
                print_json(&Store::open(&root())?.model_catalog(&options.account)?);
                return Ok(());
            }
            let client = Client::connect()?;
            options.resolve_account(&client)?;
            if command == "models" && !refresh {
                print_json(&client.request(
                    "GET",
                    &format!("/api/accounts/{}/models", options.account),
                    None,
                )?);
            } else if refresh {
                print_json(&client.request(
                    "POST",
                    &format!("/api/accounts/{}/resolve", options.account),
                    Some(&json!({})),
                )?);
            } else {
                print_json(&client.request("GET", "/api/status", None)?);
            }
            Ok(())
        }
        "exec" => {
            if args.get(1).is_some_and(|s| s == "resume") {
                launch_resume(&args[2..], true)
            } else {
                run_agent(PromptOptions::parse(&args[1..])?)
            }
        }
        "chat" | "image" => chat_command(&args[1..], command == "image"),
        "agents" | "runs" | "sessions" => job_command(command, &args[1..]),
        "artifact" => {
            if args.len() != 5 || args[1] != "get" {
                return Err(ServiceError::invalid("artifact get ACCOUNT ID DEST"));
            }
            let client = Client::connect()?;
            client.download_artifact(
                identifier(&args[2])?,
                identifier(&args[3])?,
                Path::new(&args[4]),
            )?;
            Ok(())
        }
        _ => {
            let options = PromptOptions::parse(args)?;
            if options.print {
                run_agent(options)
            } else {
                crate::tui::run(options)
            }
        }
    }
}
fn print_json(value: &impl serde::Serialize) {
    println!("{}", serde_json::to_string_pretty(value).unwrap());
}
fn identifier(value: &str) -> Result<&str> {
    if value.is_empty()
        || !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        Err(ServiceError::invalid("invalid identifier"))
    } else {
        Ok(value)
    }
}
#[derive(Default, Clone)]
pub struct PromptOptions {
    pub(crate) account: String,
    pub(crate) provider: Option<String>,
    pub(crate) provider_name: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) thinking: Option<String>,
    pub(crate) session: Option<String>,
    pub(crate) temporary: bool,
    pub(crate) keep: bool,
    pub(crate) files: Vec<PathBuf>,
    pub(crate) workspace: PathBuf,
    pub(crate) agent: bool,
    pub(crate) yolo: bool,
    pub(crate) no_shell: bool,
    pub(crate) permissions: ToolPermissions,
    pub(crate) timeout: u64,
    pub(crate) knowledge: Option<String>,
    pub(crate) mcp: Option<PathBuf>,
    pub(crate) words: Vec<String>,
    pub(crate) print: bool,
    pub(crate) json: bool,
    pub(crate) resume: Option<String>,
    pub(crate) workers: Option<usize>,
    pub(crate) pace: Option<crate::gemini::HumanPace>,
    pub(crate) overrides: std::collections::HashSet<String>,
}
impl PromptOptions {
    pub fn parse(args: &[String]) -> Result<Self> {
        let mut o = Self {
            account: "default".into(),
            workspace: std::env::current_dir().map_err(ServiceError::storage)?,
            no_shell: true,
            permissions: ToolPermissions::read_only(),
            timeout: 120,
            ..Default::default()
        };
        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            let field = match arg.as_str() {
                "-m" | "--model" => Some("model"),
                "--thinking" => Some("thinking"),
                "-C" | "--cd" => Some("workspace"),
                "--provider" => Some("provider"),
                "--account" => Some("account"),
                "--no-shell" => Some("no_shell"),
                "--mcp-config" => Some("mcp"),
                "--knowledge" => Some("knowledge"),
                _ => None,
            };
            if let Some(field) = field {
                o.overrides.insert(field.into());
            }
            match arg.as_str() {
                "--provider" => {
                    o.provider = Some(
                        identifier(
                            iter.next()
                                .ok_or_else(|| ServiceError::invalid("missing provider ID"))?,
                        )?
                        .into(),
                    )
                }
                "--account" => {
                    o.account = identifier(
                        iter.next()
                            .ok_or_else(|| ServiceError::invalid("missing account"))?,
                    )?
                    .into()
                }
                "-m" | "--model" => {
                    o.model = Some(
                        iter.next()
                            .ok_or_else(|| ServiceError::invalid("missing model"))?
                            .into(),
                    )
                }
                "--thinking" => {
                    o.thinking = Some(
                        iter.next()
                            .ok_or_else(|| ServiceError::invalid("missing thinking level"))?
                            .into(),
                    )
                }
                "--session" => {
                    o.session = Some(
                        identifier(
                            iter.next()
                                .ok_or_else(|| ServiceError::invalid("missing session"))?,
                        )?
                        .into(),
                    )
                }
                "--print" | "-p" => o.print = true,
                "--json" => o.json = true,
                "--continue" | "-c" | "--last" => o.resume = Some("last".into()),
                "--resume" | "-r" => {
                    o.resume = Some(
                        identifier(
                            iter.next()
                                .ok_or_else(|| ServiceError::invalid("missing conversation ID"))?,
                        )?
                        .into(),
                    )
                }
                "--keep-session" => o.keep = true,
                "--detach" => return Err(ServiceError::invalid("automatic daemons stop after their last client exits; keep the command running until the task finishes")),
                "--yolo" => {
                    o.agent = true;
                    o.yolo = true;
                    o.no_shell = false;
                    o.permissions = ToolPermissions::all();
                    o.overrides.insert("agent".into());
                    o.overrides.insert("no_shell".into());
                    o.overrides.insert("permissions".into());
                }
                "--agent" => {
                    o.agent = true;
                    o.yolo = false;
                    o.overrides.insert("agent".into());
                }
                "--chat" => {
                    o.agent = false;
                    o.yolo = false;
                    o.overrides.insert("agent".into());
                }
                "--temp" | "--temporary" => {
                    o.temporary = true;
                    o.overrides.insert("temporary".into());
                }
                "--no-shell" => {
                    o.no_shell = true;
                    o.permissions.shell = false;
                    o.yolo = false;
                }
                "--file" | "-i" | "--image" => o.files.push(PathBuf::from(
                    iter.next()
                        .ok_or_else(|| ServiceError::invalid("missing file"))?,
                )),
                "-C" | "--cd" => {
                    o.workspace = PathBuf::from(
                        iter.next()
                            .ok_or_else(|| ServiceError::invalid("missing workspace"))?,
                    )
                }
                "--timeout" => {
                    o.timeout = iter
                        .next()
                        .ok_or_else(|| ServiceError::invalid("missing timeout"))?
                        .parse()
                        .map_err(|_| ServiceError::invalid("invalid timeout"))?
                }
                "--workers" => {
                    let workers: usize = iter
                        .next()
                        .ok_or_else(|| ServiceError::invalid("missing worker count"))?
                        .parse()
                        .map_err(|_| ServiceError::invalid("--workers must be 1..16"))?;
                    if !(1..=16).contains(&workers) {
                        return Err(ServiceError::invalid("--workers must be 1..16"));
                    }
                    o.workers = Some(workers);
                }
                "--pace" => {
                    let raw = iter
                        .next()
                        .ok_or_else(|| ServiceError::invalid("missing pace level (fast|low|high)"))?;
                    let pace = crate::gemini::HumanPace::parse(raw)
                        .ok_or_else(|| ServiceError::invalid("--pace must be fast, low, or high"))?;
                    o.pace = Some(pace);
                }
                "--knowledge" => {
                    o.knowledge = Some(
                        iter.next()
                            .ok_or_else(|| ServiceError::invalid("missing source"))?
                            .into(),
                    )
                }
                "--mcp-config" => {
                    o.mcp = Some(PathBuf::from(
                        iter.next()
                            .ok_or_else(|| ServiceError::invalid("missing config"))?,
                    ))
                }
                "--live" | "--refresh" => o.words.push(arg.clone()),
                "--" => {
                    o.words.extend(iter.cloned());
                    break;
                }
                "-" => {
                    use std::io::Read;
                    let mut text = String::new();
                    std::io::stdin()
                        .take(48001)
                        .read_to_string(&mut text)
                        .map_err(ServiceError::storage)?;
                    if text.len() > 48000 {
                        return Err(ServiceError::invalid("stdin prompt exceeds 48000 bytes"));
                    }
                    o.words.push(text);
                }
                s if s.starts_with('-') => {
                    return Err(ServiceError::invalid(format!("unknown option {s}")))
                }
                _ => o.words.push(arg.clone()),
            }
        }
        o.workspace = o.workspace.canonicalize().map_err(ServiceError::storage)?;
        if let Some(path) = &o.mcp {
            o.mcp = Some(path.canonicalize().map_err(ServiceError::storage)?);
        }
        if !o.workspace.is_dir() {
            return Err(ServiceError::invalid(
                "working directory must be a directory",
            ));
        }
        Ok(o)
    }
    pub(crate) fn inherit(&mut self, request: &crate::agent_service::AgentRequest) -> Result<()> {
        if self.overrides.contains("workspace") && self.workspace != request.workspace {
            return Err(ServiceError::invalid("resume requires the original working directory; omit -C or start a new conversation"));
        }
        if (self.overrides.contains("account") || self.overrides.contains("provider"))
            && self.account != request.account_id
        {
            return Err(ServiceError::invalid(
                "resume requires the original account",
            ));
        }
        self.workspace = request.workspace.clone();
        self.account = request.account_id.clone();
        if !self.overrides.contains("model") {
            self.model = request.model.clone();
        }
        if !self.overrides.contains("thinking") {
            self.thinking = request.thinking.clone();
        }
        if !self.overrides.contains("agent") {
            self.agent = !request.chat_only;
        }
        if !self.overrides.contains("permissions") {
            if let Some(perms) = request.permissions {
                self.permissions = perms;
                self.no_shell = !perms.shell;
            }
        }
        if !self.overrides.contains("no_shell") {
            self.no_shell = request.no_shell;
        }
        // Always inherit [temp] from the resumed job: resuming a `[temp]` conversation must stay
        // temporary regardless of the current `self.temporary` state or overrides.  There is no
        // `--no-temp` flag, so the override can only mean `--temp` (which already sets `true`).
        // The `||` preserves active/saved `/temp on` when resuming a normal job so server history
        // is never silently written.
        self.temporary = self.temporary || request.is_temporary();
        if !self.overrides.contains("mcp") {
            self.mcp = request.mcp_config.clone();
        }
        if !self.overrides.contains("knowledge") {
            self.knowledge = request.knowledge.clone();
        }
        Ok(())
    }
    pub(crate) fn apply_worker_override(&self) {
        if self.workers.is_none() && self.pace.is_none() {
            return;
        }
        let store = Store::open(&root()).ok();
        if let Some(workers) = self.workers {
            crate::tab_pool::set_worker_tabs(store.as_ref(), workers);
        }
        if let Some(pace) = self.pace {
            crate::gemini::set_human_pace(store.as_ref(), pace);
        }
    }
    pub(crate) fn inherit_from_job(&mut self, job: &Value) -> Result<()> {
        let request =
            serde_json::from_value(job["request"].clone()).map_err(ServiceError::storage)?;
        self.inherit(&request)
    }
    pub(crate) fn resolve_account(&mut self, client: &Client) -> Result<()> {
        self.resolve_or_create_account(client, false).map(|_| ())
    }
    pub(crate) fn resolve_or_create_account(
        &mut self,
        client: &Client,
        auto_create: bool,
    ) -> Result<bool> {
        let accounts_val = match client.request("GET", "/api/accounts", None) {
            Ok(v) => v,
            Err(e) if self.overrides.contains("provider") => return Err(e),
            Err(_) => return Ok(false),
        };
        let providers_val = client
            .request("GET", "/api/providers", None)
            .unwrap_or_else(|_| json!([]));
        let accounts = accounts_val.as_array().map(Vec::as_slice).unwrap_or(&[]);
        let providers = providers_val.as_array().map(Vec::as_slice).unwrap_or(&[]);
        match resolve_account_from_lists(
            &mut self.account,
            &mut self.provider,
            &mut self.provider_name,
            self.overrides.contains("account"),
            self.overrides.contains("provider"),
            accounts,
            providers,
        ) {
            Ok(()) => Ok(false),
            Err(err)
                if auto_create
                    && err.code == ErrorCode::NotFound
                    && self.overrides.contains("provider")
                    && !self.overrides.contains("account") =>
            {
                let requested = self.provider.clone().unwrap_or_default();
                let created =
                    create_account_on_client(client, None, 0, Some(&requested), accounts)?;
                self.account = created["id"]
                    .as_str()
                    .ok_or_else(|| ServiceError::invalid("created account missing id"))?
                    .to_owned();
                self.provider_name = providers
                    .iter()
                    .find(|p| p["id"].as_str() == Some(requested.as_str()))
                    .and_then(|p| p["name"].as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
                Ok(true)
            }
            Err(err) => Err(err),
        }
    }

    pub(crate) fn load_saved_permissions(&mut self, root: &Path) {
        if self.yolo {
            self.permissions = ToolPermissions::all();
            self.no_shell = false;
            return;
        }
        if let Ok(prefs) = crate::tui::preferences::Preferences::open(root) {
            if !self.overrides.contains("permissions") {
                if let Some(mut perms) = prefs.permissions() {
                    if self.overrides.contains("no_shell") {
                        perms.shell = !self.no_shell;
                    }
                    self.no_shell = !perms.shell;
                    self.permissions = perms;
                    return;
                }
            }
            if !self.overrides.contains("no_shell") {
                if let Some(no_shell) = prefs.no_shell() {
                    self.no_shell = no_shell;
                    self.permissions.shell = !no_shell;
                }
            }
        }
    }

    pub(crate) fn load_saved_temporary(&mut self, root: &Path) {
        if !self.overrides.contains("temporary") {
            if let Ok(prefs) = crate::tui::preferences::Preferences::open(root) {
                self.temporary = prefs.temporary();
            }
        }
    }

    /// Resolve `temporary` mode for low-level `chat`/`image` commands.
    ///
    /// Unlike `run_agent` (`--resume`), which can branch a normal parent conversation into a new
    /// temporary turn using local context when saved `/temp on` is active, `chat --session <ID>`
    /// targets a specific browser session whose `SessionMode` is fixed on the server
    /// (`Service::submit` rejects mode mismatches). When `--session` is given without `--temp`, we
    /// intentionally skip saved `/temp on` and adopt the target session's recorded mode instead.
    pub(crate) fn resolve_chat_temporary(
        &mut self,
        root: &Path,
        fetch_session: impl FnOnce(&str) -> Result<crate::rpc::pb::Session>,
    ) {
        if let Some(sid) = self.session.as_deref() {
            if !self.overrides.contains("temporary") {
                if let Ok(session) = fetch_session(sid) {
                    if session_is_temporary(&session) == Some(true) {
                        self.temporary = true;
                    }
                }
            }
        } else {
            self.load_saved_temporary(root);
        }
    }

    pub(crate) fn effective_permissions(&self) -> ToolPermissions {
        if self.yolo {
            ToolPermissions::all()
        } else {
            let mut perms = self.permissions;
            if self.no_shell {
                perms.shell = false;
            }
            perms
        }
    }

    pub(crate) fn agent_request(
        &self,
        task: String,
        parent_id: Option<String>,
    ) -> crate::agent_service::AgentRequest {
        let perms = self.effective_permissions();
        crate::agent_service::AgentRequest {
            environment: None,
            task,
            workspace: self.workspace.clone(),
            account_id: self.account.clone(),
            model: self.model.clone(),
            thinking: self.thinking.clone(),
            session_mode: SessionMode::from_temporary(self.temporary),
            no_shell: !perms.shell,
            max_steps: 0,
            knowledge: self.knowledge.clone(),
            mcp_config: self.mcp.clone(),
            artifacts: Vec::new(),
            parent_id,
            chat_only: !self.agent,
            permissions: Some(perms),
            session_retention: SessionRetention::Unspecified,
        }
    }
}
fn launch_resume(args: &[String], print: bool) -> Result<()> {
    let mut args = args.to_vec();
    let explicit = if args.first().is_some_and(|a| !a.starts_with('-')) {
        Some(args.remove(0))
    } else {
        None
    };
    let mut options = PromptOptions::parse(&args)?;
    if let Some(id) = explicit {
        options.resume = Some(identifier(&id)?.into());
    }
    if options.resume.is_none() {
        options.resume = Some(if print { "last" } else { "" }.into());
    }
    if print {
        run_agent(options)
    } else {
        crate::tui::run(options)
    }
}
fn with_temporary_unsupported_hint(
    err: ServiceError,
    temporary: bool,
    cli_temp_override: bool,
) -> ServiceError {
    if temporary {
        let lower = err.message.to_ascii_lowercase();
        let is_temp_unsupported = lower.contains("temporary")
            && (err.code == ErrorCode::UnsupportedCapability
                || lower.contains("does not support temporary sessions")
                || lower.contains("temporary chat control unavailable")
                || lower.contains("unsupported capability"));
        if is_temp_unsupported {
            let mut enriched = err;
            if cli_temp_override {
                enriched.message.push_str(
                    "\nHint: omit `--temp` or select a provider that supports temporary sessions.",
                );
            } else {
                enriched.message.push_str(
                    "\nHint: run `/temp off` in the TUI to disable saved temporary mode, or select a provider that supports temporary sessions.",
                );
            }
            return enriched;
        }
    }
    err
}
fn run_agent(mut o: PromptOptions) -> Result<()> {
    o.apply_worker_override();
    o.load_saved_permissions(&root());
    o.load_saved_temporary(&root());
    let client = Client::connect()?;
    o.resolve_account(&client)?;
    let parent = if let Some(id) = &o.resume {
        let job = client.conversation(id, &o.workspace, &o.account)?;
        o.inherit_from_job(&job)?;
        Some(
            job["id"]
                .as_str()
                .ok_or_else(|| ServiceError::invalid("missing conversation ID"))?
                .to_owned(),
        )
    } else {
        None
    };
    let prepared = crate::input::prepare(&o.words.join(" "), &o.workspace, &o.files)?;
    for warning in &prepared.warnings {
        eprintln!("warning: {warning}");
    }
    if prepared.text.trim().is_empty() {
        return Err(ServiceError::invalid("provide a task or follow-up prompt"));
    }
    let cli_temp_override = o.overrides.contains("temporary");
    let mut request = o.agent_request(prepared.text, parent);
    request.artifacts = client.upload_files(&o.account, &prepared.files, &prepared.temp_files)?;
    let value = client
        .request(
            "POST",
            "/api/agents",
            Some(&serde_json::to_value(request).map_err(ServiceError::storage)?),
        )
        .map_err(|err| with_temporary_unsupported_hint(err, o.temporary, cli_temp_override))?;
    let id = value["id"]
        .as_str()
        .ok_or_else(|| ServiceError::invalid("missing job id"))?;
    let mode_desc = if !o.agent {
        "chat".to_owned()
    } else if o.yolo {
        "agent (--yolo, all permissions)".to_owned()
    } else {
        format!(
            "agent (permissions: {})",
            o.effective_permissions().summary()
        )
    };
    let mode_desc = if o.temporary {
        format!("{mode_desc}, temporary")
    } else {
        mode_desc
    };
    eprintln!("mode: {mode_desc} | agent: {id}");
    follow_agent(&client, id, o.json, o.print)
        .map_err(|err| with_temporary_unsupported_hint(err, o.temporary, cli_temp_override))
}
fn follow_agent(client: &Client, id: &str, json: bool, quiet: bool) -> Result<()> {
    let cancel = crate::agent::CancelToken::new();
    let c = cancel.clone();
    let _ = ctrlc::set_handler(move || c.cancel());
    let mut after = 0;
    let mut cancelled = false;
    poll_until(Duration::from_millis(25), Some(&cancel), || {
        if cancel.is_cancelled() && !cancelled {
            let _ = client.request(
                "POST",
                &format!("/api/agents/{id}/cancel"),
                Some(&json!({})),
            );
            cancelled = true;
        }
        for event in client.events(id, after)? {
            after = event.sequence;
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&event).map_err(ServiceError::storage)?
                );
            }
            if quiet || json {
                continue;
            }
            match event.kind.as_str() {
                "tool_started" => {
                    eprintln!("\n[{}]", event.data["tool"].as_str().unwrap_or("tool"))
                }
                "subagent_started" => {
                    eprintln!(
                        "\n[worker: {}]",
                        event.data["label"].as_str().unwrap_or("subagent")
                    )
                }
                "step_started" => eprintln!("\nstep {}", event.data["step"]),
                _ => {}
            }
        }
        let job = client.request("GET", &format!("/api/agents/{id}"), None)?;
        if job["state"] != "running" {
            if json {
                println!("{}", json!({"event":"result","job":job}));
            } else if let Some(answer) = job["result"]["final_response"].as_str() {
                println!("{answer}");
            }
            return if job["state"] != "completed" {
                Err(ServiceError::new(
                    ErrorCode::Internal,
                    "agent",
                    format!("agent ended as {}: {}", job["state"], job["error"]),
                ))
            } else {
                Ok(std::ops::ControlFlow::Break(()))
            };
        }
        Ok(std::ops::ControlFlow::Continue(()))
    })
}
fn session_is_temporary(session: &crate::rpc::pb::Session) -> Option<bool> {
    match crate::rpc::pb::SessionMode::try_from(session.mode).ok()? {
        crate::rpc::pb::SessionMode::Temporary => Some(true),
        crate::rpc::pb::SessionMode::Chat => Some(false),
        crate::rpc::pb::SessionMode::Unspecified => None,
    }
}
fn chat_command(args: &[String], image: bool) -> Result<()> {
    let mut o = PromptOptions::parse(args)?;
    let client = Client::connect()?;
    o.resolve_account(&client)?;
    o.resolve_chat_temporary(&root(), |sid| client.get_session(sid));
    let cli_temp_override = o.overrides.contains("temporary");
    let prepared = crate::input::prepare(&o.words.join(" "), &o.workspace, &o.files)?;
    for warning in &prepared.warnings {
        eprintln!("warning: {warning}");
    }
    let mut r = TurnRequest::text(prepared.text);
    r.account_id = o.account;
    r.model = o.model;
    r.thinking = o.thinking;
    r.mode = SessionMode::from_temporary(o.temporary);
    r.session_id = o.session;
    r.retention = SessionRetention::for_turn(o.keep);
    r.timeout_secs = o.timeout;
    r.generate_image = image;
    for path in prepared.files {
        r.artifacts.push(client.upload(&r.account_id, &path)?);
    }
    let id = client
        .submit(&r)
        .map_err(|err| with_temporary_unsupported_hint(err, o.temporary, cli_temp_override))?;
    eprintln!("run: {id}");
    let cancel = crate::agent::CancelToken::new();
    let c = cancel.clone();
    let _ = ctrlc::set_handler(move || c.cancel());
    let result = client
        .wait(&id, &cancel)
        .map_err(|err| with_temporary_unsupported_hint(err, o.temporary, cli_temp_override))?;
    print_json(&result);
    Ok(())
}
fn job_command(kind: &str, args: &[String]) -> Result<()> {
    let client = Client::connect()?;
    let action = args.first().map(String::as_str).unwrap_or("list");
    let id = if action == "list" {
        ""
    } else {
        identifier(
            args.get(1)
                .ok_or_else(|| ServiceError::invalid("missing ID"))?,
        )?
    };
    let value = match action {
        "list" => client.request("GET", &format!("/api/{kind}"), None)?,
        "show" => client.request("GET", &format!("/api/{kind}/{id}"), None)?,
        "log" => json!(client.events(id, 0)?),
        "cancel" | "close" | "resume" | "media" => client.request(
            "POST",
            &format!("/api/{kind}/{id}/{action}"),
            Some(&json!({})),
        )?,
        "steer" => {
            let job = client.request("GET", &format!("/api/agents/{id}"), None)?;
            let workspace = Path::new(
                job["request"]["workspace"]
                    .as_str()
                    .ok_or_else(|| ServiceError::invalid("missing workspace"))?,
            );
            let account = job["request"]["account_id"]
                .as_str()
                .ok_or_else(|| ServiceError::invalid("missing account"))?;
            let prepared = crate::input::prepare(&args[2..].join(" "), workspace, &[])?;
            for warning in &prepared.warnings {
                eprintln!("warning: {warning}");
            }
            let artifacts = client.upload_files(account, &prepared.files, &prepared.temp_files)?;
            client.request(
                "POST",
                &format!("/api/agents/{id}/steer"),
                Some(&json!({"message":prepared.text,"artifacts":artifacts})),
            )?
        }
        "follow" if kind == "agents" => return follow_agent(&client, id, false, false),
        _ => return Err(ServiceError::invalid("unknown job operation")),
    };
    print_json(&value);
    Ok(())
}
fn create_account_on_client(
    client: &Client,
    desired_id: Option<&str>,
    port: u16,
    provider: Option<&str>,
    existing_accounts: &[Value],
) -> Result<Value> {
    let provider_id = provider.unwrap_or(crate::provider::DEFAULT_PROVIDER_ID);
    let account_id = match desired_id {
        Some(id) => id.to_owned(),
        None => {
            let used_ids: Vec<&str> = existing_accounts
                .iter()
                .filter_map(|a| a["id"].as_str())
                .collect();
            crate::provider::suggest_account_id(provider_id, &used_ids)
        }
    };
    let mut body = json!({"id": account_id, "port": port});
    if let Some(provider) = provider {
        body["provider"] = json!(provider);
    }
    client.request("POST", "/api/accounts", Some(&body))
}

fn wait_for_account_login(client: &Client, id: &str) -> Result<()> {
    client.request(
        "POST",
        &format!("/api/accounts/{id}/login"),
        Some(&json!({})),
    )?;
    eprintln!("Complete sign-in in the browser. Waiting for model choices; Ctrl+C cancels.");
    let cancel = crate::agent::CancelToken::new();
    let signal = cancel.clone();
    let _ = ctrlc::set_handler(move || signal.cancel());
    let deadline = std::time::Instant::now() + Duration::from_secs(11 * 60);
    poll_until(Duration::from_millis(250), Some(&cancel), || {
        if cancel.is_cancelled() || std::time::Instant::now() >= deadline {
            return Err(ServiceError::new(
                ErrorCode::Cancelled,
                "login",
                "login wait stopped; use /browser login to try again",
            ));
        }
        let status = client.request("GET", "/api/status", None)?;
        let login = status["accounts"]
            .as_array()
            .and_then(|accounts| accounts.iter().find(|a| a["id"] == id))
            .map(|a| &a["login"]);
        match login.and_then(|value| value["state"].as_str()) {
            Some("ready") => {
                eprintln!("Signed in ({id}). Model choices are ready.");
                return Ok(std::ops::ControlFlow::Break(()));
            }
            Some("failed" | "timed_out") => {
                return Err(ServiceError::invalid(
                    login
                        .and_then(|value| value["error"].as_str())
                        .unwrap_or("login did not complete; run /model refresh after signing in"),
                ))
            }
            _ => {}
        }
        Ok(std::ops::ControlFlow::Continue(()))
    })
}

type AccountsAddArgs = (Option<String>, Option<u16>, Option<String>, bool);

fn parse_accounts_add_args(args: &[String]) -> Result<AccountsAddArgs> {
    let mut positionals: Vec<&str> = Vec::new();
    let mut provider: Option<String> = None;
    let mut do_login = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--provider" => {
                let raw = iter
                    .next()
                    .ok_or_else(|| ServiceError::invalid("missing provider ID"))?;
                provider = Some(identifier(raw)?.to_owned());
            }
            "--login" => do_login = true,
            s if s.starts_with('-') => {
                return Err(ServiceError::invalid(format!(
                    "unexpected option {s}; expected accounts add [ID] [PORT] [--provider ID] [--login]"
                )))
            }
            pos => positionals.push(pos),
        }
    }
    let (id, port) = match positionals.as_slice() {
        [] => {
            if provider.is_none() {
                return Err(ServiceError::invalid(
                    "accounts add [ID] [PORT] [--provider ID] [--login]",
                ));
            }
            (None, None)
        }
        [one] => {
            if one.chars().all(|c| c.is_ascii_digit()) {
                let p = one
                    .parse::<u16>()
                    .map_err(|_| ServiceError::invalid("invalid port"))?;
                if provider.is_none() {
                    return Err(ServiceError::invalid(
                        "accounts add [ID] [PORT] [--provider ID] [--login]",
                    ));
                }
                (None, Some(p))
            } else {
                (Some(identifier(one)?.to_owned()), None)
            }
        }
        [id_raw, port_raw] => {
            let id = identifier(id_raw)?.to_owned();
            let port = port_raw
                .parse::<u16>()
                .map_err(|_| ServiceError::invalid("invalid port"))?;
            (Some(id), Some(port))
        }
        _ => {
            return Err(ServiceError::invalid(
                "accounts add [ID] [PORT] [--provider ID] [--login]",
            ))
        }
    };
    Ok((id, port, provider, do_login))
}

fn parse_login_args(args: &[String]) -> Result<(Option<String>, Option<String>)> {
    let mut target: Option<String> = None;
    let mut provider: Option<String> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--provider" => {
                let raw = iter
                    .next()
                    .ok_or_else(|| ServiceError::invalid("missing provider ID"))?;
                provider = Some(identifier(raw)?.to_owned());
            }
            s if s.starts_with('-') => {
                return Err(ServiceError::invalid(format!(
                    "unexpected option {s}; expected login [ACCOUNT|PROVIDER] [--provider ID]"
                )))
            }
            pos => {
                if target.is_some() {
                    return Err(ServiceError::invalid(
                        "login [ACCOUNT|PROVIDER] [--provider ID]",
                    ));
                }
                target = Some(identifier(pos)?.to_owned());
            }
        }
    }
    Ok((target, provider))
}

fn login_with_client(client: &Client, args: &[String]) -> Result<()> {
    let (target, provider) = parse_login_args(args)?;
    let accounts_val = client.request("GET", "/api/accounts", None)?;
    let providers_val = client.request("GET", "/api/providers", None)?;
    let accounts = accounts_val.as_array().map(Vec::as_slice).unwrap_or(&[]);
    let providers = providers_val.as_array().map(Vec::as_slice).unwrap_or(&[]);

    let account_id = match (target.as_deref(), provider.as_deref()) {
        (None, None) => "default".to_owned(),
        (Some(acct), None) => {
            if accounts.iter().any(|a| a["id"].as_str() == Some(acct)) {
                acct.to_owned()
            } else if providers.iter().any(|p| p["id"].as_str() == Some(acct)) {
                ensure_account_for_provider(client, None, acct, accounts)?
            } else {
                return Err(ServiceError::new(
                    ErrorCode::NotFound,
                    "account",
                    format!(
                        "no account or registered provider named '{acct}'. Run `anticrumb providers list` or `anticrumb accounts list`."
                    ),
                ));
            }
        }
        (desired, Some(pid)) => {
            if !providers.iter().any(|p| p["id"].as_str() == Some(pid)) {
                return Err(ServiceError::new(
                    ErrorCode::NotFound,
                    "provider",
                    format!(
                        "provider '{pid}' is not registered. Add it with `anticrumb providers add <DIR_OR_FILE>`."
                    ),
                ));
            }
            ensure_account_for_provider(client, desired, pid, accounts)?
        }
    };
    wait_for_account_login(client, &account_id)
}

fn ensure_account_for_provider(
    client: &Client,
    desired_id: Option<&str>,
    provider_id: &str,
    accounts: &[Value],
) -> Result<String> {
    if let Some(id) = desired_id {
        if let Some(existing) = accounts.iter().find(|a| a["id"].as_str() == Some(id)) {
            let actual = existing["provider"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or(crate::provider::DEFAULT_PROVIDER_ID);
            if actual != provider_id {
                return Err(ServiceError::invalid(format!(
                    "account '{id}' uses provider '{actual}', not '{provider_id}'"
                )));
            }
            return Ok(id.to_owned());
        }
    } else {
        let matching: Vec<&Value> = accounts
            .iter()
            .filter(|a| a["provider"].as_str() == Some(provider_id))
            .collect();
        if let Some(found) = matching
            .iter()
            .copied()
            .find(|a| a["state"].as_str() == Some("ready"))
            .or_else(|| matching.into_iter().next())
        {
            if let Some(id) = found["id"].as_str() {
                return Ok(id.to_owned());
            }
        }
    }
    let created = create_account_on_client(client, desired_id, 0, Some(provider_id), accounts)?;
    let created_id = created["id"]
        .as_str()
        .ok_or_else(|| ServiceError::invalid("created account missing id"))?
        .to_owned();
    let port = created["port"].as_u64().unwrap_or(0);
    eprintln!("Created account '{created_id}' for provider '{provider_id}' on CDP port {port}.");
    Ok(created_id)
}

fn login_command(args: &[String]) -> Result<()> {
    let client = Client::connect()?;
    login_with_client(&client, args)
}

fn account_command(args: &[String]) -> Result<()> {
    let client = Client::connect()?;
    let action = args.first().map(String::as_str).unwrap_or("list");
    match action {
        "list" | "status" => print_json(&client.request("GET", "/api/accounts", None)?),
        "add" => {
            let (id, port, provider, do_login) = parse_accounts_add_args(&args[1..])?;
            let existing = if id.is_none() {
                client
                    .request("GET", "/api/accounts", None)?
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            let created = create_account_on_client(
                &client,
                id.as_deref(),
                port.unwrap_or(0),
                provider.as_deref(),
                &existing,
            )?;
            print_json(&created);
            if do_login {
                let created_id = created["id"]
                    .as_str()
                    .ok_or_else(|| ServiceError::invalid("created account missing id"))?;
                wait_for_account_login(&client, created_id)?;
            }
        }
        "login" => login_with_client(&client, &args[1..])?,
        "attach" => {
            let id = identifier(
                args.get(1)
                    .ok_or_else(|| ServiceError::invalid("accounts attach ID PORT"))?,
            )?;
            let port = args
                .get(2)
                .and_then(|p| p.parse::<u16>().ok())
                .filter(|p| *p > 0)
                .ok_or_else(|| ServiceError::invalid("port must be 1..65535"))?;
            print_json(&client.request(
                "POST",
                &format!("/api/accounts/{id}/attach"),
                Some(&json!({"port":port})),
            )?);
        }
        "resolve" => {
            let id = identifier(
                args.get(1)
                    .ok_or_else(|| ServiceError::invalid("missing account"))?,
            )?;
            print_json(&client.request(
                "POST",
                &format!("/api/accounts/{id}/resolve"),
                Some(&json!({})),
            )?);
        }
        "pause" => {
            let id = identifier(
                args.get(1)
                    .ok_or_else(|| ServiceError::invalid("missing account"))?,
            )?;
            print_json(&client.request(
                "POST",
                &format!("/api/accounts/{id}/pause"),
                Some(&json!({})),
            )?);
        }
        "show" | "hide" => {
            let id = identifier(
                args.get(1)
                    .ok_or_else(|| ServiceError::invalid("missing account"))?,
            )?;
            print_json(&client.request(
                "POST",
                &format!("/api/accounts/{id}/browser-mode"),
                Some(&json!({"headless": args[0] == "hide"})),
            )?);
        }
        _ => {
            return Err(ServiceError::invalid(
                "accounts list|add|attach|show|hide|login|resolve|pause",
            ))
        }
    }
    Ok(())
}
fn parse_providers_add_args(args: &[String]) -> Result<(String, bool, bool)> {
    let mut path: Option<String> = None;
    let mut replace = false;
    let mut do_login = false;
    for arg in args {
        match arg.as_str() {
            "--replace" => replace = true,
            "--login" => do_login = true,
            s if s.starts_with('-') => {
                return Err(ServiceError::invalid(format!(
                "unexpected option {s}; expected providers add DIR_OR_FILE [--replace] [--login]"
            )))
            }
            pos => {
                if path.is_some() {
                    return Err(ServiceError::invalid(
                        "providers add DIR_OR_FILE [--replace] [--login]",
                    ));
                }
                path = Some(pos.to_owned());
            }
        }
    }
    let path = path
        .ok_or_else(|| ServiceError::invalid("providers add DIR_OR_FILE [--replace] [--login]"))?;
    Ok((path, replace, do_login))
}
fn provider_command(args: &[String]) -> Result<()> {
    let client = Client::connect()?;
    let action = args.first().map(String::as_str).unwrap_or("list");
    match action {
        "list" => print_json(&client.request("GET", "/api/providers", None)?),
        "show" => {
            let id = identifier(
                args.get(1)
                    .ok_or_else(|| ServiceError::invalid("providers show ID"))?,
            )?;
            print_json(&client.request("GET", &format!("/api/providers/{id}"), None)?);
        }
        "add" => {
            let (path, replace, do_login) = parse_providers_add_args(&args[1..])?;
            let value = read_provider_manifest_for_add(Path::new(&path))?;
            let provider_id = value["id"]
                .as_str()
                .ok_or_else(|| ServiceError::invalid("provider manifest missing id"))?
                .to_owned();
            if replace {
                let _ = client.request("DELETE", &format!("/api/providers/{provider_id}"), None);
            }
            match client.request("POST", "/api/providers", Some(&value)) {
                Ok(registered) => print_json(&registered),
                Err(err) if do_login && err.code == ErrorCode::InvalidRequest => {
                    eprintln!(
                        "Provider '{provider_id}' is already registered; proceeding to login."
                    );
                }
                Err(err) => return Err(err),
            }
            if do_login {
                let accounts_val = client.request("GET", "/api/accounts", None)?;
                let accounts = accounts_val.as_array().map(Vec::as_slice).unwrap_or(&[]);
                let account_id =
                    ensure_account_for_provider(&client, None, &provider_id, accounts)?;
                wait_for_account_login(&client, &account_id)?;
            }
        }
        "remove" | "unregister" => {
            let id = identifier(
                args.get(1)
                    .ok_or_else(|| ServiceError::invalid("providers remove ID"))?,
            )?;
            print_json(&client.request("DELETE", &format!("/api/providers/{id}"), None)?);
        }
        _ => {
            return Err(ServiceError::invalid(
                "providers list|show ID|add DIR_OR_FILE|remove ID",
            ))
        }
    }
    Ok(())
}
fn resolve_account_from_lists(
    account: &mut String,
    provider: &mut Option<String>,
    provider_name: &mut Option<String>,
    account_overridden: bool,
    provider_overridden: bool,
    accounts: &[Value],
    providers: &[Value],
) -> Result<()> {
    let find_provider_name = |pid: &str| -> Option<String> {
        providers
            .iter()
            .find(|p| p["id"].as_str() == Some(pid))
            .and_then(|p| p["name"].as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    if !provider_overridden {
        if let Some(found_provider) = accounts
            .iter()
            .find(|a| a["id"].as_str() == Some(account.as_str()))
            .and_then(|a| a["provider"].as_str())
            .filter(|s| !s.is_empty())
        {
            *provider = Some(found_provider.to_owned());
            *provider_name = find_provider_name(found_provider);
        }
        return Ok(());
    }
    let requested = provider.clone().unwrap_or_default();
    if !providers.is_empty()
        && !providers
            .iter()
            .any(|p| p["id"].as_str() == Some(requested.as_str()))
    {
        return Err(ServiceError::new(
            ErrorCode::NotFound,
            "provider",
            format!(
                "provider '{requested}' is not registered. Run `anticrumb providers add <DIR_OR_FILE>` or `anticrumb providers list`."
            ),
        ));
    }
    if account_overridden {
        let Some(matched) = accounts
            .iter()
            .find(|a| a["id"].as_str() == Some(account.as_str()))
        else {
            return Err(ServiceError::new(
                ErrorCode::NotFound,
                "account",
                format!("account '{account}' does not exist"),
            ));
        };
        let actual = matched["provider"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or(crate::provider::DEFAULT_PROVIDER_ID);
        if actual != requested {
            return Err(ServiceError::invalid(format!(
                "account '{account}' uses provider '{actual}', not '{requested}'"
            )));
        }
        *provider_name = find_provider_name(&requested);
        return Ok(());
    }
    let matching: Vec<&Value> = accounts
        .iter()
        .filter(|a| a["provider"].as_str() == Some(requested.as_str()))
        .collect();
    let chosen = matching
        .iter()
        .copied()
        .find(|a| a["state"].as_str() == Some("ready"))
        .or_else(|| matching.into_iter().next());
    let Some(chosen) = chosen else {
        return Err(ServiceError::new(
            ErrorCode::NotFound,
            "account",
            format!(
                "no account is registered for provider '{requested}'. Run `anticrumb login --provider {requested}` (or `anticrumb accounts add --provider {requested}`)."
            ),
        ));
    };
    *account = chosen["id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ServiceError::invalid("account entry missing id"))?
        .to_owned();
    *provider_name = find_provider_name(&requested);
    Ok(())
}
fn read_provider_manifest_for_add(path: &Path) -> Result<Value> {
    let manifest_path = if path.is_dir() {
        path.join("provider.json")
    } else {
        path.to_path_buf()
    };
    let mut manifest = crate::provider::ProviderManifest::read_file(&manifest_path)?;
    if manifest.scripts_dir.is_none() {
        if let Some(provider_dir) = manifest_path
            .canonicalize()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf))
        {
            let scripts = provider_dir.join("scripts");
            if scripts.is_dir() {
                manifest.scripts_dir = Some(scripts);
            } else if provider_dir.join("driver.js").is_file() {
                manifest.scripts_dir = Some(provider_dir);
            }
        }
    }
    if manifest.scripts.is_empty() {
        if let Some(dir) = &manifest.scripts_dir {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let file_path = entry.path();
                    if !file_path.is_file() {
                        continue;
                    }
                    let Some(name) = file_path.file_name().and_then(|s| s.to_str()) else {
                        continue;
                    };
                    if !name.ends_with(".js") {
                        continue;
                    }
                    if let Ok(body) = fs::read_to_string(&file_path) {
                        if body.len() <= 128 * 1024 {
                            manifest.scripts.insert(name.to_owned(), body);
                        }
                    }
                }
            }
        }
    }
    serde_json::to_value(manifest).map_err(ServiceError::storage)
}
fn knowledge_command(args: &[String]) -> Result<()> {
    let store = Store::open(&root())?;
    match args.first().map(String::as_str).unwrap_or("list") {
        "list" => print_json(&store.list::<crate::knowledge::Source>("local", "source")?),
        "add" => print_json(&crate::knowledge::add(
            &store,
            args.get(1)
                .ok_or_else(|| ServiceError::invalid("missing source id"))?,
            Path::new(
                args.get(2)
                    .ok_or_else(|| ServiceError::invalid("missing path"))?,
            ),
            args.get(3).cloned(),
        )?),
        "sync" => print_json(&crate::knowledge::sync(
            &store,
            args.get(1)
                .ok_or_else(|| ServiceError::invalid("missing source id"))?,
        )?),
        "query" => print_json(&crate::knowledge::query(
            &store,
            &args[1..].join(" "),
            None,
            5,
        )?),
        _ => {
            return Err(ServiceError::invalid(
                "knowledge list|add ID PATH [NOTEBOOK_URL]|sync ID|query TEXT",
            ))
        }
    }
    Ok(())
}
fn connect_config(args: &[String]) -> Result<()> {
    let target = args.first().map(String::as_str).unwrap_or("opencode");
    let base = match crate::rpc::connection::load(&config_root())? {
        Some(crate::rpc::client::Connection::Grpc { endpoint, .. }) => endpoint,
        _ => "https://127.0.0.1:50051".into(),
    };
    let exe = std::env::current_exe().map_err(ServiceError::storage)?;
    let value = match target {
        "opencode" => {
            let reuse_scope = id("opencode");
            let version = std::process::Command::new("opencode")
                .arg("--version")
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
            eprintln!(
                "OpenCode: {}",
                version
                    .as_deref()
                    .unwrap_or("not found; select --schema=1 or --schema=2")
            );
            let modern = args.iter().any(|a| a == "--schema=2")
                || version.as_deref().is_some_and(|v| v.starts_with("2."));
            if version.is_none() && !args.iter().any(|a| a.starts_with("--schema=")) {
                return Err(ServiceError::invalid(
                    "cannot detect OpenCode schema; pass --schema=1 or --schema=2",
                ));
            }
            if modern {
                json!({"providers":{"anticrumb":{"package":"@opencode/ai/providers/openai-compatible","name":"Gemini Web Local","settings":{"baseURL":format!("{base}/v1"),"apiKey":"{env:ANTICRUMB_TOKEN}","headers":{"X-Anticrumb-Reuse-Scope":reuse_scope}},"models":{"gemini-web-default":{"name":"Gemini Web selected model"}}}}})
            } else {
                json!({"provider":{"anticrumb":{"npm":"@ai-sdk/openai-compatible","name":"Gemini Web Local","options":{"baseURL":format!("{base}/v1"),"apiKey":"{env:ANTICRUMB_TOKEN}","headers":{"X-Anticrumb-Reuse-Scope":reuse_scope}},"models":{"gemini-web-default":{"name":"Gemini Web selected model","limit":{"context":24000,"output":8192}}}}}})
            }
        }
        "mcp" => json!({"mcpServers":{"anticrumb":{"command":exe,"args":["mcp"]}}}),
        "claude" => {
            json!({"env":{"ANTHROPIC_BASE_URL":base,"ANTHROPIC_MODEL":"gemini-web-default","ANTHROPIC_DEFAULT_SONNET_MODEL":"gemini-web-default","ANTHROPIC_DEFAULT_HAIKU_MODEL":"gemini-web-default","ANTHROPIC_DEFAULT_OPUS_MODEL":"gemini-web-default","CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC":"1"}})
        }
        "antigravity" => {
            json!({"modelProvider":"gemini","model":"gemini-web-default","env":{"GOOGLE_GEMINI_BASE_URL":base}})
        }
        "codex" => {
            if args.iter().any(|a| a == "--install") {
                return Err(ServiceError::invalid(
                    "Codex configuration is TOML; merge the printed configuration manually",
                ));
            }

            println!("model_provider = \"anticrumb\"\nmodel = \"gemini-web-default\"\nmodel_context_window = 24000\nmodel_auto_compact_token_limit = 18000\n[model_providers.anticrumb]\nname = \"Gemini Web Local\"\nbase_url = \"{base}/v1\"\nwire_api = \"responses\"\nenv_key = \"ANTICRUMB_TOKEN\"");
            eprintln!("Use the token printed by `anticrumb server` as ANTICRUMB_TOKEN. Configure the provider client to trust the server certificate.");
            return Ok(());
        }
        _ => {
            return Err(ServiceError::invalid(
                "connect opencode|codex|claude|antigravity|mcp",
            ))
        }
    };
    if let Some(index) = args.iter().position(|s| s == "--install") {
        let path = PathBuf::from(
            args.get(index + 1)
                .ok_or_else(|| ServiceError::invalid("--install needs a JSON file path"))?,
        );
        let mut merged = if path.exists() {
            serde_json::from_slice::<Value>(&fs::read(&path).map_err(ServiceError::storage)?)
                .map_err(|_| {
                    ServiceError::invalid(
                        "existing file is not JSON; merge printed configuration manually",
                    )
                })?
        } else {
            json!({})
        };
        fn merge(to: &mut Value, from: Value) {
            match (to, from) {
                (Value::Object(to), Value::Object(from)) => {
                    for (k, v) in from {
                        merge(to.entry(k).or_insert(Value::Null), v)
                    }
                }
                (to, from) => *to = from,
            }
        }
        merge(&mut merged, value);
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(ServiceError::storage)?;
            }
        }
        if path.exists() {
            fs::copy(
                &path,
                path.with_extension(format!("json.backup-{}", now_ms())),
            )
            .map_err(ServiceError::storage)?;
        }
        let tmp = path.with_extension(format!("json.tmp-{}", id("config")));
        fs::write(
            &tmp,
            serde_json::to_vec_pretty(&merged).map_err(ServiceError::storage)?,
        )
        .map_err(ServiceError::storage)?;
        fs::rename(tmp, &path).map_err(ServiceError::storage)?;
        eprintln!("Wrote {}", path.display());
    } else {
        print_json(&value);
    }
    eprintln!("Use the token printed by `anticrumb server` as ANTICRUMB_TOKEN (OpenCode/Codex), ANTHROPIC_AUTH_TOKEN (Claude), or GEMINI_API_KEY (Antigravity). Configure the provider client to trust the server certificate.");
    Ok(())
}
fn group_conversations(value: Value, workspace: &Path, account: &str) -> Result<Vec<Value>> {
    let all_jobs: Vec<Value> = value
        .as_array()
        .ok_or_else(|| ServiceError::invalid("invalid conversation list"))?
        .iter()
        .filter(|v| {
            v["request"]["workspace"]
                .as_str()
                .is_some_and(|s| Path::new(s) == workspace)
                && v["request"]["account_id"] == account
        })
        .cloned()
        .collect();

    // Group conversation chains: only show one entry per conversation.
    // A "leaf" is a turn whose ID is not referenced as parent_id by any
    // other turn. Its label uses the root turn's first prompt text.
    let parent_ids: std::collections::HashSet<&str> = all_jobs
        .iter()
        .filter_map(|v| v["request"]["parent_id"].as_str())
        .collect();
    let job_map: std::collections::HashMap<&str, &Value> = all_jobs
        .iter()
        .filter_map(|v| Some((v["id"].as_str()?, v)))
        .collect();

    let mut jobs: Vec<Value> = all_jobs
        .iter()
        .filter(|v| v["id"].as_str().is_some_and(|id| !parent_ids.contains(id)))
        .map(|leaf| {
            // Walk up the parent chain to find the root's first prompt.
            let mut current = leaf;
            for _ in 0..100 {
                match current["request"]["parent_id"]
                    .as_str()
                    .and_then(|pid| job_map.get(pid))
                {
                    Some(parent) => current = parent,
                    None => break,
                }
            }
            let root_task = current["request"]["task"].clone();
            let mut entry = (*leaf).clone();
            entry["request"]["task"] = root_task;
            entry
        })
        .collect();

    jobs.sort_by_key(|v| {
        std::cmp::Reverse(
            v["updated_at"]
                .as_u64()
                .or(v["created_at"].as_u64())
                .unwrap_or(0),
        )
    });
    Ok(jobs)
}

fn upload_mime(path: &Path) -> &'static str {
    let extension = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "txt" | "md" | "rs" | "py" | "js" => "text/plain",
        _ => "application/octet-stream",
    }
}

impl Client {
    pub(crate) async fn request_async(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value> {
        crate::rpc::workspace_client::presentation(&self.rpc, method, path, body).await
    }
    pub(crate) async fn upload_files_async(
        &self,
        account: &str,
        files: &[PathBuf],
        temp_files: &[PathBuf],
    ) -> Result<Vec<String>> {
        // Runs on success, error, and when the TUI aborts this future mid-upload.
        struct TempFiles<'a>(&'a [PathBuf]);
        impl Drop for TempFiles<'_> {
            fn drop(&mut self) {
                crate::input::cleanup_temp_files(self.0);
            }
        }
        let _temp_files = TempFiles(temp_files);
        if files.len() > crate::input::MAX_FILES {
            return Err(ServiceError::invalid("maximum 10 attachments"));
        }
        let mut uploaded = Vec::with_capacity(files.len());
        {
            for path in files {
                let path_owned = path.clone();
                let (resolved, name, mime) = tokio::task::spawn_blocking(move || {
                    let resolved = crate::input::file(Path::new("."), &path_owned)?;
                    let mime = upload_mime(&resolved);
                    let name = resolved
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned();
                    Ok::<_, ServiceError>((resolved, name, mime))
                })
                .await
                .map_err(ServiceError::storage)??;
                let value = self
                    .rpc
                    .put_artifact_file(account, &resolved, &name, mime)
                    .await?;
                uploaded.push(value.id);
            }
        }
        Ok(uploaded)
    }
    pub(crate) async fn conversations_async(
        &self,
        workspace: &Path,
        account: &str,
    ) -> Result<Vec<Value>> {
        let value = self.request_async("GET", "/api/agents", None).await?;
        group_conversations(value, workspace, account)
    }
    pub(crate) async fn conversation_async(
        &self,
        id: &str,
        workspace: &Path,
        account: &str,
    ) -> Result<Value> {
        if id == "last" || id.is_empty() {
            return self
                .conversations_async(workspace, account)
                .await?
                .into_iter()
                .next()
                .ok_or_else(|| {
                    ServiceError::invalid("no saved conversations in this working directory")
                });
        }
        self.request_async("GET", &format!("/api/agents/{}", identifier(id)?), None)
            .await
    }
    pub(crate) async fn delete_conversation_async(
        &self,
        id: &str,
        workspace: &Path,
        account: &str,
    ) -> Result<()> {
        let id = identifier(id)?;
        let job = self
            .request_async("GET", &format!("/api/agents/{id}"), None)
            .await?;
        if job["request"]["workspace"].as_str().map(Path::new) != Some(workspace)
            || job["request"]["account_id"] != account
        {
            return Err(ServiceError::invalid(
                "conversation deletion requires the same workspace and account",
            ));
        }
        self.request_async("DELETE", &format!("/api/agents/{id}"), None)
            .await?;
        Ok(())
    }
    /// Read events straight from RPC, skipping the JSON presentation round trip
    /// (hot path: polled continuously while a turn is active).
    pub(crate) async fn events_async(&self, id: &str, after: i64) -> Result<Vec<Event>> {
        let cursor = crate::rpc::pb::EventCursor {
            run_id: id.into(),
            after,
        };
        let page = if id.starts_with("agent_") {
            self.rpc.read_agent_events(cursor).await?
        } else {
            self.rpc.read_events(cursor).await?
        };
        page.events
            .into_iter()
            .map(crate::rpc::codec::event_from)
            .collect()
    }
    pub(crate) async fn agent_async(&self, id: &str) -> Result<crate::rpc::pb::Agent> {
        self.rpc
            .get_agent(crate::rpc::pb::AgentRef { id: id.into() })
            .await
    }
    pub(crate) fn agent_value(agent: crate::rpc::pb::Agent) -> Result<Value> {
        serde_json::to_value(crate::rpc::codec::agent_from(agent)?).map_err(ServiceError::storage)
    }
    pub(crate) async fn start_agent_async(
        &self,
        request: crate::agent_service::AgentRequest,
    ) -> Result<String> {
        let id = crate::rpc::workspace_client::start_agent(&self.rpc, request)
            .await?
            .id;
        if id.is_empty() {
            return Err(ServiceError::invalid("missing job ID"));
        }
        Ok(id)
    }
    pub(crate) async fn clear_web_history_async(&self, account: &str) -> Result<Value> {
        self.request_async(
            "POST",
            &format!("/api/accounts/{account}/clear-history"),
            Some(&json!({})),
        )
        .await
    }
    pub(crate) async fn web_history_cleanup_async(&self, account: &str) -> Result<Value> {
        self.request_async(
            "GET",
            &format!("/api/accounts/{account}/clear-history"),
            None,
        )
        .await
    }
    pub fn upload_files(
        &self,
        account: &str,
        files: &[PathBuf],
        temp_files: &[PathBuf],
    ) -> Result<Vec<String>> {
        self.runtime
            .block_on(self.upload_files_async(account, files, temp_files))
    }
    pub fn conversations(&self, workspace: &Path, account: &str) -> Result<Vec<Value>> {
        self.runtime
            .block_on(self.conversations_async(workspace, account))
    }
    pub fn conversation(&self, id: &str, workspace: &Path, account: &str) -> Result<Value> {
        self.runtime
            .block_on(self.conversation_async(id, workspace, account))
    }
    pub fn delete_conversation(&self, id: &str, workspace: &Path, account: &str) -> Result<()> {
        self.runtime
            .block_on(self.delete_conversation_async(id, workspace, account))
    }
    pub fn events(&self, id: &str, after: i64) -> Result<Vec<Event>> {
        self.runtime.block_on(self.events_async(id, after))
    }
    pub fn upload(&self, account: &str, path: &Path) -> Result<String> {
        let path = crate::input::file(Path::new("."), path)?;
        self.put_file(account, &path, upload_mime(&path))
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_banner_contains_version_commit_and_build_time() {
        let banner = crate::build_info::help_banner();
        assert!(banner.starts_with(&format!(
            "anticrumb {}",
            crate::build_info::display_version()
        )));
        assert!(banner.contains("commit: "));
        assert!(banner.contains("built: "));
    }

    #[test]
    fn familiar_flags_and_resume_overrides_are_preserved() {
        let mut options = PromptOptions::parse(&[
            "-m".into(),
            "selected".into(),
            "-p".into(),
            "--no-shell".into(),
            "--workers".into(),
            "4".into(),
            "--pace".into(),
            "high".into(),
            "question".into(),
        ])
        .unwrap();
        assert!(options.print);
        assert!(!options.yolo);
        assert_eq!(options.workers, Some(4));
        assert_eq!(options.pace, Some(crate::gemini::HumanPace::High));
        assert_eq!(options.words, vec!["question"]);
        let previous = options.agent_request("earlier".into(), None);
        assert!(previous.chat_only);
        let mut previous = previous;
        previous.model = Some("old".into());
        previous.no_shell = false;
        let job = json!({ "request": previous });
        options.inherit_from_job(&job).unwrap();
        assert_eq!(options.model.as_deref(), Some("selected"));
        assert!(options.no_shell);

        let dir = tempfile::tempdir().unwrap();
        // Default --agent permissions are read-only (read=true, edit=false, shell=false, browser=false, subagent=false)
        let mut default_agent = PromptOptions::parse(&["--agent".into(), "task".into()]).unwrap();
        default_agent.load_saved_permissions(dir.path());
        assert_eq!(
            default_agent.effective_permissions(),
            ToolPermissions::read_only()
        );

        let prefs = crate::tui::preferences::Preferences::open(dir.path()).unwrap();
        prefs
            .set_permissions(ToolPermissions {
                read: true,
                edit: true,
                shell: false,
                browser: true,
                subagent: false,
            })
            .unwrap();

        let mut agent_opts = PromptOptions::parse(&["--agent".into(), "task".into()]).unwrap();
        agent_opts.load_saved_permissions(dir.path());
        assert!(agent_opts.agent);
        assert!(!agent_opts.yolo);
        assert!(
            agent_opts.no_shell,
            "--agent must respect saved /permissions"
        );
        let agent_req = agent_opts.agent_request("task".into(), None);
        assert!(!agent_req.chat_only);
        assert!(agent_req.no_shell);
        assert_eq!(
            agent_req.permissions,
            Some(ToolPermissions {
                read: true,
                edit: true,
                shell: false,
                browser: true,
                subagent: false,
            })
        );

        let mut yolo =
            PromptOptions::parse(&["--no-shell".into(), "--yolo".into(), "task".into()]).unwrap();
        yolo.load_saved_permissions(dir.path());
        assert!(yolo.agent);
        assert!(yolo.yolo);
        assert!(!yolo.no_shell);
        let yolo_req = yolo.agent_request("task".into(), None);
        assert!(!yolo_req.chat_only);
        assert!(!yolo_req.no_shell);
        assert_eq!(yolo_req.permissions, Some(ToolPermissions::all()));
    }

    #[test]
    fn temporary_flags_persistence_and_resume_overrides_work() {
        let dir = tempfile::tempdir().unwrap();
        let prefs = crate::tui::preferences::Preferences::open(dir.path()).unwrap();

        // Default is non-temporary when nothing is saved.
        let mut default_opts = PromptOptions::parse(&["hello".into()]).unwrap();
        default_opts.load_saved_temporary(dir.path());
        assert!(!default_opts.temporary);

        // Saved temporary preference applies automatically unless overridden.
        prefs.set_temporary(true).unwrap();
        let mut saved_opts = PromptOptions::parse(&["hello".into()]).unwrap();
        saved_opts.load_saved_temporary(dir.path());
        assert!(saved_opts.temporary);
        assert!(saved_opts.agent_request("hello".into(), None).is_temporary());

        // `--no-temp` has been removed; `--temp` explicitly enables temporary mode and marks override.
        assert!(PromptOptions::parse(&["--no-temp".into(), "hello".into()]).is_err());

        let temp_flag = PromptOptions::parse(&["--temp".into(), "hello".into()]).unwrap();
        assert!(temp_flag.temporary);
        assert!(temp_flag.overrides.contains("temporary"));

        // Resuming a normal (`SessionMode::Chat`) job while saved `/temp on` is active preserves
        // temporary mode so follow-ups never silently leak to server history.
        let mut normal_job = default_opts.agent_request("normal earlier".into(), None);
        normal_job.session_mode = SessionMode::Chat;
        let mut saved_resuming_normal = PromptOptions::parse(&[]).unwrap();
        saved_resuming_normal.load_saved_temporary(dir.path());
        saved_resuming_normal.inherit(&normal_job).unwrap();
        assert!(saved_resuming_normal.temporary);

        // Explicit `--temp` enables temporary mode when resuming a normal job even when saved pref is off.
        prefs.set_temporary(false).unwrap();
        let mut temp_job = saved_opts.agent_request("temp earlier".into(), None);
        temp_job.session_mode = SessionMode::Temporary;
        let mut forced_temp_on_normal = PromptOptions::parse(&["--temp".into()]).unwrap();
        forced_temp_on_normal.load_saved_temporary(dir.path());
        forced_temp_on_normal.inherit(&normal_job).unwrap();
        assert!(forced_temp_on_normal.temporary);

        // When saved preference is `/temp off`, resuming a `[temp]` job still keeps it temporary,
        // even if `"temporary"` was somehow in `overrides` with `temporary == false`.
        let mut off_resuming_temp = PromptOptions::parse(&[]).unwrap();
        off_resuming_temp.load_saved_temporary(dir.path());
        off_resuming_temp.inherit(&temp_job).unwrap();
        assert!(off_resuming_temp.temporary);

        let mut overridden_false_resuming_temp = PromptOptions::parse(&[]).unwrap();
        overridden_false_resuming_temp
            .overrides
            .insert("temporary".into());
        overridden_false_resuming_temp.temporary = false;
        overridden_false_resuming_temp.inherit(&temp_job).unwrap();
        assert!(overridden_false_resuming_temp.temporary);

        // `--session` mode auto-detection distinguishes `temporary` vs `chat` sessions cleanly
        // via `SessionService.GetSession` (`pb::SessionMode`), and `resolve_chat_temporary`
        // intentionally ignores saved `/temp on` when an explicit `--session` is provided.
        let chat_session = crate::rpc::pb::Session {
            id: "sess-chat".into(),
            mode: crate::rpc::pb::SessionMode::Chat as i32,
            ..Default::default()
        };
        let temp_session = crate::rpc::pb::Session {
            id: "sess-temp".into(),
            mode: crate::rpc::pb::SessionMode::Temporary as i32,
            ..Default::default()
        };
        assert_eq!(session_is_temporary(&chat_session), Some(false));
        assert_eq!(session_is_temporary(&temp_session), Some(true));
        assert_eq!(session_is_temporary(&crate::rpc::pb::Session::default()), None);

        let fetch_session = |sid: &str| match sid {
            "sess-chat" => Ok(chat_session.clone()),
            "sess-temp" => Ok(temp_session.clone()),
            _ => Err(ServiceError::new(
                ErrorCode::NotFound,
                "store",
                "session not found",
            )),
        };

        prefs.set_temporary(true).unwrap();
        let mut chat_no_session = PromptOptions::parse(&["hello".into()]).unwrap();
        chat_no_session.resolve_chat_temporary(dir.path(), |_| {
            panic!("sessions endpoint must not be queried without --session")
        });
        assert!(chat_no_session.temporary);

        let mut chat_explicit_normal_session =
            PromptOptions::parse(&["--session".into(), "sess-chat".into(), "hello".into()])
                .unwrap();
        chat_explicit_normal_session.resolve_chat_temporary(dir.path(), fetch_session);
        assert!(
            !chat_explicit_normal_session.temporary,
            "explicit --session for a normal chat session must ignore saved /temp on"
        );

        prefs.set_temporary(false).unwrap();
        let mut chat_explicit_temp_session =
            PromptOptions::parse(&["--session".into(), "sess-temp".into(), "hello".into()])
                .unwrap();
        chat_explicit_temp_session.resolve_chat_temporary(dir.path(), fetch_session);
        assert!(
            chat_explicit_temp_session.temporary,
            "explicit --session for a temporary session must adopt temporary mode"
        );

        // Hints attach on temporary-capability errors (both sync UnsupportedCapability and wrapped
        // agent Internal errors), distinguishing saved `/temp on` from explicit `--temp` CLI flag,
        // and never fire on unrelated capability failures like image generation.
        let err = ServiceError::new(
            ErrorCode::Internal,
            "agent",
            "agent ended as failed: provider 'claude-web' does not support temporary sessions",
        );
        let hinted_saved = with_temporary_unsupported_hint(err.clone(), true, false);
        assert!(hinted_saved.message.contains("/temp off"));
        assert!(!hinted_saved.message.contains("omit `--temp`"));
        let hinted_cli = with_temporary_unsupported_hint(err.clone(), true, true);
        assert!(hinted_cli.message.contains("omit `--temp`"));
        assert!(!hinted_cli.message.contains("disable saved temporary mode"));
        let unhinted = with_temporary_unsupported_hint(err, false, false);
        assert!(!unhinted.message.contains("Hint:"));

        let sync_temp_err = ServiceError::new(
            ErrorCode::UnsupportedCapability,
            "prepare",
            "provider 'claude-web' does not support temporary sessions",
        );
        assert!(
            with_temporary_unsupported_hint(sync_temp_err.clone(), true, false)
                .message
                .contains("/temp off")
        );
        assert!(with_temporary_unsupported_hint(sync_temp_err, true, true)
            .message
            .contains("omit `--temp`"));

        let control_err = ServiceError::new(
            ErrorCode::UnsupportedCapability,
            "backend",
            "unsupported capability: temporary chat control unavailable",
        );
        assert!(with_temporary_unsupported_hint(control_err, true, false)
            .message
            .contains("/temp off"));

        let unrelated_image_err = ServiceError::new(
            ErrorCode::UnsupportedCapability,
            "download",
            "provider 'chatgpt-web' does not support image generation",
        );
        let untouched_image_err = with_temporary_unsupported_hint(unrelated_image_err, true, false);
        assert!(!untouched_image_err.message.contains("Hint:"));

        let unrelated_upload_err = ServiceError::new(
            ErrorCode::UnsupportedCapability,
            "upload",
            "provider 'custom' does not support file uploads",
        );
        assert!(
            !with_temporary_unsupported_hint(unrelated_upload_err, true, true)
                .message
                .contains("Hint:")
        );
    }

    #[test]
    fn provider_flag_parses_and_resolves_ready_account_or_validates_mismatch() {
        let mut opts =
            PromptOptions::parse(&["--provider".into(), "claude-web".into(), "hello".into()])
                .unwrap();
        assert_eq!(opts.provider.as_deref(), Some("claude-web"));
        assert!(opts.overrides.contains("provider"));

        let accounts = vec![
            json!({"id": "default", "provider": "gemini-web", "state": "ready"}),
            json!({"id": "claude-auth", "provider": "claude-web", "state": "auth_required"}),
            json!({"id": "claude-ready", "provider": "claude-web", "state": "ready"}),
        ];
        let providers = vec![
            json!({"id": "gemini-web", "name": "Gemini Web"}),
            json!({"id": "claude-web", "name": "Claude Web"}),
            json!({"id": "chatgpt-web", "name": "ChatGPT Web"}),
        ];
        resolve_account_from_lists(
            &mut opts.account,
            &mut opts.provider,
            &mut opts.provider_name,
            opts.overrides.contains("account"),
            opts.overrides.contains("provider"),
            &accounts,
            &providers,
        )
        .unwrap();
        assert_eq!(opts.account, "claude-ready");
        assert_eq!(opts.provider_name.as_deref(), Some("Claude Web"));

        // Both --provider and --account matching works; mismatch errors cleanly.
        let mut both_ok = PromptOptions::parse(&[
            "--provider".into(),
            "claude-web".into(),
            "--account".into(),
            "claude-auth".into(),
        ])
        .unwrap();
        resolve_account_from_lists(
            &mut both_ok.account,
            &mut both_ok.provider,
            &mut both_ok.provider_name,
            true,
            true,
            &accounts,
            &providers,
        )
        .unwrap();
        assert_eq!(both_ok.account, "claude-auth");

        let mut mismatch = PromptOptions::parse(&[
            "--provider".into(),
            "claude-web".into(),
            "--account".into(),
            "default".into(),
        ])
        .unwrap();
        let err = resolve_account_from_lists(
            &mut mismatch.account,
            &mut mismatch.provider,
            &mut mismatch.provider_name,
            true,
            true,
            &accounts,
            &providers,
        )
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidRequest);

        // Registered provider with no account reports setup command.
        let mut missing_acct =
            PromptOptions::parse(&["--provider".into(), "chatgpt-web".into()]).unwrap();
        let err = resolve_account_from_lists(
            &mut missing_acct.account,
            &mut missing_acct.provider,
            &mut missing_acct.provider_name,
            false,
            true,
            &accounts,
            &providers,
        )
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
        assert!(err.message.contains("accounts add"));
    }

    #[test]
    fn providers_add_helper_resolves_relative_scripts_dir_and_directory_path() {
        let dir = tempfile::tempdir().unwrap();
        let provider_dir = dir.path().join("claude-web");
        std::fs::create_dir_all(provider_dir.join("scripts")).unwrap();
        std::fs::write(provider_dir.join("scripts/driver.js"), "() => ({})").unwrap();
        let manifest_path = provider_dir.join("provider.json");
        assert_ne!(std::env::current_dir().unwrap(), provider_dir);
        let expected_scripts = provider_dir.join("scripts").canonicalize().unwrap();
        for scripts_dir in [None, Some("scripts")] {
            std::fs::write(
                &manifest_path,
                serde_json::to_string(&json!({
                    "id": "claude-web",
                    "name": "Claude Web",
                    "entry_url": "https://claude.ai/new",
                    "scripts_dir": scripts_dir,
                }))
                .unwrap(),
            )
            .unwrap();
            let value = read_provider_manifest_for_add(&manifest_path).unwrap();
            assert_eq!(value["scripts_dir"].as_str(), expected_scripts.to_str());
            assert_eq!(value["scripts"]["driver.js"].as_str(), Some("() => ({})"));

            // Passing the provider directory directly also works.
            let from_dir = read_provider_manifest_for_add(&provider_dir).unwrap();
            assert_eq!(from_dir["scripts_dir"].as_str(), expected_scripts.to_str());
            assert_eq!(
                from_dir["scripts"]["driver.js"].as_str(),
                Some("() => ({})")
            );
        }
    }

    #[test]
    fn providers_add_preserves_absolute_scripts_dir_and_inline_scripts() {
        let dir = tempfile::tempdir().unwrap();
        let provider_dir = dir.path().join("provider");
        let scripts_dir = dir.path().join("shared-scripts");
        fs::create_dir_all(&provider_dir).unwrap();
        fs::create_dir_all(&scripts_dir).unwrap();
        fs::write(scripts_dir.join("driver.js"), "() => 'disk'").unwrap();
        fs::write(
            provider_dir.join("provider.json"),
            serde_json::to_string(&json!({
                "id": "test-web",
                "name": "Test Web",
                "scripts_dir": scripts_dir,
                "scripts": {"driver.js": "() => 'inline'"},
            }))
            .unwrap(),
        )
        .unwrap();

        let value = read_provider_manifest_for_add(&provider_dir).unwrap();
        assert_eq!(value["scripts_dir"].as_str(), scripts_dir.to_str());
        assert_eq!(
            value["scripts"]["driver.js"].as_str(),
            Some("() => 'inline'")
        );
    }

    #[test]
    fn simplified_add_and_login_parsers_handle_optional_id_port_and_flags() {
        assert_eq!(
            parse_accounts_add_args(&["--provider".into(), "claude-web".into(), "--login".into(),])
                .unwrap(),
            (None, None, Some("claude-web".into()), true)
        );
        assert_eq!(
            parse_accounts_add_args(&["claude".into(), "--provider".into(), "claude-web".into(),])
                .unwrap(),
            (
                Some("claude".into()),
                None,
                Some("claude-web".into()),
                false
            )
        );
        assert_eq!(
            parse_accounts_add_args(&[
                "claude".into(),
                "9223".into(),
                "--provider".into(),
                "claude-web".into(),
            ])
            .unwrap(),
            (
                Some("claude".into()),
                Some(9223),
                Some("claude-web".into()),
                false
            )
        );
        assert_eq!(
            parse_login_args(&["claude-web".into()]).unwrap(),
            (Some("claude-web".into()), None)
        );
        assert_eq!(
            parse_login_args(&["--provider".into(), "chatgpt-web".into()]).unwrap(),
            (None, Some("chatgpt-web".into()))
        );
        assert_eq!(
            parse_providers_add_args(&[
                "./providers/claude-web".into(),
                "--replace".into(),
                "--login".into(),
            ])
            .unwrap(),
            ("./providers/claude-web".into(), true, true)
        );
    }

    #[test]
    fn conversations_groups_by_chain_and_labels_with_root_prompt() {
        // Simulate the grouping logic from Client::conversations without a server.
        let all_jobs = [
            json!({"id":"turn-1","state":"completed","request":{"task":"Write hello world","workspace":"/ws","account_id":"default","parent_id":null},"updated_at":100}),
            json!({"id":"turn-2","state":"completed","request":{"task":"Add error handling","workspace":"/ws","account_id":"default","parent_id":"turn-1"},"updated_at":200}),
            json!({"id":"turn-3","state":"completed","request":{"task":"Add tests","workspace":"/ws","account_id":"default","parent_id":"turn-2"},"updated_at":300}),
            json!({"id":"solo-1","state":"completed","request":{"task":"Standalone question","workspace":"/ws","account_id":"default","parent_id":null},"updated_at":250}),
        ];

        let parent_ids: std::collections::HashSet<&str> = all_jobs
            .iter()
            .filter_map(|v| v["request"]["parent_id"].as_str())
            .collect();
        let job_map: std::collections::HashMap<&str, &Value> = all_jobs
            .iter()
            .filter_map(|v| Some((v["id"].as_str()?, v)))
            .collect();
        let mut jobs: Vec<Value> = all_jobs
            .iter()
            .filter(|v| v["id"].as_str().is_some_and(|id| !parent_ids.contains(id)))
            .map(|leaf| {
                let mut current = leaf;
                for _ in 0..100 {
                    match current["request"]["parent_id"]
                        .as_str()
                        .and_then(|pid| job_map.get(pid))
                    {
                        Some(parent) => current = parent,
                        None => break,
                    }
                }
                let root_task = current["request"]["task"].clone();
                let mut entry = (*leaf).clone();
                entry["request"]["task"] = root_task;
                entry
            })
            .collect();
        jobs.sort_by_key(|v| std::cmp::Reverse(v["updated_at"].as_u64().unwrap_or(0)));

        // Two conversations: the 3-turn chain and the standalone
        assert_eq!(jobs.len(), 2);
        // Most recent first: turn-3 (300) then solo-1 (250)
        assert_eq!(jobs[0]["id"], "turn-3");
        assert_eq!(jobs[1]["id"], "solo-1");
        // Chain entry uses root's first prompt as label
        assert_eq!(jobs[0]["request"]["task"], "Write hello world");
        assert_eq!(jobs[1]["request"]["task"], "Standalone question");
    }

    #[test]
    fn test_client_identity_and_version() {
        let v = crate::build_info::client_version();
        assert!(v.starts_with(&format!(
            "anticrumb {} ",
            crate::build_info::display_version()
        )));

        let id = client_identity();
        assert!(!id.is_empty());
        assert!(!id.contains('\n') && !id.contains('\r'));
    }
}
