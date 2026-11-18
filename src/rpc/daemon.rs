pub use super::private_write;
pub(super) use super::server_cmd::certificate_validity_epochs;
pub use super::server_cmd::{certificate_fingerprint, state_dir, Options};
use crate::domain::{Result, ServiceError};

#[derive(Default)]
pub struct DaemonOptions {
    pub managed: bool,
    pub workers: Option<usize>,
    pub pace: Option<crate::gemini::HumanPace>,
}

impl DaemonOptions {
    pub fn parse(args: &[String]) -> Result<Self> {
        let mut options = Self::default();
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            let value = |args: &mut std::slice::Iter<'_, String>| {
                args.next()
                    .cloned()
                    .ok_or_else(|| ServiceError::invalid(format!("missing value for {arg}")))
            };
            match arg.as_str() {
                "--managed" => options.managed = true,
                "--workers" => {
                    let workers: usize = value(&mut args)?
                        .parse()
                        .map_err(|_| ServiceError::invalid("--workers must be 1..16"))?;
                    if !(1..=16).contains(&workers) {
                        return Err(ServiceError::invalid("--workers must be 1..16"));
                    }
                    options.workers = Some(workers);
                }
                "--pace" => {
                    let raw = value(&mut args)?;
                    let pace = crate::gemini::HumanPace::parse(&raw).ok_or_else(|| {
                        ServiceError::invalid("--pace must be fast, low, or high")
                    })?;
                    options.pace = Some(pace);
                }
                "--listen" | "--state-dir" | "--token" | "--regenerate-token" | "--tls-cert"
                | "--tls-key" | "--tls-self-signed" | "--regenerate-tls" | "--tls-san" => {
                    return Err(ServiceError::invalid(
                        "network, state-dir, token, and TLS options belong to `anticrumb server`; `anticrumb daemon` only runs the local IPC daemon (use ANTICRUMB_HOME to isolate local state)",
                    ));
                }
                _ => {
                    return Err(ServiceError::invalid(
                        "unknown daemon option; use `anticrumb daemon --help`",
                    ))
                }
            }
        }
        Ok(options)
    }
}

pub fn run(args: &[String]) -> Result<()> {
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h" | "help"))
    {
        println!("Run a persistent local IPC daemon:\n  anticrumb daemon\n\nNormal anticrumb commands connect through a Unix socket or Windows named pipe.\nThey start a daemon automatically when needed; the automatic daemon exits after\nits last client disconnects. An explicitly started daemon stays running.\nLocal connections use OS user permissions and need no token or TLS setup.\n\nOptions:\n  --workers N    TempChat tabs per account (default 2)\n  --pace LEVEL   fast, low (default), high\n\nTo expose a browser server over the network, run `anticrumb server` (see `anticrumb server --help`).\nUse ANTICRUMB_HOME for a separate local daemon and state directory.");
        return Ok(());
    }
    let options = DaemonOptions::parse(args)?;
    if let Some(workers) = options.workers {
        crate::tab_pool::set_worker_tabs(None, workers);
    }
    if let Some(pace) = options.pace {
        crate::gemini::set_human_pace(None, pace);
    }
    tokio::runtime::Runtime::new()
        .map_err(ServiceError::storage)?
        .block_on(super::local::run(options.managed, None))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<DaemonOptions> {
        let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
        DaemonOptions::parse(&args)
    }

    #[test]
    fn local_daemon_accepts_local_flags_and_rejects_network_options() {
        let defaults = parse(&[]).unwrap();
        assert!(!defaults.managed);
        assert!(defaults.workers.is_none());
        assert!(defaults.pace.is_none());

        let configured = parse(&["--managed", "--workers", "4", "--pace", "fast"]).unwrap();
        assert!(configured.managed);
        assert_eq!(configured.workers, Some(4));

        for rejected in [
            vec!["--listen"],
            vec!["--listen", "127.0.0.1:50051"],
            vec!["--state-dir", "/tmp/server"],
            vec!["--token", "0123456789abcdef0123456789abcdef"],
            vec!["--regenerate-token"],
            vec!["--tls-self-signed"],
            vec!["--regenerate-tls"],
            vec!["--tls-san", "localhost"],
            vec!["--tls-cert", "cert.pem", "--tls-key", "key.pem"],
            vec!["--ipc"],
            vec!["127.0.0.1:50051"],
        ] {
            assert!(parse(&rejected).is_err(), "{rejected:?}");
        }
    }
}
