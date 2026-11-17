#[cfg(unix)]
mod unix_pty {
    use base64::Engine as _;
    use serde_json::Value;
    use std::collections::BTreeSet;
    use std::fs;
    use std::os::fd::FromRawFd;
    use std::os::unix::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};
    use tempfile::TempDir;
    use unicode_width::UnicodeWidthStr;

    struct PtyHarness {
        master: libc::c_int,
        child: Child,
        output: String,
        pending_utf8: Vec<u8>,
    }

    impl PtyHarness {
        fn spawn(
            binary: &Path,
            state: &Path,
            workspace: &Path,
            extra_env: &[(&str, Option<&str>)],
        ) -> Self {
            let mut master: libc::c_int = -1;
            let mut slave: libc::c_int = -1;
            let ws = libc::winsize {
                ws_row: 26,
                ws_col: 110,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            #[cfg(target_os = "macos")]
            let mut ws = ws;
            #[cfg(target_os = "macos")]
            let winp: *mut libc::winsize = &mut ws;
            #[cfg(not(target_os = "macos"))]
            let winp: *const libc::winsize = &ws;
            let rc = unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    winp,
                )
            };
            assert_eq!(rc, 0, "openpty failed: {}", std::io::Error::last_os_error());
            unsafe {
                libc::fcntl(master, libc::F_SETFD, libc::FD_CLOEXEC);
            }

            let fd_in = unsafe { libc::dup(slave) };
            let fd_out = unsafe { libc::dup(slave) };
            let fd_err = unsafe { libc::dup(slave) };
            assert!(fd_in >= 0 && fd_out >= 0 && fd_err >= 0);

            let mut cmd = Command::new(binary);
            cmd.arg(state).args(["-C"]).arg(workspace);
            for (k, v) in extra_env {
                match v {
                    Some(val) => {
                        cmd.env(k, val);
                    }
                    None => {
                        cmd.env_remove(k);
                    }
                }
            }
            unsafe {
                cmd.stdin(Stdio::from_raw_fd(fd_in))
                    .stdout(Stdio::from_raw_fd(fd_out))
                    .stderr(Stdio::from_raw_fd(fd_err));
                cmd.pre_exec(|| {
                    if libc::setsid() == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    #[cfg(target_os = "macos")]
                    const TIOCSCTTY_REQ: libc::c_ulong = 0x2000_7461;
                    #[cfg(not(target_os = "macos"))]
                    let tiocsctty_req = libc::TIOCSCTTY;
                    #[cfg(target_os = "macos")]
                    let tiocsctty_req = TIOCSCTTY_REQ;
                    let _ = libc::ioctl(0, tiocsctty_req, 0 as libc::c_int);
                    Ok(())
                });
            }
            let child = cmd.spawn().expect("spawn tui_fixture");
            unsafe { libc::close(slave) };

            Self {
                master,
                child,
                output: String::new(),
                pending_utf8: Vec::new(),
            }
        }

        fn pump_utf8(&mut self, bytes: &[u8]) {
            self.pending_utf8.extend_from_slice(bytes);
            loop {
                match std::str::from_utf8(&self.pending_utf8) {
                    Ok(valid) => {
                        self.output.push_str(valid);
                        self.pending_utf8.clear();
                        break;
                    }
                    Err(e) => {
                        let valid_len = e.valid_up_to();
                        if valid_len > 0 {
                            let valid =
                                std::str::from_utf8(&self.pending_utf8[..valid_len]).unwrap();
                            self.output.push_str(valid);
                            self.pending_utf8.drain(..valid_len);
                            continue;
                        }
                        if let Some(err_len) = e.error_len() {
                            self.output.push('\u{FFFD}');
                            self.pending_utf8.drain(..err_len);
                        } else {
                            break;
                        }
                    }
                }
            }
        }

        fn wait_for(&mut self, text: &str) -> String {
            let start = self.output.len();
            let until = Instant::now() + Duration::from_secs(5);
            let mut buf = [0u8; 65536];
            while Instant::now() < until {
                let mut pfd = libc::pollfd {
                    fd: self.master,
                    events: libc::POLLIN,
                    revents: 0,
                };
                let ready = unsafe { libc::poll(&mut pfd, 1, 30) };
                if ready > 0 && (pfd.revents & (libc::POLLIN | libc::POLLHUP)) != 0 {
                    let n = unsafe {
                        libc::read(
                            self.master,
                            buf.as_mut_ptr().cast::<libc::c_void>(),
                            buf.len(),
                        )
                    };
                    if n > 0 {
                        self.pump_utf8(&buf[..n as usize]);
                    }
                }
                let complete_frame =
                    self.output.rfind("\x1b[?2026h") <= self.output.rfind("\x1b[?2026l");
                if self.output[start..].contains(text) && complete_frame {
                    return self.output[start..].to_string();
                }
            }
            let tail_start = self.output.len().saturating_sub(1800);
            let tail_start = (tail_start..=self.output.len())
                .find(|&i| self.output.is_char_boundary(i))
                .unwrap_or(self.output.len());
            panic!("Missing {text:?}: {}", &self.output[tail_start..]);
        }

        fn send(&self, text: &str) {
            let mut data = text.as_bytes();
            while !data.is_empty() {
                let written = unsafe {
                    libc::write(
                        self.master,
                        data.as_ptr().cast::<libc::c_void>(),
                        data.len(),
                    )
                };
                assert!(written > 0, "write to pty master failed");
                data = &data[written as usize..];
            }
        }

        fn resize(&self, columns: u16) {
            let ws = libc::winsize {
                ws_row: 26,
                ws_col: columns,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            unsafe {
                libc::ioctl(self.master, libc::TIOCSWINSZ, &ws);
                libc::kill(self.child.id() as libc::pid_t, libc::SIGWINCH);
            }
        }

        fn wait_exit(&mut self) -> i32 {
            let until = Instant::now() + Duration::from_secs(5);
            while Instant::now() < until {
                if let Some(status) = self.child.try_wait().unwrap() {
                    return status.code().unwrap_or(1);
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            panic!("tui_fixture did not exit within timeout");
        }
    }

    impl Drop for PtyHarness {
        fn drop(&mut self) {
            if self.child.try_wait().ok().flatten().is_none() {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
            unsafe { libc::close(self.master) };
        }
    }

    fn strip_csi(input: &str) -> String {
        let mut out = String::with_capacity(input.len());
        let bytes = input.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                i += 2;
                while i < bytes.len() && matches!(bytes[i], b'0'..=b'9' | b';' | b'?') {
                    i += 1;
                }
                while i < bytes.len() && matches!(bytes[i], 0x20..=0x2f) {
                    i += 1;
                }
                if i < bytes.len() && matches!(bytes[i], 0x40..=0x7e) {
                    i += 1;
                }
                continue;
            }
            let ch = input[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
        out
    }

    fn frame(text: &str) -> String {
        let latest = text
            .rsplit_once("\x1b[1;1H\x1b[J")
            .map_or(text, |(_, rest)| rest);
        strip_csi(latest)
    }

    fn screen_row(text: &str, row: usize) -> String {
        let latest = text
            .rsplit_once("\x1b[1;1H\x1b[J")
            .map_or(text, |(_, rest)| rest);
        let bytes = latest.as_bytes();
        let mut moves: Vec<(usize, usize, usize, usize)> = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                let start = i;
                let mut j = i + 2;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                if j > i + 2 && j < bytes.len() && bytes[j] == b';' {
                    let r_end = j;
                    j += 1;
                    let c_start = j;
                    while j < bytes.len() && bytes[j].is_ascii_digit() {
                        j += 1;
                    }
                    if j > c_start && j < bytes.len() && bytes[j] == b'H' {
                        let r: usize = latest[i + 2..r_end].parse().unwrap();
                        let c: usize = latest[c_start..j].parse().unwrap();
                        moves.push((start, j + 1, r, c));
                        i = j + 1;
                        continue;
                    }
                }
            }
            i += 1;
        }
        for idx in (0..moves.len()).rev() {
            let (_, body_start, r, c) = moves[idx];
            if (r, c) == (row, 1) {
                let end = moves.get(idx + 1).map_or(latest.len(), |m| m.0);
                return frame(&latest[body_start..end]);
            }
        }
        String::new()
    }

    fn cells(text: &str) -> usize {
        UnicodeWidthStr::width(text)
    }

    fn fixture_binary() -> PathBuf {
        if let Some(path) = std::env::var_os("ANTICRUMB_TUI_FIXTURE") {
            return PathBuf::from(path);
        }
        let exe = std::env::current_exe().unwrap();
        let profile_dir = exe.parent().unwrap().parent().unwrap();
        let candidate = profile_dir.join("examples/tui_fixture");
        if candidate.is_file() {
            return candidate;
        }
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let status = Command::new("cargo")
            .current_dir(&root)
            .args(["build", "--locked", "--example", "tui_fixture"])
            .status()
            .expect("build tui_fixture");
        assert!(status.success(), "cargo build --example tui_fixture failed");
        root.join("target/debug/examples/tui_fixture")
    }

    #[test]
    fn modified_arrows_move_composer_by_words_and_folded_pastes() {
        let binary = fixture_binary();
        let temp = TempDir::new().unwrap();
        let home = temp.path().join("home");
        let state = home.join("workspace");
        let workspace = temp.path().join("project");
        fs::create_dir_all(&state).unwrap();
        fs::create_dir(&workspace).unwrap();
        let home_str = home.to_string_lossy().into_owned();
        let mut pty = PtyHarness::spawn(
            &binary,
            &state,
            &workspace,
            &[
                ("ANTICRUMB_HOME", Some(home_str.as_str())),
                ("TERM", Some("xterm-256color")),
                ("NO_COLOR", None),
            ],
        );
        pty.wait_for("W:project");

        // Insert a visible marker after actual terminal escape sequences. Each
        // independent draft checks cursor placement without submitting a turn.
        for (draft, keys, expected) in [
            ("alpha beta", "\x1b[1;5D", "alpha |beta"),
            ("alpha beta", "\x1b[1;3D\x1b[1;3D", "|alpha beta"),
            ("alpha beta", "\x01\x1b[1;5C", "alpha| beta"),
            ("alpha beta", "\x01\x1b[1;3C\x1b[1;3C", "alpha beta|"),
            ("alpha beta", "\x1bb", "alpha |beta"),
            ("alpha beta", "\x01\x1bf", "alpha| beta"),
            (
                "\u{4e00}\u{4e8c}\u{4e09} \u{56db}\u{4e8c}\u{4e09}",
                "\x1b[1;3D",
                "\u{4e00}\u{4e8c}\u{4e09} |\u{56db}\u{4e8c}\u{4e09}",
            ),
            (
                "\u{4e00}\u{4e8c}\u{4e09} \u{56db}\u{4e8c}\u{4e09}",
                "\x01\x1b[1;5C",
                "\u{4e00}\u{4e8c}\u{4e09}| \u{56db}\u{4e8c}\u{4e09}",
            ),
            ("src/tui/editor.rs", "\x1b[1;5D", "src/tui/editor.|rs"),
            (
                "src/tui/editor.rs",
                "\x01\x1b[1;3C\x1b[1;3C",
                "src/tui|/editor.rs",
            ),
            ("ab\u{754c}", "\x1b[D", "ab|\u{754c}"),
            ("\u{754c}ab", "\x01\x1b[C", "\u{754c}|ab"),
        ] {
            // Ctrl+E then Ctrl+U clears the whole previous draft regardless of
            // the cursor position left by the preceding assertion.
            pty.send("\x05\x15");
            pty.send(draft);
            pty.send(keys);
            pty.send("|");
            let rendered = pty.wait_for(expected);
            assert!(frame(&rendered).contains(expected), "{keys:?}: {rendered}");
        }

        let paste = format!("\x1b[200~{}\x1b[201~", "x".repeat(1001));
        for (keys, expected) in [
            ("\x1b[1;3D", "|[Pasted Content 1001 chars]"),
            ("\x01\x1b[1;5C", "[Pasted Content 1001 chars]|"),
        ] {
            pty.send("\x05\x15");
            pty.send(&paste);
            pty.send(keys);
            pty.send("|");
            let rendered = pty.wait_for(expected);
            assert!(frame(&rendered).contains(expected), "{keys:?}: {rendered}");
        }

        pty.send("\x05\x15Keep this draft");
        pty.wait_for("Keep this draft");
        for _ in 0..3 {
            // Recover from terminal resets even when no key reaches the app.
            let refresh = pty.wait_for("\x1b[?1006h");
            assert!(refresh.contains("\x1b[?2004h"));
            assert!(
                !refresh.contains("\x1b[2J"),
                "mode refresh must not accumulate terminal rows"
            );
            assert!(
                !refresh.contains("\x1b[3J"),
                "mode refresh must not purge shell scrollback"
            );
        }
        pty.send("\x05\x15\x04");
        assert_eq!(pty.wait_exit(), 0);
        let observations: Value =
            serde_json::from_str(&fs::read_to_string(state.join("observations.json")).unwrap())
                .unwrap();
        assert_eq!(observations["tasks"], serde_json::json!([]));
        assert_eq!(observations["uploads"], serde_json::json!([]));
    }

    #[test]
    fn tui_pty_contract() {
        use std::os::unix::fs::PermissionsExt;

        let binary = fixture_binary();
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        let home = root.join("home");
        let state = home.join("workspace");
        let workspace = root.join("project");
        fs::create_dir_all(&state).unwrap();
        fs::create_dir(&workspace).unwrap();
        fs::write(workspace.join("a b.txt"), "PRIVATE_NATIVE_ATTACHMENT").unwrap();

        let observations = || -> Value {
            serde_json::from_str(&fs::read_to_string(state.join("observations.json")).unwrap())
                .unwrap()
        };

        let clipboard = root.join("clipboard");
        fs::create_dir(&clipboard).unwrap();
        let wl_paste = clipboard.join("wl-paste");
        fs::write(
            &wl_paste,
            "#!/bin/sh\nprintf '%s' 'clipboard fixture text'\n",
        )
        .unwrap();
        fs::set_permissions(&wl_paste, fs::Permissions::from_mode(0o755)).unwrap();

        let copied_file = root.join("copied.txt");
        let copy_helper = "#!/bin/sh\nif [ -f \"$ANTICRUMB_TEST_COPY.fail\" ]; then exit 7; fi\ncat > \"$ANTICRUMB_TEST_COPY\"\n";
        for helper in ["wl-copy", "xclip", "xsel", "pbcopy"] {
            let p = clipboard.join(helper);
            fs::write(&p, copy_helper).unwrap();
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let path_with_clipboard = format!(
            "{}:{}",
            clipboard.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let home_str = home.to_string_lossy().into_owned();
        let copied_str = copied_file.to_string_lossy().into_owned();

        let base_env = [
            ("ANTICRUMB_HOME", Some(home_str.as_str())),
            ("TERM", Some("xterm-256color")),
            ("NO_COLOR", None),
            ("ANTICRUMB_TEST_COPY", Some(copied_str.as_str())),
            ("SSH_TTY", None),
            ("SSH_CONNECTION", None),
            ("SSH_CLIENT", None),
            ("PATH", Some(path_with_clipboard.as_str())),
            ("WAYLAND_DISPLAY", Some("fixture")),
        ];

        let mut pty = PtyHarness::spawn(&binary, &state, &workspace, &base_env);
        pty.wait_for(&format!("anticrumb {}", env!("CARGO_PKG_VERSION")));
        pty.wait_for("W:project");
        pty.send("/copy\r");
        pty.wait_for("No assistant response to copy yet.");
        assert!(!copied_file.exists());

        pty.send("unfinished draft\x1b[B");
        pty.wait_for("unfinished draft");
        pty.send("\x15/model\r");
        pty.wait_for("No cached models.");
        assert_eq!(observations()["model_probes"], 0);

        pty.send("/browser login\r");
        pty.wait_for("Model choices will load automatically");
        assert_eq!(observations()["model_probes"], 0);
        pty.send("unfinished login draft");
        pty.wait_for("unfinished login draft");
        fs::File::create(state.join("login-complete")).unwrap();
        let completed = pty.wait_for("Model choices are ready");
        assert!(completed.contains("unfinished login draft"));
        assert_eq!(observations()["model_probes"], 1);
        assert_eq!(observations()["tasks"].as_array().unwrap().len(), 0);

        pty.send("\x15/model\r");
        pty.wait_for("Model · ↑↓ model");
        pty.send("\x1b[A");
        pty.wait_for("Unavailable");
        pty.send("\x1b[B\x1b[C\x1b[C\x1b[C");
        pty.wait_for("Ultra");
        pty.send("\x1b[B\x1b[C\x1b[C\x1b[D\r");
        let selected = pty.wait_for("M:4.0 Argon");
        assert!(selected.contains("E:Medium"));
        pty.send("/model\r");
        pty.wait_for("Quick answers");
        assert_eq!(observations()["model_probes"], 1);
        pty.send("\x1b");
        std::thread::sleep(Duration::from_millis(150));
        pty.send("/model refresh\r");
        pty.wait_for("Quick answers");
        assert_eq!(observations()["model_probes"], 2);
        pty.send("\x1b");
        std::thread::sleep(Duration::from_millis(150));

        pty.send("/theme monokai\r");
        let themed = pty.wait_for("Theme: Monokai");
        assert!(themed.contains("48;2;39;40;34"));
        pty.send("/theme\r");
        pty.wait_for("Busan Night");
        pty.send("\x1b[B\r");
        pty.wait_for("Theme: Nord");

        pty.send("/browser\r");
        pty.wait_for("Chrome/fixture");
        pty.send("/browser show\r");
        pty.wait_for("visible (window open)");
        pty.send("/browser hide\r");
        pty.wait_for("headless (background)");
        pty.send("/browser open\r");
        let rejected = pty.wait_for("[status|show|hide|refresh|login|clear-history]");
        assert!(frame(&rejected)
            .contains("Usage: /browser [status|show|hide|refresh|login|clear-history]"));
        assert_eq!(
            observations()["browser_mode_requests"],
            serde_json::json!([
                {"account_id": "default", "headless": false},
                {"account_id": "default", "headless": true}
            ])
        );
        pty.send("/usage\r");
        let usage = pty.wait_for("75% remaining");
        assert!(usage.contains("25% used") && usage.contains("Resets at 5:41"));
        assert!(usage.contains("Left:≈75% @5:41am Wk:≈100% @Nov 20 3:41pm"));
        pty.resize(48);
        pty.wait_for("Left:≈75% @5:41am Wk:≈100% @Nov 20 3:41pm");
        pty.resize(110);
        pty.wait_for("W:project");
        assert_eq!(observations()["web_usage_queries"], 1);
        assert_eq!(observations()["model_probes"], 2);
        assert_eq!(observations()["tasks"].as_array().unwrap().len(), 0);

        pty.send("draft before history\x1b[A");
        pty.wait_for("History");
        pty.send("\x1b[B");
        pty.wait_for("draft before history");
        pty.send("\x15");
        pty.send("@a\t");
        pty.wait_for("@\"a b.txt\"");
        pty.send("\x15");
        pty.send("\x1b[200~hello\nworld\x1b[201~");
        pty.wait_for("world");
        pty.send("\x15");
        if cfg!(target_os = "linux") {
            pty.send("\x16");
            pty.wait_for("clipboard fixture text");
            pty.send("\x15");
        }

        pty.send("/permissions\r");
        pty.wait_for("shell disabled");
        pty.send("\x1b[B\r");
        pty.wait_for("file + shell tools");
        pty.send("Read @\"a b.txt\"\r");
        pty.wait_for("Attachment turn 1 completed.");
        let highlighted = pty.wait_for("let answer = 42;");
        assert!(highlighted.contains("\x1b[1mImportant result"));
        assert!(highlighted.contains("\x1b[3memphasis"));
        assert!(highlighted.contains("\x1b[38;2;136;192;208mfile_name.rs"));
        assert!(!highlighted.contains("**Important result**"));
        assert!(!highlighted.contains("```rust"));

        let table_rows: Vec<String> = (1..24).map(|r| screen_row(&highlighted, r)).collect();
        let header = table_rows
            .iter()
            .find(|r| r.starts_with("Item") && r.contains("State"))
            .unwrap();
        let divider = table_rows
            .iter()
            .find(|r| r.starts_with("---") && r.contains("-+-"))
            .unwrap();
        let wide = table_rows
            .iter()
            .find(|r| r.starts_with("\u{754c}\u{8a9e}") && r.contains("ready"))
            .unwrap();
        let numeric = table_rows
            .iter()
            .find(|r| r.starts_with("test") && r.contains("42"))
            .unwrap();
        assert!([header, wide, numeric]
            .iter()
            .all(|r| r.contains(" | ") && !r.starts_with('|') && !r.trim_end().ends_with('|')));
        assert_eq!(cells(divider.trim_end()), cells(header.trim_end()));
        assert_eq!(
            cells(wide.split("ready").next().unwrap()),
            header.find("State").unwrap()
        );
        assert_eq!(cells(numeric.trim_end()), cells(header.trim_end()));

        pty.send("/copy\r");
        let copied = pty.wait_for("characters to clipboard.");
        let saved_resp = observations()["saved_agents"][0]["result"]["final_response"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(fs::read_to_string(&copied_file).unwrap(), saved_resp);
        assert!(saved_resp.contains("**Important result**"));
        assert!(saved_resp.contains("| Item | State |"));

        let wide_row = (2..24)
            .find(|&row| screen_row(&copied, row).starts_with("\u{754c}\u{8a9e}"))
            .unwrap();
        let wide_text = screen_row(&copied, wide_row).trim_end().to_string();
        let selection_width = cells(&wide_text);
        pty.send(&format!("\x1b[<0;1;{wide_row}M"));
        pty.wait_for("Drag to select");
        pty.send(&format!("\x1b[<32;{selection_width};{wide_row}M"));
        let selecting = pty.wait_for("Drag to select");
        assert!(selecting.contains("\x1b[48;2;136;192;208m"));
        pty.send(&format!("\x1b[<0;{selection_width};{wide_row}m"));
        pty.wait_for("characters to clipboard.");
        assert_eq!(fs::read_to_string(&copied_file).unwrap(), wide_text);

        pty.send("\x1b[<0;1;25M\x1b[<32;5;25M\x1b[<0;5;25m");
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(fs::read_to_string(&copied_file).unwrap(), wide_text);

        let copied_fail = root.join("copied.txt.fail");
        fs::File::create(&copied_fail).unwrap();
        pty.send("/copy\r");
        let failure = pty.wait_for("Could not copy:");
        assert!(failure.contains("wl-copy"));
        fs::remove_file(&copied_fail).unwrap();
        pty.send("/copy\r");
        pty.wait_for("characters to clipboard.");
        assert_eq!(fs::read_to_string(&copied_file).unwrap(), saved_resp);
        assert_eq!(
            observations()["uploads"],
            serde_json::json!(["PRIVATE_NATIVE_ATTACHMENT"])
        );
        assert_eq!(
            observations()["tasks"][0]["artifacts"],
            observations()["uploaded_ids"]
        );
        assert!(!observations()["tasks"][0]["task"]
            .as_str()
            .unwrap()
            .contains("PRIVATE_NATIVE_ATTACHMENT"));
        assert_eq!(observations()["tasks"][0]["model"], "argon");
        assert_eq!(observations()["tasks"][0]["thinking"], "Medium");

        pty.send("/resume\r");
        pty.wait_for("Resume conversation");
        pty.send("\r");
        pty.wait_for("Resumed agent_fixture");
        pty.send("Explain the previous result\r");
        pty.wait_for("Attachment turn 2 completed.");
        let tasks = observations()["tasks"].as_array().unwrap().clone();
        assert_eq!(tasks.last().unwrap()["parent_id"], "agent_fixture");
        assert_eq!(tasks.last().unwrap()["thinking"], "Medium");

        pty.send("one\x1b\rthree\x1b[AX\x1b[BY\r");
        pty.wait_for("Attachment turn 3 completed.");
        assert_eq!(
            observations()["tasks"].as_array().unwrap().last().unwrap()["task"],
            "oneX\nthreYe"
        );

        pty.send("alt-bs: alpha src/main.rs");
        pty.wait_for("alt-bs: alpha src/main.rs");
        pty.send("\x1b\x7f\x1b\x7f!");
        let after_alt_bs = pty.wait_for("alt-bs: alpha src/!");
        assert!(frame(&after_alt_bs).contains("alt-bs: alpha src/!"));
        pty.send("\x15");

        let short = format!(
            "{}SHORT_PASTE_END_1000",
            "x".repeat(1000 - "SHORT_PASTE_END_1000".len())
        );
        assert_eq!(short.len(), 1000);
        pty.send(&format!("\x1b[200~{short}\x1b[201~"));
        let inline = pty.wait_for("SHORT_PASTE_END_1000");
        assert!(!frame(&inline).contains("Pasted Content 1000 chars"));
        pty.send("\x15");

        let block = format!(
            "{}\n@not-an-attachment\nLONG_PASTE_END",
            "\u{754c}".repeat(1001)
        );
        let label = format!("[Pasted Content {} chars]", block.chars().count());
        pty.send(&format!("prefix \x1b[200~{block}\x1b[201~"));
        let folded = pty.wait_for(&label);
        assert!(!frame(&folded).contains("LONG_PASTE_END"));
        pty.send("\x1be");
        pty.wait_for("LONG_PASTE_END");
        pty.send(&format!("\x15prefix \x1b[200~{block}\x1b[201~ suffix\r"));
        pty.wait_for("Attachment turn 4 completed.");
        let last_task = observations()["tasks"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        assert_eq!(last_task["task"], format!("prefix {block} suffix"));
        assert_eq!(last_task["artifacts"], serde_json::json!([]));
        assert_eq!(
            observations()["uploads"],
            serde_json::json!(["PRIVATE_NATIVE_ATTACHMENT"])
        );

        pty.send("/help\r");
        pty.wait_for("exact Gemini token usage");
        pty.send("wheel draft");
        let before = frame(&pty.wait_for("wheel draft"));
        pty.send("\x1b[<64;10;5M");
        let earlier_frame = pty.wait_for("wheel draft");
        let earlier = frame(&earlier_frame);
        let before_head: String = before.chars().take(600).collect();
        let earlier_head: String = earlier.chars().take(600).collect();
        assert_ne!(before_head, earlier_head);

        let (selected_row, selected_text) = (2..24)
            .map(|row| (row, screen_row(&earlier_frame, row)))
            .find(|(_, text)| text.is_ascii() && text.len() > 8)
            .unwrap();
        pty.send(&format!(
            "\x1b[<0;1;{selected_row}M\x1b[<32;8;{selected_row}M\x1b[<0;8;{selected_row}m"
        ));
        pty.wait_for("characters to clipboard.");
        assert_eq!(
            fs::read_to_string(&copied_file).unwrap(),
            &selected_text[..8]
        );
        pty.send("\x1b[<65;10;5M");
        let later = frame(&pty.wait_for("wheel draft"));
        let later_head: String = later.chars().take(600).collect();
        assert_eq!(before_head, later_head);
        assert!(later.contains("Left:≈75% @5:41am Wk:≈100% @Nov 20 3:41pm"));

        pty.send("\x15/clear\r");
        pty.wait_for("New conversation");
        pty.send("/status\r");
        pty.wait_for("Conversation: new");
        pty.send("/exit\r");
        pty.wait_for("\x1b[?1006l");
        assert_eq!(pty.wait_exit(), 0);
        assert_eq!(observations()["model_probes"], 2);
        assert_eq!(observations()["tasks"].as_array().unwrap().len(), 4);
        assert!(pty.output.contains("\x1b[?1006h") && pty.output.contains("\x1b[?1006l"));
        drop(pty);

        let mut pty = PtyHarness::spawn(&binary, &state, &workspace, &base_env);
        let reopened = pty.wait_for("W:project");
        assert!(reopened.contains("48;2;46;52;64"));
        pty.send("\x1b[A");
        let history = pty.wait_for("History");
        assert!(history.contains("/status"));
        pty.send("\x1b[A\x1b[A\x1b[A");
        pty.wait_for(&label);
        pty.send(&format!("{}\x1be", "\x1b[D".repeat(" suffix".len())));
        pty.wait_for("LONG_PASTE_END");
        pty.send("\x05\x15/model\r");
        pty.wait_for("Quick answers");
        assert_eq!(observations()["model_probes"], 2);
        pty.send("\x1b");
        std::thread::sleep(Duration::from_millis(150));

        pty.send("/resume\r");
        pty.wait_for("Resume conversation");
        pty.send("\r");
        pty.wait_for("Send a follow-up to continue.");
        pty.send("/status\r");
        pty.wait_for("Conversation: agent_fixture_4");
        pty.send("/resume\r");
        pty.wait_for("Resume conversation");
        pty.send("D");
        pty.wait_for("Delete ALL Gemini chats?");
        pty.send("\x1b");
        pty.wait_for("Resume conversation");
        assert_eq!(
            observations()["history_clear_requests"],
            serde_json::json!([])
        );
        pty.send("\x1b[Bd");
        let confirmation = pty.wait_for("Delete conversation?");
        assert!(confirmation.contains("d confirm · Esc cancel"));
        pty.resize(48);
        let narrow_confirmation = pty.wait_for("d confirm · Esc cancel");
        assert!(screen_row(&narrow_confirmation, 21).contains("d confirm · Esc cancel"));
        pty.resize(110);
        pty.wait_for("Delete conversation?");
        pty.send("\r");
        pty.wait_for("Resume conversation");
        assert_eq!(observations()["deleted_ids"], serde_json::json!([]));
        pty.send("d");
        pty.wait_for("Delete conversation?");
        pty.send("\x1b");
        pty.wait_for("Resume conversation");
        assert_eq!(observations()["deleted_ids"], serde_json::json!([]));

        fs::File::create(state.join("reject-delete")).unwrap();
        pty.send("d");
        pty.wait_for("Delete conversation?");
        pty.send("d");
        let failed = pty.wait_for("Could not delete conversation:");
        assert!(screen_row(&failed, 24).contains("Could not delete conversation:"));
        assert_eq!(observations()["deleted_ids"], serde_json::json!([]));
        assert_eq!(observations()["saved_agents"].as_array().unwrap().len(), 4);
        fs::remove_file(state.join("reject-delete")).unwrap();
        pty.send("d");
        pty.wait_for("Delete conversation?");
        pty.send("d");
        let removed = pty.wait_for("Deleted saved conversation agent_fixture_3.");
        assert!(removed.contains("Resume conversation"));
        assert!(screen_row(&removed, 24).contains("Deleted saved conversation agent_fixture_3."));
        assert_eq!(
            observations()["deleted_ids"],
            serde_json::json!(["agent_fixture_3"])
        );
        let remaining_ids: BTreeSet<String> = observations()["saved_agents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|j| j["id"].as_str().unwrap().to_string())
            .collect();
        let expected_ids: BTreeSet<String> =
            ["agent_fixture", "agent_fixture_2", "agent_fixture_4"]
                .into_iter()
                .map(str::to_string)
                .collect();
        assert_eq!(remaining_ids, expected_ids);

        pty.send("\x1b");
        std::thread::sleep(Duration::from_millis(150));
        pty.send("/status\r");
        pty.wait_for("Conversation: agent_fixture_4");
        pty.send("/resume\r");
        pty.wait_for("Resume conversation");
        pty.send("\x1b[3~");
        pty.wait_for("Delete conversation?");
        pty.send("d");
        pty.wait_for("Deleted saved conversation agent_fixture_4.");
        pty.send("\x1b");
        std::thread::sleep(Duration::from_millis(150));
        pty.send("/status\r");
        pty.wait_for("Conversation: new");
        pty.send("/resume\r");
        pty.wait_for("Resume conversation");
        for job_id in ["agent_fixture_2", "agent_fixture"] {
            pty.send("d");
            pty.wait_for("Delete conversation?");
            pty.send("d");
            pty.wait_for(&format!("Deleted saved conversation {job_id}."));
        }
        assert_eq!(observations()["saved_agents"], serde_json::json!([]));
        assert_eq!(
            observations()["deleted_ids"],
            serde_json::json!([
                "agent_fixture_3",
                "agent_fixture_4",
                "agent_fixture_2",
                "agent_fixture"
            ])
        );
        pty.send("/resume\r");
        pty.wait_for("No saved conversations in this directory.");
        assert_eq!(
            fs::read_to_string(workspace.join("a b.txt")).unwrap(),
            "PRIVATE_NATIVE_ATTACHMENT"
        );
        pty.send("\x15/exit\r");
        assert_eq!(pty.wait_exit(), 0);
        drop(pty);

        let mut pty = PtyHarness::spawn(&binary, &state, &workspace, &base_env);
        pty.wait_for(&format!("anticrumb {}", env!("CARGO_PKG_VERSION")));
        pty.wait_for("W:project");
        pty.send("/resume\r");
        pty.wait_for("No saved conversations in this directory.");
        assert_eq!(
            observations()["history_clear_requests"],
            serde_json::json!([])
        );
        pty.send("/browser clear-history\r");
        let confirmation = pty.wait_for("Delete ALL Gemini chats?");
        assert!(screen_row(&confirmation, 21).contains("Account: default"));
        assert!(screen_row(&confirmation, 22).contains("Includes chats created outside anticrumb."));
        pty.send("\r");
        pty.wait_for("Alt+Enter newline");
        assert_eq!(
            observations()["history_clear_requests"],
            serde_json::json!([])
        );
        pty.send("/browser clear-history\r");
        pty.wait_for("Delete ALL Gemini chats?");
        pty.send("delete\r");
        pty.wait_for("Type DELETE, then Enter: delete");
        assert_eq!(
            observations()["history_clear_requests"],
            serde_json::json!([])
        );
        pty.send("\x1b");
        pty.wait_for("Alt+Enter newline");
        pty.send("/browser clear-history\r");
        pty.wait_for("Delete ALL Gemini chats?");
        pty.send("\x1b[200~DELETE\n\x1b[201~");
        pty.wait_for("Type DELETE, then Enter: DELETE");
        assert_eq!(
            observations()["history_clear_requests"],
            serde_json::json!([])
        );
        fs::File::create(state.join("reject-clear-history")).unwrap();
        pty.send("\r");
        pty.wait_for("Could not queue Gemini history cleanup:");
        assert_eq!(
            observations()["history_clear_requests"],
            serde_json::json!([])
        );
        fs::remove_file(state.join("reject-clear-history")).unwrap();
        pty.send("/resume clear-history\r");
        pty.wait_for("Delete ALL Gemini chats?");
        pty.send("DELETE\r");
        pty.wait_for("Gemini history cleanup (default): pending");
        let running = pty.wait_for("running · 1 chats deleted");
        assert!(screen_row(&running, 26).contains("History:1 deleted"));
        pty.wait_for("completed · 2 chats deleted");
        assert_eq!(
            observations()["history_clear_requests"],
            serde_json::json!(["default"])
        );
        assert_eq!(observations()["saved_agents"], serde_json::json!([]));
        pty.send("/exit\r");
        assert_eq!(pty.wait_exit(), 0);
        assert_eq!(observations()["tasks"].as_array().unwrap().len(), 4);
        drop(pty);

        fs::File::create(&copied_fail).unwrap();
        let ssh_env = [
            ("ANTICRUMB_HOME", Some(home_str.as_str())),
            ("TERM", Some("xterm-256color")),
            ("NO_COLOR", None),
            ("ANTICRUMB_TEST_COPY", Some(copied_str.as_str())),
            ("SSH_TTY", None),
            ("SSH_CONNECTION", Some("fixture")),
            ("SSH_CLIENT", None),
            ("PATH", Some(path_with_clipboard.as_str())),
            ("WAYLAND_DISPLAY", None),
            ("DISPLAY", None),
        ];
        let mut pty = PtyHarness::spawn(&binary, &state, &workspace, &ssh_env);
        let reopened = pty.wait_for("W:project");
        assert!(!frame(&reopened).contains("Gemini history cleanup"));
        pty.send("/status\r");
        pty.wait_for("completed · 2 chats deleted");
        assert_eq!(
            observations()["history_clear_requests"],
            serde_json::json!(["default"])
        );
        pty.send("Check clipboard fallback\r");
        pty.wait_for("Attachment turn 5 completed.");
        pty.send("/copy\r");
        let requested = pty.wait_for("Copy request sent to terminal");
        let marker = "\x1b]52;c;";
        let start = requested.find(marker).expect("OSC52 prefix") + marker.len();
        let end = requested[start..].find('\x07').expect("OSC52 BEL") + start;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&requested[start..end])
            .unwrap();
        let expected_resp = observations()["saved_agents"][0]["result"]["final_response"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(String::from_utf8(decoded).unwrap(), expected_resp);
        assert!(requested.contains("clipboard access depends on terminal settings"));
        pty.send("/exit\r");
        assert_eq!(pty.wait_exit(), 0);
    }
}
