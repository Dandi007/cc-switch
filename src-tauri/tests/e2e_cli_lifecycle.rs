//! L2 — CLI lifecycle black-box tests
//!
//! Each case runs `cc-switch-cli` via assert_cmd with isolated `--config-dir`
//! and verifies startup, shutdown, takeover, and signal handling semantics.
//!
//! Cases marked ⭐ should FAIL on the current `cli-headless` branch and turn
//! green after the corresponding bug fix.

#[cfg(test)]
mod l2_cli_lifecycle {
    use assert_cmd::Command;
    use std::net::TcpStream;
    use std::process::{Child, Command as StdCommand, Stdio};
    use std::time::Duration;
    use tempfile::TempDir;

    fn cli_binary() -> &'static str {
        env!("CARGO_BIN_EXE_cc-switch-cli")
    }

    fn wait_for_port(addr: &str, port: u16, timeout: Duration) -> bool {
        let target = format!("{addr}:{port}");
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if target
                .parse()
                .ok()
                .and_then(|a| {
                    TcpStream::connect_timeout(&a, Duration::from_millis(200)).ok()
                })
                .is_some()
            {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }

    fn spawn_proxy_foreground(config_dir: &TempDir, addr: &str, port: u16) -> Child {
        seed_proxy_config(config_dir, addr, port);

        let mut child = StdCommand::new(cli_binary())
            .arg("--config-dir")
            .arg(config_dir.path())
            .arg("--app")
            .arg("codex")
            .arg("proxy")
            .arg("start")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn proxy start");

        assert!(
            wait_for_port(addr, port, Duration::from_secs(15)),
            "proxy did not start listening on {addr}:{port} within 15s"
        );
        child
    }

    fn seed_proxy_config(config_dir: &TempDir, addr: &str, port: u16) {
        let _ = StdCommand::new(cli_binary())
            .arg("--config-dir")
            .arg(config_dir.path())
            .arg("--app")
            .arg("codex")
            .arg("provider")
            .arg("list")
            .output()
            .expect("init db");

        let db_path = config_dir.path().join("cc-switch.db");
        let conn = rusqlite::Connection::open(&db_path).expect("open db");
        conn.execute(
            "UPDATE proxy_config SET listen_address = ?1, listen_port = ?2",
            rusqlite::params![addr, port as i32],
        )
        .expect("update proxy port");
    }

    fn sibling_cmd(config_dir: &TempDir, args: &[&str]) -> assert_cmd::assert::Assert {
        let mut cmd = Command::new(cli_binary());
        cmd.arg("--config-dir").arg(config_dir.path());
        for a in args {
            cmd.arg(a);
        }
        cmd.assert()
    }

    fn pid_file_exists(config_dir: &TempDir) -> bool {
        config_dir.path().join("proxy.pid").exists()
    }

    // -----------------------------------------------------------------------
    // L2.basic_start_stop
    // -----------------------------------------------------------------------

    #[test]
    fn l2_basic_start_stop() {
        let dir = TempDir::new().expect("tempdir");
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            l.local_addr().unwrap().port()
        };

        let mut proxy = spawn_proxy_foreground(&dir, "127.0.0.1", port);

        let status = sibling_cmd(&dir, &["--app", "codex", "proxy", "status"]);
        let stdout = String::from_utf8_lossy(&status.get_output().stdout);
        assert!(
            stdout.contains("pid_file") || stdout.contains("\"running\":true"),
            "status should report running: {stdout}"
        );

        sibling_cmd(&dir, &["--app", "codex", "proxy", "stop"]).success();
        std::thread::sleep(Duration::from_millis(500));
        assert!(!pid_file_exists(&dir), "PID file must be removed after stop");

        let _ = proxy.kill();
    }

    // -----------------------------------------------------------------------
    // ⭐ L2.stop_restore_skips_sibling — Bug #2
    // -----------------------------------------------------------------------

    #[test]
    fn l2_stop_restore_skips_sibling() {
        let dir = TempDir::new().expect("tempdir");
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            l.local_addr().unwrap().port()
        };

        let mut proxy = spawn_proxy_foreground(&dir, "127.0.0.1", port);

        sibling_cmd(
            &dir,
            &[
                "--app", "claude", "proxy", "takeover", "set", "--enabled", "true",
            ],
        )
        .success();

        let _stop_restore =
            sibling_cmd(&dir, &["--app", "codex", "proxy", "stop-restore"]);

        assert!(
            wait_for_port("127.0.0.1", port, Duration::from_secs(2)),
            "foreground proxy must still be running after sibling stop-restore"
        );

        assert!(
            pid_file_exists(&dir),
            "PID file must still exist after sibling stop-restore"
        );

        let _ = proxy.kill();
        std::thread::sleep(Duration::from_millis(300));
    }

    // -----------------------------------------------------------------------
    // ⭐ L2.sibling_readonly_doesnt_recover — Bug #3
    // -----------------------------------------------------------------------

    #[test]
    fn l2_sibling_readonly_doesnt_recover() {
        let dir = TempDir::new().expect("tempdir");
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            l.local_addr().unwrap().port()
        };

        let mut proxy = spawn_proxy_foreground(&dir, "127.0.0.1", port);

        sibling_cmd(
            &dir,
            &[
                "--app", "claude", "proxy", "takeover", "set", "--enabled", "true",
            ],
        )
        .success();

        let status = sibling_cmd(&dir, &["--app", "codex", "provider", "list"]);
        status.success();

        assert!(
            wait_for_port("127.0.0.1", port, Duration::from_secs(2)),
            "foreground proxy must still be running after sibling read-only command"
        );

        let _ = proxy.kill();
        std::thread::sleep(Duration::from_millis(300));
    }

    // -----------------------------------------------------------------------
    // ⭐ L2.takeover_set_requires_running — Bug #4
    // -----------------------------------------------------------------------

    #[test]
    fn l2_takeover_set_requires_running() {
        let dir = TempDir::new().expect("tempdir");

        let result = sibling_cmd(
            &dir,
            &[
                "--app", "claude", "proxy", "takeover", "set", "--enabled", "true",
            ],
        );

        let stderr = String::from_utf8_lossy(&result.get_output().stderr);
        assert!(
            !result.get_output().status.success() || stderr.contains("not running"),
            "takeover set without running proxy must fail, stderr={stderr}"
        );
    }

    // -----------------------------------------------------------------------
    // ⭐ L2.sigterm_clean_shutdown — Bug #6 (unix only)
    // -----------------------------------------------------------------------

    #[cfg(unix)]
    #[test]
    fn l2_sigterm_clean_shutdown() {
        let dir = TempDir::new().expect("tempdir");
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            l.local_addr().unwrap().port()
        };

        let mut proxy = spawn_proxy_foreground(&dir, "127.0.0.1", port);

        sibling_cmd(
            &dir,
            &[
                "--app", "claude", "proxy", "takeover", "set", "--enabled", "true",
            ],
        )
        .success();

        let pid = proxy.id();
        let status = StdCommand::new("kill")
            .arg(pid.to_string())
            .status()
            .expect("send SIGTERM");
        assert!(status.success(), "kill command failed");

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut exited = false;
        while std::time::Instant::now() < deadline {
            match proxy.try_wait() {
                Ok(Some(_)) => {
                    exited = true;
                    break;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(200)),
                Err(_) => break,
            }
        }

        assert!(exited, "proxy must exit after SIGTERM within 10s");

        std::thread::sleep(Duration::from_millis(500));
        assert!(
            !pid_file_exists(&dir),
            "PID file must be removed after SIGTERM clean shutdown"
        );

        let _ = proxy.kill();
    }
    // -----------------------------------------------------------------------
    // ⭐ L2.takeover_set_rejected_from_sibling — A2 fix
    // -----------------------------------------------------------------------

    #[test]
    fn l2_takeover_set_rejected_from_sibling() {
        let dir = TempDir::new().expect("tempdir");
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            l.local_addr().unwrap().port()
        };

        let mut proxy = spawn_proxy_foreground(&dir, "127.0.0.1", port);

        // Sibling CLI must not be allowed to modify takeover state even
        // though the proxy is running (the sibling doesn't own the process).
        let result = sibling_cmd(
            &dir,
            &[
                "--app", "claude", "proxy", "takeover", "set", "--enabled", "true",
            ],
        );

        let stderr = String::from_utf8_lossy(&result.get_output().stderr);
        assert!(
            !result.get_output().status.success() || stderr.contains("another process"),
            "sibling takeover set must be rejected, stderr={stderr}"
        );

        assert!(
            wait_for_port("127.0.0.1", port, Duration::from_secs(2)),
            "foreground proxy must still be running after rejected sibling takeover set"
        );

        let _ = proxy.kill();
        std::thread::sleep(Duration::from_millis(300));
    }

    // -----------------------------------------------------------------------
    // ⭐ L2.duplicate_start_rejected — A3 fix
    // -----------------------------------------------------------------------

    #[test]
    fn l2_duplicate_start_rejected() {
        let dir = TempDir::new().expect("tempdir");
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            l.local_addr().unwrap().port()
        };

        let mut proxy = spawn_proxy_foreground(&dir, "127.0.0.1", port);

        // A second proxy start must be refused before init runs
        // recover_from_crash, which would tear down the active proxy.
        let result = sibling_cmd(
            &dir,
            &["--app", "codex", "proxy", "start"],
        );

        let stderr = String::from_utf8_lossy(&result.get_output().stderr);
        assert!(
            !result.get_output().status.success() || stderr.contains("already running"),
            "duplicate proxy start must be rejected, stderr={stderr}"
        );

        assert!(
            wait_for_port("127.0.0.1", port, Duration::from_secs(2)),
            "foreground proxy must still be running after rejected duplicate start"
        );

        let _ = proxy.kill();
        std::thread::sleep(Duration::from_millis(300));
    }
}
