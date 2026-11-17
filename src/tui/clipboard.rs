//! Native clipboard access and an explicit terminal clipboard fallback.
use base64::Engine;
use std::{
    io::{self, IsTerminal, Read, Write},
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

const MAX_BYTES: usize = 48_000;
const PASTE_HINT: &str = "Use your terminal's paste shortcut (Ctrl+Shift+V or Shift+Insert).";

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CopyOutcome {
    Copied,
    /// The TUI must write and flush this on its main thread. OSC 52 does not
    /// acknowledge success: report a request, not a confirmed clipboard write.
    TerminalSequence(String),
}

/// Copy complete UTF-8 text, preserving whitespace and final newlines. Clipboard
/// helpers receive text only on stdin, never in process arguments or a shell.
pub(crate) fn write_text(text: &str) -> Result<CopyOutcome, String> {
    #[cfg(target_os = "linux")]
    let (commands, hint) = {
        let present = |key| std::env::var_os(key).is_some_and(|value| !value.is_empty());
        let wayland = present("WAYLAND_DISPLAY");
        let x11 = present("DISPLAY");
        (linux_write_commands(wayland, x11),
            "Install wl-clipboard for Wayland, or xclip/xsel for X11, and check your display connection.")
    };
    #[cfg(target_os = "macos")]
    let (commands, hint): (Vec<ClipboardCommand>, &str) = (
        vec![("pbcopy", &[])],
        "Check that pbcopy is available in your macOS login session.",
    );
    #[cfg(target_os = "windows")]
    let (commands, hint): (Vec<ClipboardCommand>, &str) = (vec![("powershell.exe", &[
        "-NoLogo", "-NoProfile", "-NonInteractive", "-STA", "-Command",
        "$ErrorActionPreference = 'Stop'; [Console]::InputEncoding = [System.Text.UTF8Encoding]::new($false); Set-Clipboard -Value ([Console]::In.ReadToEnd())",
    ])], "Check that PowerShell Set-Clipboard is available in your Windows login session.");
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    let (commands, hint): (Vec<ClipboardCommand>, &str) = (
        vec![],
        "This platform has no supported desktop clipboard helper.",
    );

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut errors = Vec::new();
    for (program, args) in commands {
        let mut command = Command::new(program);
        command.args(args);
        #[cfg(target_os = "linux")]
        command.env_remove("LC_ALL").env("LC_CTYPE", "C.UTF-8");
        #[cfg(target_os = "macos")]
        command.env_remove("LC_ALL").env("LC_CTYPE", "en_US.UTF-8");
        match write_command(&mut command, text, deadline) {
            Ok(()) => return Ok(CopyOutcome::Copied),
            Err(error) => {
                errors.push(format!("{program}: {error}"));
                if error.kind() == io::ErrorKind::TimedOut {
                    break;
                }
            }
        }
    }
    let ssh = ["SSH_TTY", "SSH_CONNECTION", "SSH_CLIENT"]
        .iter()
        .any(|key| std::env::var_os(key).is_some_and(|value| !value.is_empty()));
    let terminal =
        std::env::var_os("TERM").is_some_and(|value| !value.is_empty() && value != "dumb");
    if ssh && terminal && io::stdout().is_terminal() {
        return osc52_sequence(text).map(CopyOutcome::TerminalSequence);
    }
    let reason = if errors.is_empty() {
        "no desktop clipboard is available in this session".into()
    } else {
        errors.join("; ")
    };
    Err(format!("Could not copy text ({reason}). {hint} For terminal selection, hold Shift while dragging and use your terminal's copy shortcut."))
}

type ClipboardCommand = (&'static str, &'static [&'static str]);

#[cfg(target_os = "linux")]
fn linux_write_commands(wayland: bool, x11: bool) -> Vec<ClipboardCommand> {
    let mut commands: Vec<ClipboardCommand> = Vec::new();
    if wayland {
        commands.push(("wl-copy", &["--type", "text/plain;charset=utf-8"]));
    }
    if x11 {
        commands.push((
            "xclip",
            &["-selection", "clipboard", "-in", "-target", "UTF8_STRING"],
        ));
        commands.push(("xsel", &["--clipboard", "--input"]));
    }
    commands
}

fn osc52_sequence(text: &str) -> Result<String, String> {
    // Keep the encoded request below common terminal control-string bounds.
    // Native clipboard helpers do not impose this fallback-only size limit.
    if text.len() > 64 * 1024 {
        return Err("Text exceeds the terminal clipboard request limit (64 KiB). Use a desktop clipboard helper or your terminal's copy shortcut.".into());
    }
    Ok(format!(
        "\x1b]52;c;{}\x07",
        base64::engine::general_purpose::STANDARD.encode(text.as_bytes())
    ))
}

fn write_command(command: &mut Command, text: &str, deadline: Instant) -> io::Result<()> {
    if Instant::now() >= deadline {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "clipboard write timed out",
        ));
    }
    // A blocked helper and any child retaining its input pipe must be stopped
    // together on timeout. Successful clipboard owners remain running normally.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdin = child.stdin.take().expect("piped clipboard stdin");
    let bytes = text.as_bytes().to_vec();
    let (tx, rx) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let result = stdin.write_all(&bytes).and_then(|_| stdin.flush());
        drop(stdin);
        let _ = tx.send(result);
    });
    let result = (|| {
        let mut written = false;
        loop {
            if let Ok(result) = rx.try_recv() {
                result?;
                written = true;
            }
            if let Some(status) = child.try_wait()? {
                if !status.success() {
                    return Err(io::Error::other(format!(
                        "clipboard utility exited with {status}"
                    )));
                }
                if written {
                    return Ok(());
                }
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "clipboard write timed out",
                ));
            }
            thread::sleep(Duration::from_millis(5));
        }
    })();
    if result.is_err() {
        #[cfg(unix)]
        unsafe {
            libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
        }
        stop(child);
    }
    result
}

pub(super) fn read_text() -> Result<String, String> {
    #[cfg(target_os = "linux")]
    let commands: &[(&str, &[&str])] = {
        let graphical = ["WAYLAND_DISPLAY", "DISPLAY"]
            .iter()
            .any(|key| std::env::var_os(key).is_some_and(|value| !value.is_empty()));
        if !graphical {
            return Err(format!(
                "No desktop clipboard is available in this session. {PASTE_HINT}"
            ));
        }
        &[
            ("wl-paste", &["--no-newline"]),
            ("xclip", &["-selection", "clipboard", "-o"]),
            ("xsel", &["--clipboard", "--output"]),
        ]
    };
    #[cfg(target_os = "macos")]
    let commands: &[(&str, &[&str])] = &[("pbpaste", &[])];
    #[cfg(target_os = "windows")]
    let commands: &[(&str, &[&str])] = &[(
        "powershell.exe",
        &[
            "-NoProfile", "-NonInteractive", "-Command",
            "$ErrorActionPreference = 'Stop'; [Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false); [Console]::Write((Get-Clipboard -Raw))",
        ],
    )];
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    let commands: &[(&str, &[&str])] = &[];

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut reason = "No supported clipboard utility is installed".to_owned();
    for (program, args) in commands {
        match read_command(Command::new(program).args(*args), deadline) {
            Ok(text) => return Ok(text),
            Err(error) => {
                reason = format!("{program}: {error}");
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::InvalidData
                ) {
                    break;
                }
            }
        }
    }
    Err(format!("Could not read clipboard ({reason}). {PASTE_HINT}"))
}

fn read_command(command: &mut Command, deadline: Instant) -> io::Result<String> {
    if Instant::now() >= deadline {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "clipboard read timed out",
        ));
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stdout = child.stdout.take().expect("piped clipboard stdout");
    let (tx, rx) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout
            .take(MAX_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = tx.send(result);
    });
    let result = (|| {
        let mut output = None;
        loop {
            if let Ok(bytes) = rx.try_recv() {
                let bytes = bytes?;
                if bytes.len() > MAX_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "clipboard exceeds 48,000 bytes",
                    ));
                }
                output = Some(bytes);
            }
            if let Some(status) = child.try_wait()? {
                if !status.success() {
                    return Err(io::Error::other(format!(
                        "clipboard utility exited with {status}"
                    )));
                }
                if let Some(bytes) = output.take() {
                    return String::from_utf8(bytes).map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidData, "clipboard text is not UTF-8")
                    });
                }
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "clipboard read timed out",
                ));
            }
            thread::sleep(Duration::from_millis(5));
        }
    })();
    if result.is_err() {
        stop(child);
    }
    result
}

fn stop(mut child: Child) {
    let _ = child.kill();
    let deadline = Instant::now() + Duration::from_millis(100);
    loop {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        if Instant::now() >= deadline {
            // Reap an unusually slow child without holding up the terminal.
            thread::spawn(move || {
                let _ = child.wait();
            });
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn clipboard_command_preserves_text_and_enforces_size_limit() {
        for input in [
            "  \u{754c}\u{8a9e}\nsecond line\n\n".to_owned(),
            "x".repeat(MAX_BYTES),
        ] {
            let actual = read_command(
                Command::new("printf").args(["%s", &input]),
                Instant::now() + Duration::from_secs(1),
            )
            .unwrap();
            assert_eq!(actual, input);
        }
        let error = read_command(
            Command::new("printf").args(["%s", &"x".repeat(MAX_BYTES + 1)]),
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn clipboard_command_times_out_without_waiting_for_exit() {
        let started = Instant::now();
        let error = read_command(
            Command::new("sleep").arg("5"),
            started + Duration::from_millis(25),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn clipboard_write_preserves_unicode_whitespace_and_large_content() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("copied.txt");
        for input in [
            "  \u{754c}\u{8a9e}\nsecond line\n\n".to_owned(),
            "line \u{754c}\u{8a9e}\n".repeat(20_000),
            String::new(),
        ] {
            write_command(
                Command::new("sh")
                    .args(["-c", "cat > \"$1\"", "clipboard-fixture"])
                    .arg(&output),
                &input,
                Instant::now() + Duration::from_secs(2),
            )
            .unwrap();
            assert_eq!(std::fs::read_to_string(&output).unwrap(), input);
        }
    }

    #[test]
    fn clipboard_write_reports_failure_and_times_out_with_blocked_stdin() {
        assert!(write_command(
            Command::new("sh").args(["-c", "exit 7"]),
            "fixture",
            Instant::now() + Duration::from_secs(1),
        )
        .is_err());
        let started = Instant::now();
        let error = write_command(
            Command::new("sleep").arg("5"),
            &"x".repeat(1_000_000),
            started + Duration::from_millis(25),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn terminal_copy_request_encodes_controls_and_enforces_its_own_limit() {
        let input = "\u{754c}\u{8a9e}\n\x1b]52;c;injected\x07\n";
        let sequence = osc52_sequence(input).unwrap();
        let encoded = sequence
            .strip_prefix("\x1b]52;c;")
            .unwrap()
            .strip_suffix('\x07')
            .unwrap();
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .unwrap(),
            input.as_bytes()
        );
        assert!(!encoded.contains(['\x1b', '\x07', '\n']));
        assert!(osc52_sequence(&"x".repeat(64 * 1024)).is_ok());
        assert!(osc52_sequence(&"x".repeat(64 * 1024 + 1)).is_err());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn native_writers_follow_the_available_display_protocols() {
        assert!(linux_write_commands(false, false).is_empty());
        assert_eq!(
            linux_write_commands(true, false)
                .iter()
                .map(|c| c.0)
                .collect::<Vec<_>>(),
            ["wl-copy"]
        );
        assert_eq!(
            linux_write_commands(false, true)
                .iter()
                .map(|c| c.0)
                .collect::<Vec<_>>(),
            ["xclip", "xsel"]
        );
        assert_eq!(
            linux_write_commands(true, true)
                .iter()
                .map(|c| c.0)
                .collect::<Vec<_>>(),
            ["wl-copy", "xclip", "xsel"]
        );
    }
}
