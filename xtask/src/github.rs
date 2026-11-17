use serde_json::Value;
use std::fmt;
use std::io::Write;
use std::path::Path;
use std::process::Command;

pub const REPOSITORY: &str = "ivere27/anticrumb";
pub const BRANCH: &str = "master";

#[derive(Debug, Clone)]
pub struct PublishError(pub String);

impl fmt::Display for PublishError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PublishError {}

impl PublishError {
    pub fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

#[derive(Debug, Clone)]
pub struct ProcessOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

pub trait Runner {
    fn run_process(&self, args: &[String], root: &Path) -> Result<ProcessOutput, PublishError>;
}

pub struct RealRunner;

impl Runner for RealRunner {
    fn run_process(&self, args: &[String], root: &Path) -> Result<ProcessOutput, PublishError> {
        let (prog, rest) = args
            .split_first()
            .ok_or_else(|| PublishError::new("empty command"))?;
        let output = Command::new(prog)
            .args(rest)
            .current_dir(root)
            .output()
            .map_err(|e| PublishError::new(format!("cannot run {prog}: {e}")))?;
        Ok(ProcessOutput {
            status: output.status.code().unwrap_or(1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

pub fn run_command(
    runner: &dyn Runner,
    args: &[String],
    root: &Path,
) -> Result<String, PublishError> {
    let result = runner.run_process(args, root)?;
    if result.status != 0 {
        let detail = if !result.stderr.trim().is_empty() {
            result.stderr.trim()
        } else {
            result.stdout.trim()
        };
        return Err(PublishError::new(format!(
            "{} command failed: {detail}",
            args.first().map(String::as_str).unwrap_or("command")
        )));
    }
    Ok(result.stdout.trim().to_string())
}

pub fn api(
    runner: &dyn Runner,
    path: &str,
    root: &Path,
    paginate: bool,
) -> Result<Value, PublishError> {
    let mut args: Vec<String> = ["gh", "api", "--hostname", "github.com", path]
        .into_iter()
        .map(str::to_string)
        .collect();
    if paginate {
        args.push("--paginate".to_string());
        args.push("--slurp".to_string());
    }
    let out = run_command(runner, &args, root)?;
    let value: Value = serde_json::from_str(&out)
        .map_err(|_| PublishError::new("GitHub returned an invalid JSON response"))?;
    if paginate {
        let Some(pages) = value.as_array() else {
            return Err(PublishError::new("GitHub returned invalid paginated data"));
        };
        let mut flat = Vec::new();
        for page in pages {
            let Some(entries) = page.as_array() else {
                return Err(PublishError::new("GitHub returned invalid paginated data"));
            };
            flat.extend(entries.iter().cloned());
        }
        return Ok(Value::Array(flat));
    }
    Ok(value)
}

pub fn releases(runner: &dyn Runner, root: &Path) -> Result<Vec<Value>, PublishError> {
    let val = api(
        runner,
        &format!("repos/{REPOSITORY}/releases?per_page=100"),
        root,
        true,
    )?;
    Ok(val.as_array().cloned().unwrap_or_default())
}

fn parse_last_http_status_and_body(stdout: &str) -> Option<(u16, &str)> {
    let mut last_match: Option<(u16, usize)> = None;
    let mut offset = 0usize;
    for line in stdout.split_inclusive('\n') {
        let line_end = offset + line.len();
        if let Some(rest) = line.strip_prefix("HTTP/") {
            if let Some((_, after_proto)) = rest.split_once(char::is_whitespace) {
                let code_part = after_proto.trim_start();
                let digits: String = code_part
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect();
                if digits.len() == 3 {
                    if let Ok(code) = digits.parse::<u16>() {
                        last_match = Some((code, line_end));
                    }
                }
            }
        }
        offset = line_end;
    }
    let (status, after_status_line) = last_match?;
    let tail = &stdout[after_status_line..];
    let body = if let Some(b) = tail.strip_prefix("\r\n") {
        b
    } else if let Some(b) = tail.strip_prefix('\n') {
        b
    } else if let Some((_, b)) = tail.split_once("\r\n\r\n") {
        b
    } else if let Some((_, b)) = tail.split_once("\n\n") {
        b
    } else {
        ""
    };
    Some((status, body))
}

pub fn pages_request(
    runner: &dyn Runner,
    root: &Path,
    method: &str,
) -> Result<Option<Value>, PublishError> {
    let mut args: Vec<String> = [
        "gh",
        "api",
        "--hostname",
        "github.com",
        "--include",
        "--method",
        method,
        &format!("repos/{REPOSITORY}/pages"),
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    if matches!(method, "POST" | "PUT") {
        args.extend([
            "--raw-field".to_string(),
            "build_type=legacy".to_string(),
            "--raw-field".to_string(),
            format!("source[branch]={BRANCH}"),
            "--raw-field".to_string(),
            "source[path]=/".to_string(),
        ]);
    }
    let result = runner.run_process(&args, root)?;
    let Some((status, body)) = parse_last_http_status_and_body(&result.stdout) else {
        let detail = if !result.stderr.trim().is_empty() {
            result.stderr.trim()
        } else {
            "no HTTP response"
        };
        return Err(PublishError::new(format!(
            "cannot check GitHub Pages: {detail}"
        )));
    };
    if method == "GET" && status == 404 {
        return Ok(None);
    }
    if result.status != 0 || !matches!(status, 200 | 201 | 204) {
        let detail = if !result.stderr.trim().is_empty() {
            result.stderr.trim().to_string()
        } else {
            format!("HTTP {status}")
        };
        return Err(PublishError::new(format!(
            "GitHub Pages {method} failed (HTTP {status}): {detail}. \
            Use a gh account with permission to manage Pages (fine-grained tokens need \
            Pages and Administration write permissions), and a public repository \
            or a plan supporting Pages for private repositories. No release was created."
        )));
    }
    if status == 204 {
        return Ok(Some(Value::Object(Default::default())));
    }
    let value: Value = serde_json::from_str(body)
        .map_err(|_| PublishError::new("GitHub Pages returned invalid response data"))?;
    if !value.is_object() {
        return Err(PublishError::new(
            "GitHub Pages returned invalid response data",
        ));
    }
    Ok(Some(value))
}

fn pages_uses_root(page: &Value) -> bool {
    let build_type_ok = match page.get("build_type") {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s == "legacy",
        _ => false,
    };
    let source = page.get("source");
    let branch_ok = source.and_then(|s| s.get("branch")).and_then(Value::as_str) == Some(BRANCH);
    let path_ok = source.and_then(|s| s.get("path")).and_then(Value::as_str) == Some("/");
    build_type_ok && branch_ok && path_ok
}

pub fn ensure_pages(
    runner: &dyn Runner,
    root: &Path,
    out: &mut dyn Write,
) -> Result<(), PublishError> {
    let page = pages_request(runner, root, "GET")?;
    match page {
        None => {
            let mut created = pages_request(runner, root, "POST")?
                .unwrap_or_else(|| Value::Object(Default::default()));
            if !pages_uses_root(&created) {
                created = pages_request(runner, root, "GET")?
                    .unwrap_or_else(|| Value::Object(Default::default()));
            }
            if !pages_uses_root(&created) {
                return Err(PublishError::new(
                    "GitHub did not confirm Pages from master /(root); no release was created",
                ));
            }
            let _ = writeln!(out, "Enabled GitHub Pages from master /(root).");
        }
        Some(existing) if !pages_uses_root(&existing) => {
            pages_request(runner, root, "PUT")?;
            let confirmed = pages_request(runner, root, "GET")?
                .unwrap_or_else(|| Value::Object(Default::default()));
            if !pages_uses_root(&confirmed) {
                return Err(PublishError::new(
                    "GitHub did not confirm Pages from master /(root); no release was created",
                ));
            }
            let _ = writeln!(out, "Configured GitHub Pages to publish master /(root).");
        }
        Some(_) => {}
    }
    Ok(())
}
