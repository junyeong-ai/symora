#![cfg(unix)]

use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn copy_binary(destination: &Path) {
    // Copy in a child so this process never holds the executable open for
    // writing. A concurrent fork can inherit that descriptor until exec,
    // making Linux reject execution with ETXTBSY.
    let out = Command::new("cp")
        .arg(env!("CARGO_BIN_EXE_symora"))
        .arg(destination)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn command(exe: &Path, home: &Path, action: &str) -> Command {
    let mut command = Command::new(exe);
    command
        .args(["daemon", action])
        .current_dir(home)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home)
        .env_remove("SYMORA_NO_DAEMON");
    command
}

fn json_ok(exe: &Path, home: &Path, action: &str) -> serde_json::Value {
    let out = command(exe, home, action).output().unwrap();
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(value.get("error").is_none(), "{value}");
    value
}

struct Daemons {
    home: PathBuf,
    binaries: Vec<PathBuf>,
}

impl Drop for Daemons {
    fn drop(&mut self) {
        for exe in &self.binaries {
            let _ = command(exe, &self.home, "stop").output();
        }
    }
}

#[test]
fn zero_idle_timeout_allows_start_and_project_request() {
    let home = tempfile::tempdir().unwrap();
    let exe = Path::new(env!("CARGO_BIN_EXE_symora"));
    let _cleanup = Daemons {
        home: home.path().to_path_buf(),
        binaries: vec![exe.to_path_buf()],
    };
    let config_dir = home.path().join("symora");
    std::fs::create_dir(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        "[daemon]\nidle_timeout_mins = 0\n",
    )
    .unwrap();
    std::fs::write(home.path().join("main.rs"), "fn needle() {}\n").unwrap();
    assert_eq!(json_ok(exe, home.path(), "start")["started"], true);
    let out = search_command(exe, home.path()).output().unwrap();
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(value.get("error").is_none(), "{value}");
    assert_eq!(value["count"], 1, "{value}");
    assert_eq!(value["items"][0]["file"], "main.rs");
    let status = json_ok(exe, home.path(), "status");
    assert_eq!(status["active_projects"], 1, "{status}");
    assert!(status["projects"][0]["requests"].as_u64().unwrap() > 0);
    assert_eq!(json_ok(exe, home.path(), "stop")["stopped"], true);
}

#[test]
fn installations_run_independent_daemons_and_leave_legacy_files_untouched() {
    let home = tempfile::tempdir().unwrap();
    let base = home.path().join(".symora");
    std::fs::create_dir(&base).unwrap();
    let legacy = [
        "daemon.sock",
        "daemon.pid",
        "daemon.lock",
        "daemon.bind.lock",
    ];
    for name in legacy {
        std::fs::write(base.join(name), "legacy owner").unwrap();
    }
    let first = home.path().join("first");
    let second = home.path().join("second");
    for exe in [&first, &second] {
        copy_binary(exe);
    }
    let _cleanup = Daemons {
        home: home.path().to_path_buf(),
        binaries: vec![first.clone(), second.clone()],
    };
    assert_eq!(json_ok(&first, home.path(), "start")["started"], true);
    let before = json_ok(&first, home.path(), "status");
    assert_eq!(before["running"], true);
    let first_pid = before["pid"].as_u64().unwrap();
    assert_eq!(json_ok(&second, home.path(), "start")["started"], true);
    let after = json_ok(&first, home.path(), "status");
    let other = json_ok(&second, home.path(), "status");
    assert_eq!(after["running"], true);
    assert_eq!(other["running"], true);
    assert_eq!(after["pid"].as_u64().unwrap(), first_pid);
    assert_ne!(after["pid"], other["pid"]);
    assert_ne!(after["socket_path"], other["socket_path"]);
    for status in [&after, &other] {
        let pid = status["pid"].as_u64().unwrap();
        assert_eq!(unsafe { libc::kill(pid.try_into().unwrap(), 0) }, 0);
        let socket = Path::new(status["socket_path"].as_str().unwrap());
        assert_eq!(socket.parent().unwrap(), base);
        assert!(socket.as_os_str().as_bytes().len() < 104);
        let stem = socket.file_stem().unwrap().to_str().unwrap();
        assert!(stem.starts_with("daemon-"));
        assert_eq!(stem.len(), 23);
        for suffix in ["pid", "lock", "bind.lock"] {
            assert!(base.join(format!("{stem}.{suffix}")).exists());
        }
        assert_eq!(
            std::fs::read_to_string(base.join(format!("{stem}.pid")))
                .unwrap()
                .trim(),
            pid.to_string()
        );
    }
    let link = home.path().join("link");
    std::os::unix::fs::symlink(&first, &link).unwrap();
    assert_eq!(json_ok(&link, home.path(), "status")["pid"], after["pid"]);
    assert_eq!(json_ok(&first, home.path(), "start")["started"], false);
    assert_eq!(json_ok(&first, home.path(), "stop")["stopped"], true);
    assert_eq!(json_ok(&second, home.path(), "status")["pid"], other["pid"]);
    assert_eq!(json_ok(&second, home.path(), "stop")["stopped"], true);
    for name in legacy {
        assert_eq!(
            std::fs::read_to_string(base.join(name)).unwrap(),
            "legacy owner"
        );
    }
}

#[test]
fn installation_replaces_a_stale_build_at_its_own_key() {
    use std::io::Write;
    use std::os::unix::net::UnixListener;

    let home = tempfile::tempdir().unwrap();
    let exe = home.path().join("symora");
    copy_binary(&exe);
    let _cleanup = Daemons {
        home: home.path().to_path_buf(),
        binaries: vec![exe.clone()],
    };
    let canonical = exe.canonicalize().unwrap();
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in canonical.as_os_str().as_bytes() {
        hash = (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    let base = home.path().join(".symora");
    std::fs::create_dir(&base).unwrap();
    let socket = base.join(format!("daemon-{hash:016x}.sock"));
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let stale = std::thread::spawn(move || {
        loop {
            let (mut stream, request) = accept_request(&listener);
            let shutdown = request.method == symora::daemon::protocol::methods::SHUTDOWN;
            let result = if shutdown {
                serde_json::json!({"shutdown": true})
            } else {
                assert_eq!(request.method, symora::daemon::protocol::methods::PING);
                serde_json::json!({"version": env!("CARGO_PKG_VERSION"), "build": "stale-build"})
            };
            let response = symora::daemon::protocol::Response::success(request.id, result);
            writeln!(stream, "{}", serde_json::to_string(&response).unwrap()).unwrap();
            if shutdown {
                return;
            }
        }
    });
    let started = json_ok(&exe, home.path(), "start");
    assert_eq!(started["started"], true);
    assert_eq!(
        started["message"],
        "Daemon from a different binary was replaced"
    );
    stale.join().unwrap();
    let status = json_ok(&exe, home.path(), "status");
    assert_eq!(status["running"], true);
    assert_eq!(Path::new(status["socket_path"].as_str().unwrap()), socket);
    assert_eq!(json_ok(&exe, home.path(), "stop")["stopped"], true);
}

#[test]
fn uninstall_stops_its_installation_and_removes_daemon_root() {
    let home = tempfile::tempdir().unwrap();
    let first = home.path().join("first");
    let second = home.path().join("second");
    for exe in [&first, &second] {
        copy_binary(exe);
    }
    let _cleanup = Daemons {
        home: home.path().to_path_buf(),
        binaries: vec![first.clone(), second.clone()],
    };
    json_ok(&first, home.path(), "start");
    json_ok(&second, home.path(), "start");
    let first_status = json_ok(&first, home.path(), "status");
    let second_status = json_ok(&second, home.path(), "status");
    let uninstall = |exe: &Path, keep_data: bool| {
        let mut cmd = Command::new(exe);
        cmd.args([
            "self",
            "uninstall",
            "--yes",
            "--keep-skill",
            "--keep-config",
        ])
        .current_dir(home.path())
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path())
        .env_remove("SYMORA_NO_DAEMON");
        if keep_data {
            cmd.arg("--keep-daemon-data");
        }
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    };
    uninstall(&first, true);
    assert!(!first.exists());
    assert!(home.path().join(".symora").exists());
    assert!(
        std::os::unix::net::UnixStream::connect(first_status["socket_path"].as_str().unwrap())
            .is_err()
    );
    assert_eq!(
        json_ok(&second, home.path(), "status")["pid"],
        second_status["pid"]
    );
    let second_socket = second_status["socket_path"].as_str().unwrap();
    uninstall(&second, false);
    assert!(!second.exists());
    assert!(!home.path().join(".symora").exists());
    assert!(std::os::unix::net::UnixStream::connect(second_socket).is_err());
    let pid: libc::pid_t = second_status["pid"].as_u64().unwrap().try_into().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if unsafe { libc::kill(pid, 0) } == -1 {
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "uninstalled daemon is still alive"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn stopped_daemon_socket(exe: &Path, home: &Path) -> (PathBuf, serde_json::Value) {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    json_ok(exe, home, "start");
    let status = json_ok(exe, home, "status");
    let socket = PathBuf::from(status["socket_path"].as_str().unwrap());
    let mut stream = UnixStream::connect(&socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let request =
        symora::daemon::protocol::Request::new(1, symora::daemon::protocol::methods::PING, None);
    writeln!(stream, "{}", serde_json::to_string(&request).unwrap()).unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    let response: symora::daemon::protocol::Response = serde_json::from_str(&line).unwrap();
    let identity = response.result.unwrap();
    assert!(identity["build"].as_str().is_some());
    assert_eq!(json_ok(exe, home, "stop")["stopped"], true);
    std::fs::remove_file(&socket).unwrap();
    (socket, identity)
}

fn accept_request(
    listener: &std::os::unix::net::UnixListener,
) -> (
    std::os::unix::net::UnixStream,
    symora::daemon::protocol::Request,
) {
    use std::time::{Duration, Instant};

    let deadline = Instant::now() + Duration::from_secs(15);
    let (stream, _) = loop {
        match listener.accept() {
            Ok(connection) => break connection,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "client did not connect");
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => panic!("{e}"),
        }
    };
    let request = read_request(&stream);
    (stream, request)
}

fn read_request(stream: &std::os::unix::net::UnixStream) -> symora::daemon::protocol::Request {
    use std::io::{BufRead, BufReader};
    use std::time::Duration;

    // The listener is polled nonblocking, and macOS accepted sockets inherit
    // that mode: a read before the client's write fails at once despite its
    // timeout.
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

fn search_command(exe: &Path, home: &Path) -> Command {
    let mut command = Command::new(exe);
    command
        .args(["--format", "compact", "search", "content", "needle"])
        .current_dir(home)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home)
        .env_remove("SYMORA_NO_DAEMON");
    command
}

#[test]
fn project_request_starts_daemon_when_listener_disappears_after_ping() {
    use std::io::Write;
    use std::os::unix::net::UnixListener;
    use symora::daemon::protocol::{Response, methods};

    let home = tempfile::tempdir().unwrap();
    let exe = Path::new(env!("CARGO_BIN_EXE_symora"));
    let _cleanup = Daemons {
        home: home.path().to_path_buf(),
        binaries: vec![exe.to_path_buf()],
    };
    std::fs::write(home.path().join("main.rs"), "fn needle() {}\n").unwrap();
    let (socket, identity) = stopped_daemon_socket(exe, home.path());
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let stand_in = std::thread::spawn(move || {
        let (mut stream, request) = accept_request(&listener);
        assert_eq!(request.method, methods::PING);
        // Removing the listener before answering makes the request's own
        // connect observe absence regardless of client scheduling.
        drop(listener);
        std::fs::remove_file(socket).unwrap();
        let response = Response::success(request.id, identity);
        writeln!(stream, "{}", serde_json::to_string(&response).unwrap()).unwrap();
    });
    let out = search_command(exe, home.path()).output().unwrap();
    stand_in.join().unwrap();
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(value.get("error").is_none(), "{value}");
    assert_eq!(value["count"], 1, "{value}");
    assert_eq!(value["items"][0]["file"], "main.rs");
    assert_eq!(json_ok(exe, home.path(), "status")["running"], true);
}

enum RequestClose {
    AfterRead,
    BeforeRead,
}

#[test]
fn accepted_project_request_without_answer_is_not_retried() {
    assert_accepted_project_request_is_not_retried(RequestClose::AfterRead);
}

#[test]
fn accepted_project_connection_closed_without_read_is_not_retried() {
    assert_accepted_project_request_is_not_retried(RequestClose::BeforeRead);
}

fn assert_accepted_project_request_is_not_retried(close: RequestClose) {
    use std::io::Write;
    use std::os::unix::net::UnixListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};
    use symora::daemon::protocol::{Response, methods};

    let home = tempfile::tempdir().unwrap();
    let exe = Path::new(env!("CARGO_BIN_EXE_symora"));
    let _cleanup = Daemons {
        home: home.path().to_path_buf(),
        binaries: vec![exe.to_path_buf()],
    };
    let (socket, identity) = stopped_daemon_socket(exe, home.path());
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let finished = Arc::new(AtomicBool::new(false));
    let client_finished = Arc::clone(&finished);
    let stand_in = std::thread::spawn(move || {
        let (mut stream, request) = accept_request(&listener);
        assert_eq!(request.method, methods::PING);
        let response = Response::success(request.id, identity);
        writeln!(stream, "{}", serde_json::to_string(&response).unwrap()).unwrap();
        drop(stream);
        let mut connections = 0;
        let deadline = Instant::now() + Duration::from_secs(15);
        while !client_finished.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "client did not finish");
            let stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(e) => panic!("{e}"),
            };
            connections += 1;
            if matches!(close, RequestClose::AfterRead) {
                let request = read_request(&stream);
                assert_eq!(request.method, methods::SEARCH_CONTENT);
            }
            drop(stream);
        }
        connections
    });
    let out = search_command(exe, home.path()).output().unwrap();
    finished.store(true, Ordering::Release);
    assert_eq!(stand_in.join().unwrap(), 1);
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["error"]["code"], "lsp_unavailable", "{value}");
    assert_eq!(
        value["error"]["message"], "Connection closed before 'search_content' was answered",
        "{value}"
    );
}

#[test]
fn status_without_daemon_does_not_start_one() {
    let home = tempfile::tempdir().unwrap();
    let exe = Path::new(env!("CARGO_BIN_EXE_symora"));
    let _cleanup = Daemons {
        home: home.path().to_path_buf(),
        binaries: vec![exe.to_path_buf()],
    };
    assert_eq!(json_ok(exe, home.path(), "status")["running"], false);
    let base = home.path().join(".symora");
    let entries = match std::fs::read_dir(base) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => panic!("{e}"),
    };
    for entry in entries {
        let path = entry.unwrap().path();
        assert_ne!(path.extension().and_then(|s| s.to_str()), Some("pid"));
        assert_ne!(path.extension().and_then(|s| s.to_str()), Some("sock"));
    }
}

#[test]
fn edit_notifications_do_not_start_daemon_when_listener_disappears_after_ping() {
    use std::io::Write;
    use std::os::unix::net::UnixListener;
    use symora::daemon::protocol::{Response, methods};

    for notification in [methods::REFRESH_FILES, methods::NOTE_FILES_EDITED] {
        let home = tempfile::tempdir().unwrap();
        let exe = Path::new(env!("CARGO_BIN_EXE_symora"));
        let _cleanup = Daemons {
            home: home.path().to_path_buf(),
            binaries: vec![exe.to_path_buf()],
        };
        let file = home.path().join("main.rs");
        std::fs::write(&file, "fn alpha() {}\n").unwrap();
        let (socket, identity) = stopped_daemon_socket(exe, home.path());
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stand_in = std::thread::spawn(move || {
            if notification == methods::NOTE_FILES_EDITED {
                let (mut stream, request) = accept_request(&listener);
                assert_eq!(request.method, methods::PING);
                let response = Response::success(request.id, identity.clone());
                writeln!(stream, "{}", serde_json::to_string(&response).unwrap()).unwrap();
                let (mut stream, request) = accept_request(&listener);
                assert_eq!(request.method, methods::REFRESH_FILES);
                let response = Response::success(request.id, serde_json::json!({}));
                writeln!(stream, "{}", serde_json::to_string(&response).unwrap()).unwrap();
            }
            let (mut stream, request) = accept_request(&listener);
            assert_eq!(request.method, methods::PING);
            // The gated notification must encounter absence on its own
            // connect, after its liveness check succeeded.
            drop(listener);
            std::fs::remove_file(socket).unwrap();
            let response = Response::success(request.id, identity);
            writeln!(stream, "{}", serde_json::to_string(&response).unwrap()).unwrap();
        });
        let out = Command::new(exe)
            .args([
                "--format",
                "compact",
                "edit",
                "replace",
                "main.rs:1:1",
                "--text",
                "fn beta() {}",
            ])
            .current_dir(home.path())
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", home.path())
            .env_remove("SYMORA_NO_DAEMON")
            .output()
            .unwrap();
        stand_in.join().unwrap();
        assert!(
            out.status.success(),
            "{notification}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert!(value.get("error").is_none(), "{notification}: {value}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "fn beta() {}\n");
        assert_eq!(
            json_ok(exe, home.path(), "status")["running"],
            false,
            "{notification} started a daemon"
        );
    }
}
