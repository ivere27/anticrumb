//! A text command is parsed once, then follows the same contract as an agent call.
use super::{format_response, request, validate_args, workspace_session, HELP};
use crate::{
    agent::CancelToken,
    cli::Client,
    domain::{Result, ServiceError},
};
use serde_json::{json, Value};
use std::{
    io::{self, BufRead, IsTerminal, Write},
    path::PathBuf,
    sync::mpsc,
    time::Duration,
};

/// Shell-like quoting only. Deliberately never expands variables, paths, or commands.
pub fn words(line: &str) -> std::result::Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut started = false;
    for c in line.chars() {
        if escaped {
            word.push(c);
            escaped = false;
            started = true;
            continue;
        }
        if c == '\\' && quote != Some('\'') {
            escaped = true;
            started = true;
            continue;
        }
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else {
                word.push(c);
            }
            started = true;
            continue;
        }
        match c {
            '\'' | '"' => {
                quote = Some(c);
                started = true;
            }
            c if c.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            _ => {
                word.push(c);
                started = true;
            }
        }
    }
    if quote.is_some() || escaped {
        return Err("unterminated quote or escape in browser command".into());
    }
    if started {
        words.push(word);
    }
    Ok(words)
}
fn target(word: &str) -> Value {
    if let Some(r) = word.strip_prefix('@') {
        json!({"ref":r})
    } else if let Some(css) = word.strip_prefix("css:") {
        json!({"css":css})
    } else if let Some((role, name)) = word.strip_prefix("role:").and_then(|s| s.split_once(':')) {
        json!({"role":role,"name":name})
    } else {
        json!({"name":word})
    }
}
pub fn parse(line: &str) -> std::result::Result<Value, String> {
    if line.trim_start().starts_with('{') {
        let args: Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
        validate_args(&args)?;
        return Ok(args);
    }
    parse_words(&words(line)?)
}
pub fn parse_words(words: &[String]) -> std::result::Result<Value, String> {
    let mut args = json!({});
    let mut i = 0;
    // Options precede the verb; option-looking form text is always literal.
    while let Some(option) = words.get(i).filter(|s| s.starts_with("--")) {
        let key = match option.as_str() {
            "--session" => "session",
            "--tab" => "tab",
            "--timeout-ms" => "timeout_ms",
            "--max-chars" => "max_chars",
            "--wait-ms" => "wait_ms",
            _ => return Err(format!("unknown browser option '{option}'")),
        };
        let value = words
            .get(i + 1)
            .ok_or_else(|| format!("missing value for {option}"))?;
        args[key] = if matches!(key, "timeout_ms" | "max_chars" | "wait_ms") {
            json!(value
                .parse::<u32>()
                .map_err(|_| format!("invalid integer for {option}"))?)
        } else {
            json!(value)
        };
        i += 2;
    }
    let action = words.get(i).map(String::as_str).unwrap_or("help");
    let rest = words.get(i + 1..).unwrap_or_default();
    args["action"] = json!(action);
    let arity = |min: usize, max: usize| -> std::result::Result<(), String> {
        if !(min..=max).contains(&rest.len()) {
            Err(format!("wrong arguments for '{action}'; use help"))
        } else {
            Ok(())
        }
    };
    let number = |i: usize| -> std::result::Result<f64, String> {
        let n = rest[i]
            .parse::<f64>()
            .map_err(|_| "expected a numeric coordinate or delta")?;
        if !n.is_finite() {
            return Err("coordinates and deltas must be finite".into());
        }
        Ok(n)
    };
    match action {
        "screenshot" => {
            if !rest.is_empty() && rest.len() != 4 {
                return Err("screenshot expects no arguments or X Y WIDTH HEIGHT".into());
            }
            if rest.len() == 4 {
                args["region"] =
                    json!({"x":number(0)?,"y":number(1)?,"width":number(2)?,"height":number(3)?});
            }
        }
        "click-point" | "wheel" => {
            let n = if action == "wheel" { 5 } else { 3 };
            arity(n, n)?;
            args["action"] = json!(if action == "click-point" {
                "click"
            } else {
                "wheel"
            });
            args["point"] = json!({"screenshot_id":rest[0],"x":number(1)?,"y":number(2)?});
            if action == "wheel" {
                args["delta_x"] = json!(number(3)?);
                args["delta_y"] = json!(number(4)?);
            }
        }
        "help" | "tabs" | "snapshot" | "back" | "forward" | "reload" | "close" | "reset" => {
            arity(0, 0)?
        }
        "open" | "navigate" => {
            arity(1, 1)?;
            args["url"] = json!(rest[0]);
        }
        "use" => {
            arity(1, 1)?;
            args["tab"] = json!(rest[0]);
        }
        "click" => {
            arity(1, 1)?;
            args["target"] = target(&rest[0]);
        }
        "fill" | "type" | "select" => {
            arity(if action == "type" { 1 } else { 2 }, 2)?;
            if rest.len() == 2 {
                args["target"] = target(&rest[0]);
            }
            args[if action == "select" { "value" } else { "text" }] = json!(rest.last().unwrap());
        }
        "press" => {
            arity(1, 2)?;
            let mut keys = rest[0].split('+').collect::<Vec<_>>();
            if rest[0] == "+" {
                keys = vec!["+"];
            }
            args["key"] = json!(keys.pop().unwrap());
            if !keys.is_empty() {
                let modifiers = keys
                    .iter()
                    .map(|m| match *m {
                        "Ctrl" | "Control" => Ok("Control"),
                        "Cmd" | "Meta" => Ok("Meta"),
                        "Alt" => Ok("Alt"),
                        "Shift" => Ok("Shift"),
                        _ => Err("unknown key modifier"),
                    })
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                args["modifiers"] = json!(modifiers);
            }
            if rest.len() == 2 {
                args["target"] = target(&rest[1]);
            }
        }
        "wait" => {
            arity(1, 2)?;
            args["target"] = target(&rest[0]);
            if rest.len() == 2 {
                args["state"] = json!(rest[1]);
            }
        }
        "scroll" => {
            arity(1, 2)?;
            let amount = rest
                .get(1)
                .map(|s| s.parse::<i32>())
                .transpose()
                .map_err(|_| "scroll pixels must be a positive integer")?
                .unwrap_or(600);
            if amount <= 0 || amount > 100000 {
                return Err("scroll pixels must be 1..100000".into());
            }
            let (key, sign) = match rest[0].as_str() {
                "up" => ("y", -1),
                "down" => ("y", 1),
                "left" => ("x", -1),
                "right" => ("x", 1),
                _ => return Err("scroll direction must be up, down, left, or right".into()),
            };
            args[key] = json!(amount * sign);
        }
        _ => return Err(format!("unknown browser command '{action}'; use help")),
    }
    validate_args(&args)?;
    Ok(args)
}

pub fn run(input: &[String]) -> Result<()> {
    let mut i = 0;
    let mut json_output = false;
    let mut account = "default".to_string();
    let mut workspace = std::env::current_dir().map_err(ServiceError::storage)?;
    let mut defaults = json!({});
    while let Some(option) = input.get(i).filter(|s| s.starts_with('-')) {
        if matches!(option.as_str(), "--help" | "-h") {
            println!("{HELP}\nanticrumb browser [--json] [--account ID] [-C DIR] [OPTIONS] [COMMAND]\nWithout COMMAND, read commands until exit or EOF; --json emits one JSON object per command.\nOptions precede COMMAND. State is shared by clients on this machine. Automatic daemons exit after their last client; start `anticrumb daemon` to keep browser sessions alive.");
            return Ok(());
        }
        if option == "--json" {
            json_output = true;
            i += 1;
            continue;
        }
        let value = input
            .get(i + 1)
            .ok_or_else(|| ServiceError::invalid(format!("missing value for {option}")))?;
        match option.as_str() {
            "--account" => account = value.clone(),
            "-C" | "--cd" => workspace = PathBuf::from(value),
            "--session" => defaults["session"] = json!(value),
            "--tab" => defaults["tab"] = json!(value),
            "--timeout-ms" | "--max-chars" | "--wait-ms" => {
                defaults[if option == "--timeout-ms" {
                    "timeout_ms"
                } else if option == "--max-chars" {
                    "max_chars"
                } else {
                    "wait_ms"
                }] = json!(value
                    .parse::<u32>()
                    .map_err(|_| ServiceError::invalid("expected integer option"))?);
            }
            _ => {
                return Err(ServiceError::invalid(format!(
                    "unknown browser option '{option}'"
                )))
            }
        }
        i += 2;
    }
    let workspace = std::fs::canonicalize(workspace).map_err(ServiceError::storage)?;
    if !workspace.is_dir() {
        return Err(ServiceError::invalid(
            "browser workspace must be a directory",
        ));
    }
    let session = workspace_session(&workspace);
    let execute = |client: &Client, args: Value, cancel: &CancelToken| -> Result<()> {
        let mut merged = defaults.clone();
        for (key, value) in args.as_object().unwrap() {
            merged[key] = value.clone();
        }
        validate_args(&merged).map_err(ServiceError::invalid)?;
        if merged["action"] == "help" {
            if json_output {
                println!("{}", json!({"ok":true,"help":HELP}));
            } else {
                println!("{HELP}");
            }
            return Ok(());
        }
        let req = request(&merged, &account, &session).map_err(ServiceError::invalid)?;
        let result = client.browser_control(req, cancel)?;
        if json_output {
            println!("{}", json!({"ok":true,"result":result}));
        } else {
            println!("{}", format_response(&result));
        }
        Ok(())
    };
    // Help does not start Chrome, an account, or a workspace runtime.
    if input.get(i).is_some_and(|s| s == "help") {
        let _ = parse_words(&input[i..]).map_err(ServiceError::invalid)?;
        println!("{HELP}");
        return Ok(());
    }
    let cancel = CancelToken::new();
    let signal = cancel.clone();
    let _ = ctrlc::set_handler(move || signal.cancel());
    let client = Client::connect()?;
    let result = if i < input.len() {
        execute(
            &client,
            parse_words(&input[i..]).map_err(ServiceError::invalid)?,
            &cancel,
        )
    } else {
        let interactive = io::stdin().is_terminal() && !json_output;
        if interactive {
            println!("Browser session {session}. Type help for commands; exit to leave.");
        }
        let (tx, rx) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            for line in io::stdin().lock().lines() {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut failed = false;
        loop {
            if interactive {
                print!("browser> ");
                io::stdout().flush().map_err(ServiceError::storage)?;
            }
            let line = loop {
                if cancel.is_cancelled() {
                    break None;
                }
                match rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(line) => break Some(line),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(_) => break None,
                }
            };
            let Some(line) = line else {
                break;
            };
            let line = line.map_err(ServiceError::storage)?;
            if matches!(line.trim(), "exit" | "quit") {
                break;
            }
            if line.trim().is_empty() {
                continue;
            }
            let result = parse(&line)
                .map_err(ServiceError::invalid)
                .and_then(|args| execute(&client, args, &cancel));
            if let Err(error) = result {
                failed = true;
                if json_output {
                    println!("{}", json!({"ok":false,"error":error}));
                } else {
                    eprintln!("{error}");
                }
            }
        }
        if failed {
            Err(ServiceError::invalid("one or more browser commands failed"))
        } else {
            Ok(())
        }
    };
    let closed = client.close();
    result.and(closed)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn terminal_commands_share_agent_validation_and_preserve_literal_input() {
        assert_eq!(
            parse("fill 'Search field' ''").unwrap(),
            json!({"action":"fill","target":{"name":"Search field"},"text":""})
        );
        assert_eq!(
            parse("fill Search '--session' ").unwrap()["text"],
            "--session"
        );
        assert_eq!(
            parse("fill Search '$(echo secret) $HOME `date`'").unwrap()["text"],
            "$(echo secret) $HOME `date`"
        );
        assert_eq!(
            parse("--session shopping --tab tab_1 click @e123-2").unwrap(),
            json!({"session":"shopping","tab":"tab_1","action":"click","target":{"ref":"e123-2"}})
        );
        assert_eq!(
            parse("click 'role:button:Sign in'").unwrap()["target"],
            json!({"role":"button","name":"Sign in"})
        );
        assert_eq!(parse("scroll left 700").unwrap()["x"], -700);
        assert_eq!(
            parse("type 'Unicode 🌍 input'").unwrap(),
            json!({"action":"type","text":"Unicode 🌍 input"})
        );
        assert_eq!(
            parse("press Ctrl+A").unwrap(),
            json!({"action":"press","key":"A","modifiers":["Control"]})
        );
        assert_eq!(
            parse("press Shift+Tab").unwrap()["modifiers"],
            json!(["Shift"])
        );
        assert_eq!(
            parse("--wait-ms 150 screenshot 10 20 300 200").unwrap()["region"],
            json!({"x":10.0,"y":20.0,"width":300.0,"height":200.0})
        );
        assert_eq!(
            parse("click-point shot_1 12 24").unwrap(),
            json!({"action":"click","point":{"screenshot_id":"shot_1","x":12.0,"y":24.0}})
        );
        assert_eq!(parse("wheel shot_1 12 24 0 500").unwrap()["delta_y"], 500.0);
        assert_eq!(
            parse("press Enter css:#search").unwrap()["target"],
            json!({"css":"#search"})
        );
        for line in [
            "fill Search",
            "click 'unclosed",
            "click a b",
            "scroll down -1",
            "--max-chars 999999 snapshot",
            "snapshot --bogus",
            "press unknown",
            "use",
            "screenshot 1 2",
            "screenshot 1 2 0 200",
            "click-point shot_1 NaN 10",
            "wheel shot_1 10 10 0 0",
            "press Unknown+A",
            "{\"action\":\"wait\"}",
        ] {
            assert!(parse(line).is_err(), "{line}");
        }
    }
}
