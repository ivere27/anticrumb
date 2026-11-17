//! Local-service commands. Configuration stays local; Google credentials stay in Chrome.
use crate::{domain::*, store::Store};
use fs2::FileExt;
use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
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
    if !path.exists() {
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
        self.request(
            "POST",
            &format!("/api/accounts/{account}/clear-history"),
            Some(&json!({})),
        )
    }
    pub fn web_history_cleanup(&self, account: &str) -> Result<Value> {
        self.request(
            "GET",
            &format!("/api/accounts/{account}/clear-history"),
            None,
        )
    }
    pub fn submit(&self, request: &TurnRequest) -> Result<String> {
        let value = self.request(
            "POST",
            "/api/runs",
            Some(&serde_json::to_value(request).map_err(ServiceError::storage)?),
        )?;
        value["id"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| ServiceError::invalid("missing run ID"))
    }
    pub fn wait(&self, id: &str, cancel: &crate::agent::CancelToken) -> Result<TurnResult> {
        let mut cancellation_sent = false;
        poll_until(Duration::from_millis(100), Some(cancel), || {
            if cancel.is_cancelled() && !cancellation_sent {
                self.request("POST", &format!("/api/runs/{id}/cancel"), Some(&json!({})))?;
                cancellation_sent = true;
            }
            let run: Run =
                serde_json::from_value(self.request("GET", &format!("/api/runs/{id}"), None)?)
                    .map_err(ServiceError::storage)?;
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

pub(crate) fn commit_head() -> Option<String> {
    if let Some(hash) = option_env!("ANTICRUMB_GIT_HEAD").map(str::trim) {
        let short: String = hash.chars().take(7).collect();
        if short.len() >= 7 && short.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(short);
        }
    }
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let dot_git = manifest_dir.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else if dot_git.is_file() {
        let content = std::fs::read_to_string(&dot_git).ok()?;
        let raw = content.trim().strip_prefix("gitdir:")?.trim();
        let path = Path::new(raw);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            manifest_dir.join(path)
        }
    } else {
        return None;
    };
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let trimmed = head.trim();
    let full_sha = if let Some(reference) = trimmed.strip_prefix("ref: ") {
        let reference = reference.trim();
        let ref_path = git_dir.join(reference);
        if let Ok(sha) = std::fs::read_to_string(ref_path) {
            sha.trim().to_string()
        } else {
            let packed = std::fs::read_to_string(git_dir.join("packed-refs")).ok()?;
            packed
                .lines()
                .filter(|line| !line.starts_with('#') && !line.starts_with('^'))
                .find_map(|line| {
                    let (sha, name) = line.split_once(' ')?;
                    (name.trim() == reference).then(|| sha.trim().to_string())
                })?
        }
    } else {
        trimmed.to_string()
    };
    let short: String = full_sha.chars().take(7).collect();
    (short.len() == 7 && short.chars().all(|c| c.is_ascii_hexdigit())).then_some(short)
}

pub(crate) fn build_time() -> &'static str {
    option_env!("ANTICRUMB_BUILD_TIME").unwrap_or("unknown")
}

pub(crate) fn help_banner() -> String {
    let version = env!("CARGO_PKG_VERSION");
    let commit = commit_head().unwrap_or_else(|| "unknown".into());
    let built = build_time();
    format!("anticrumb {version} (commit: {commit}, built: {built})")
}

pub fn run(args: &[String]) -> Result<()> {
    if let Some((connection, args)) = crate::rpc::connection::inline(args)? {
        return crate::rpc::workspace_client::with_connection(connection, || run(&args));
    }
    let command = args.first().map(String::as_str).unwrap_or("");
    match command {
        "--version" | "-V" => {
            match commit_head() {
                Some(head) => println!(
                    "anticrumb {} ({}, {})",
                    env!("CARGO_PKG_VERSION"),
                    head,
                    crate::update::TARGET
                ),
                None => println!(
                    "anticrumb {} ({})",
                    env!("CARGO_PKG_VERSION"),
                    crate::update::TARGET
                ),
            }
            Ok(())
        }
        "update" => crate::update::run(&args[1..]),
        "help" | "--help" | "-h" => {
            println!("{}\n", help_banner());
            println!(
                r#"anticrumb [OPTIONS] [PROMPT]              interactive coding agent
  exec [OPTIONS] PROMPT                    run a task and print the answer
  resume [ID | --last]                     continue a conversation
  exec resume [ID | --last] PROMPT         follow up without the TUI

  -m, --model ID      -C, --cd DIR          model and working directory
  -p, --print                              noninteractive final answer
  -c, --continue     -r, --resume ID        reopen a conversation
  --file PATH        -i, --image PATH       native Gemini attachment (repeatable)
  --json                                   NDJSON events for exec/--print
  --thinking LEVEL --pace LEVEL --no-shell --mcp-config FILE --knowledge ID
  --provider ID --account ID --workers N
  --connect https://HOST:PORT --token STRING [--tls-pin SHA256]

Type @path or @"path with spaces" to upload a file, never inline its contents.
Interactive: /help /provider /model /usage /copy /theme /browser /browse /status /resume /new /clear /permissions /pace /prompts /exit
Enter sends; Esc interrupts; Tab completes; Alt+Enter inserts a newline.

Service and integration commands:
  update [--check]                         install or check the latest release
  daemon [--listen IP:PORT] [OPTIONS]       persistent host (see daemon --help)
  browser [OPTIONS] [COMMAND]               stateful browser commands / interactive shell
  chat [--session ID] [--file PATH] PROMPT
  image [--file PATH] PROMPT
  agents list|show|cancel|steer|resume|follow ID
  runs list|show|cancel ID     sessions list|close|media ID
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

Default: automatic local daemon over a Unix socket or named pipe. Remote: daemon --listen / connect.
Home: ANTICRUMB_HOME (default ~/.anticrumb: profile/, accounts/, workspace/, browser/, prompts/, providers/, AGENTS.md)"#
            );
            Ok(())
        }
        "daemon" => crate::rpc::daemon::run(&args[1..]),
        "browser" => crate::browser_control::terminal::run(&args[1..]),
        "serve" => Err(ServiceError::invalid("serve has been merged into daemon. Use `anticrumb` locally or `anticrumb daemon --listen IP:PORT` for network clients.")),
        "mcp" => {
            let client = Client::connect()?;
            let result = client.runtime.block_on(crate::mcp_sdk::serve(client.rpc.clone()));
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
            println!("anticrumb connect https://HOST:PORT --token STRING\n  First connection: compare the SHA256 and emoji fingerprint with the daemon, then confirm.\n  --tls-pin SHA256    preverified certificate for noninteractive setup\n  --ca-file PATH      use an existing trusted CA\n  --server-name NAME override the expected TLS hostname\nanticrumb connect status | local\nanticrumb --connect URL --token STRING [TUI options]\nProvider integrations: connect opencode|codex|claude|antigravity|mcp");
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
            if matches!(args.get(1).map(String::as_str), Some("--help" | "-h" | "help")) {
                println!("{}", crate::store::cache::USAGE);
                return Ok(());
            }
            let options = crate::store::cache::Options::parse(&args[1..].iter().map(String::as_str).collect::<Vec<_>>())?;
            let client = Client::connect()?;
            let report = client.cache(options);
            client.close()?;
            let report = report?;
            print_json(&json!(report));
            if report.stores.iter().any(|store| !store.error.is_empty()) {
                return Err(ServiceError::storage("cache maintenance was incomplete; see each store's error"));
            }
            Ok(())
        }
        "usage" => {
            let mut options = PromptOptions::parse(&args[1..])?;
            if !options.words.is_empty() {
                return Err(ServiceError::invalid("usage [--provider ID] [--account ID]"));
            }
            let client = Client::connect()?;
            options.resolve_account(&client)?;
            print_json(&client.request("GET", &format!("/api/accounts/{}/usage", options.account), None)?);
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
            let response = client.artifact(identifier(&args[2])?, identifier(&args[3])?)?;
            let metadata = response
                .artifact
                .ok_or_else(|| ServiceError::invalid("missing artifact metadata"))?;
            if metadata.hash != digest(&response.data)
                || metadata.size != response.data.len() as u64
            {
                return Err(ServiceError::invalid("artifact integrity mismatch"));
            }
            let mut out = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&args[4])
                .map_err(ServiceError::storage)?;
            out.write_all(&response.data)
                .map_err(ServiceError::storage)?;
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
    pub(crate) no_shell: bool,
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
                "--no-shell" => o.no_shell = true,
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
        if !self.overrides.contains("no_shell") {
            self.no_shell = request.no_shell;
        }
        if !self.overrides.contains("temporary") {
            self.temporary = request.temporary;
        }
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

    pub(crate) fn agent_request(
        &self,
        task: String,
        parent_id: Option<String>,
    ) -> crate::agent_service::AgentRequest {
        crate::agent_service::AgentRequest {
            environment: None,
            task,
            workspace: self.workspace.clone(),
            account_id: self.account.clone(),
            model: self.model.clone(),
            thinking: self.thinking.clone(),
            temporary: self.temporary,
            no_shell: self.no_shell,
            max_steps: 0,
            knowledge: self.knowledge.clone(),
            mcp_config: self.mcp.clone(),
            artifacts: Vec::new(),
            parent_id,
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
fn run_agent(mut o: PromptOptions) -> Result<()> {
    o.apply_worker_override();
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
    if prepared.text.trim().is_empty() {
        return Err(ServiceError::invalid("provide a task or follow-up prompt"));
    }
    let mut request = o.agent_request(prepared.text, parent);
    request.artifacts = client.upload_files(&o.account, &prepared.files, &prepared.temp_files)?;
    let value = client.request(
        "POST",
        "/api/agents",
        Some(&serde_json::to_value(request).map_err(ServiceError::storage)?),
    )?;
    let id = value["id"]
        .as_str()
        .ok_or_else(|| ServiceError::invalid("missing job id"))?;
    eprintln!("agent: {id}");
    follow_agent(&client, id, o.json, o.print)
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
fn chat_command(args: &[String], image: bool) -> Result<()> {
    let mut o = PromptOptions::parse(args)?;
    let client = Client::connect()?;
    o.resolve_account(&client)?;
    let prepared = crate::input::prepare(&o.words.join(" "), &o.workspace, &o.files)?;
    let mut r = TurnRequest::text(prepared.text);
    r.account_id = o.account;
    r.model = o.model;
    r.thinking = o.thinking;
    r.mode = if o.temporary {
        SessionMode::Temporary
    } else {
        SessionMode::Chat
    };
    r.session_id = o.session;
    r.keep_session = o.keep;
    r.timeout_secs = o.timeout;
    r.generate_image = image;
    for path in prepared.files {
        r.artifacts.push(client.upload(&r.account_id, &path)?);
    }
    let id = client.submit(&r)?;
    eprintln!("run: {id}");
    let cancel = crate::agent::CancelToken::new();
    let c = cancel.clone();
    let _ = ctrlc::set_handler(move || c.cancel());
    let result = client.wait(&id, &cancel)?;
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
            eprintln!("Use the token printed by `anticrumb daemon` as ANTICRUMB_TOKEN. Configure the provider client to trust the daemon certificate.");
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
    eprintln!("Use the token printed by `anticrumb daemon` as ANTICRUMB_TOKEN (OpenCode/Codex), ANTHROPIC_AUTH_TOKEN (Claude), or GEMINI_API_KEY (Antigravity). Configure the provider client to trust the daemon certificate.");
    Ok(())
}
impl Client {
    pub fn upload_files(
        &self,
        account: &str,
        files: &[PathBuf],
        temp_files: &[PathBuf],
    ) -> Result<Vec<String>> {
        if files.len() > crate::input::MAX_FILES {
            crate::input::cleanup_temp_files(temp_files);
            return Err(ServiceError::invalid("maximum 10 attachments"));
        }
        let uploaded = files
            .iter()
            .map(|path| self.upload(account, path))
            .collect::<Result<Vec<_>>>();
        crate::input::cleanup_temp_files(temp_files);
        uploaded
    }
    pub fn conversations(&self, workspace: &Path, account: &str) -> Result<Vec<Value>> {
        let value = self.request("GET", "/api/agents", None)?;
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
    pub fn conversation(&self, id: &str, workspace: &Path, account: &str) -> Result<Value> {
        if id == "last" || id.is_empty() {
            return self
                .conversations(workspace, account)?
                .into_iter()
                .next()
                .ok_or_else(|| {
                    ServiceError::invalid("no saved conversations in this working directory")
                });
        }
        self.request("GET", &format!("/api/agents/{}", identifier(id)?), None)
    }
    pub fn delete_conversation(&self, id: &str, workspace: &Path, account: &str) -> Result<()> {
        let id = identifier(id)?;
        let job = self.request("GET", &format!("/api/agents/{id}"), None)?;
        if job["request"]["workspace"].as_str().map(Path::new) != Some(workspace)
            || job["request"]["account_id"] != account
        {
            return Err(ServiceError::invalid(
                "conversation deletion requires the same workspace and account",
            ));
        }
        self.request("DELETE", &format!("/api/agents/{id}"), None)?;
        Ok(())
    }
    pub fn events(&self, id: &str, after: i64) -> Result<Vec<Event>> {
        serde_json::from_value(self.request(
            "GET",
            &format!("/api/events/{id}?after={after}"),
            None,
        )?)
        .map_err(ServiceError::storage)
    }
    pub fn upload(&self, account: &str, path: &Path) -> Result<String> {
        let path = crate::input::file(Path::new("."), path)?;
        let extension = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_lowercase();
        let mime = match extension.as_str() {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "webp" => "image/webp",
            "pdf" => "application/pdf",
            "txt" | "md" | "rs" | "py" | "js" => "text/plain",
            _ => "application/octet-stream",
        };
        self.put_file(account, &path, mime)
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_banner_contains_version_commit_and_build_time() {
        let banner = help_banner();
        assert!(banner.starts_with(&format!("anticrumb {}", env!("CARGO_PKG_VERSION"))));
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
        assert_eq!(options.workers, Some(4));
        assert_eq!(options.pace, Some(crate::gemini::HumanPace::High));
        assert_eq!(options.words, vec!["question"]);
        let previous = options.agent_request("earlier".into(), None);
        let mut previous = previous;
        previous.model = Some("old".into());
        previous.no_shell = false;
        let job = json!({ "request": previous });
        options.inherit_from_job(&job).unwrap();
        assert_eq!(options.model.as_deref(), Some("selected"));
        assert!(options.no_shell);
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
}
