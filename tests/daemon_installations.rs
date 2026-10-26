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
    project_command(exe, home, &["daemon", action])
}

fn project_command(exe: &Path, home: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(exe);
    command
        .args(args)
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

#[test]
fn project_restart_reloads_settings_without_stopping_other_sessions() {
    let python = Path::new("/usr/bin/python3");
    if !Command::new(python)
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success())
    {
        eprintln!("configuration protocol fixture requires /usr/bin/python3");
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let exe = home.path().join("symora");
    copy_binary(&exe);
    let _cleanup = Daemons {
        home: home.path().to_path_buf(),
        binaries: vec![exe.clone()],
    };
    let first = home.path().join("first");
    let second = home.path().join("second");
    let server =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/lsp/configuration_server.py");
    let configure = |root: &Path, marker: &str| {
        std::fs::create_dir_all(root.join(".symora")).unwrap();
        std::fs::write(root.join("main.py"), "value = 1\n").unwrap();
        let config: symora::models::config::SymoraConfig =
            serde_json::from_value(serde_json::json!({
                "lsp": {"servers": {"python": {
                    "command": python, "args": [server],
                    "settings": {"python": {"analysis": {"marker": marker}}}
                }}}
            }))
            .unwrap();
        std::fs::write(
            root.join(".symora/config.toml"),
            toml::to_string(&config).unwrap(),
        )
        .unwrap();
    };
    configure(&first, "first");
    configure(&second, "second");
    let run = |root: &Path, args: &[&str]| {
        let out = project_command(&exe, home.path(), args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert!(value.get("error").is_none(), "{value}");
        value
    };
    assert_eq!(
        run(&first, &["daemon", "restart", "--project"])["restarted"],
        false
    );
    assert_eq!(json_ok(&exe, home.path(), "status")["running"], false);
    assert_eq!(run(&first, &["hover", "main.py:1:1"])["content"], "first");
    assert_eq!(run(&second, &["hover", "main.py:1:1"])["content"], "second");
    let before = json_ok(&exe, home.path(), "status");
    assert_eq!(before["active_projects"], 2);

    configure(&first, "changed");
    assert_eq!(run(&first, &["hover", "main.py:1:1"])["content"], "first");
    assert_eq!(
        run(&first, &["daemon", "restart", "--project"])["restarted"],
        true
    );
    let after = json_ok(&exe, home.path(), "status");
    assert_eq!(after["pid"], before["pid"]);
    assert_eq!(after["active_projects"], 1);
    let unchanged = before["projects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| Path::new(row["project"].as_str().unwrap()) == second.canonicalize().unwrap())
        .unwrap();
    assert_eq!(&after["projects"][0], unchanged);
    assert_eq!(run(&first, &["hover", "main.py:1:1"])["content"], "changed");
    assert_eq!(run(&second, &["hover", "main.py:1:1"])["content"], "second");
    assert_eq!(json_ok(&exe, home.path(), "status")["pid"], before["pid"]);
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
        // What a terminal signals the job that started it stays off the daemon.
        let pid: libc::pid_t = pid.try_into().unwrap();
        assert_eq!(unsafe { libc::getpgid(pid) }, pid);
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
    let output = project_command(&exe, home.path(), &["daemon", "restart", "--project"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let rejected: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        rejected["error"]["message"]
            .as_str()
            .unwrap()
            .contains("different build"),
        "{rejected}"
    );
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
    project_command(
        exe,
        home,
        &["--format", "compact", "search", "content", "needle"],
    )
}

#[test]
fn concurrent_readers_cannot_replace_a_mutation_request_snapshot() {
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let python = Path::new("/usr/bin/python3");
    if !Command::new(python)
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        eprintln!("concurrent edit protocol fixture requires /usr/bin/python3");
        return;
    }

    for mode in ["concurrent-rename", "concurrent-resolve"] {
        let home = tempfile::tempdir().unwrap();
        let exe = home.path().join("symora");
        copy_binary(&exe);
        let _cleanup = Daemons {
            home: home.path().to_path_buf(),
            binaries: vec![exe.clone()],
        };
        let root = home.path().join("project");
        std::fs::create_dir(&root).unwrap();
        let alias = home.path().join("project-alias");
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        let source = root.join("main.go");
        std::fs::write(&source, "package main\nfunc abc() {}\n").unwrap();
        std::fs::write(
            root.join("go.mod"),
            "module example.com/fixture\n\ngo 1.25\n",
        )
        .unwrap();
        std::fs::create_dir(root.join(".symora")).unwrap();
        let server =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/lsp/code_action_server.py");
        let config = serde_json::json!({"lsp": {"servers": {"go": {
            "command": python, "args": [server, alias.join("main.go"), mode]
        }}}});
        let config: symora::models::config::SymoraConfig = serde_json::from_value(config).unwrap();
        std::fs::write(
            root.join(".symora/config.toml"),
            toml::to_string(&config).unwrap(),
        )
        .unwrap();

        let args: &[&str] = if mode == "concurrent-rename" {
            &["rename", "main.go:2:6", "renamed", "--format", "compact"]
        } else {
            &[
                "actions",
                "apply",
                "main.go:2:6",
                "Rename first token",
                "--format",
                "compact",
            ]
        };
        let mut mutation = project_command(&exe, home.path(), args)
            .current_dir(&root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while !root.join("ready").exists() {
            if mutation.try_wait().unwrap().is_some() || Instant::now() >= deadline {
                let _ = mutation.kill();
                let output = mutation.wait_with_output().unwrap();
                panic!(
                    "{mode} did not reach its request barrier: {}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let restart = project_command(&exe, home.path(), &["daemon", "restart", "--project"])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(!restart.status.success());
        let rejected: serde_json::Value = serde_json::from_slice(&restart.stdout).unwrap();
        assert_eq!(rejected["error"]["code"], "conflict", "{rejected}");
        let reader = project_command(
            &exe,
            home.path(),
            &["hover", "main.go:2:6", "--format", "compact"],
        )
        .current_dir(&root)
        .output()
        .unwrap();
        assert!(
            reader.status.success(),
            "{mode}: {}{}",
            String::from_utf8_lossy(&reader.stdout),
            String::from_utf8_lossy(&reader.stderr)
        );
        let reader_payload: serde_json::Value = serde_json::from_slice(&reader.stdout).unwrap();
        assert!(
            reader_payload.get("error").is_none(),
            "{mode}: {reader_payload}"
        );
        assert!(
            root.join("changed").exists(),
            "{mode} did not sync the external change"
        );

        let deadline = Instant::now() + Duration::from_secs(20);
        while mutation.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                let _ = mutation.kill();
                let output = mutation.wait_with_output().unwrap();
                panic!(
                    "{mode} did not reject its changed request: {}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = mutation.wait_with_output().unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(!output.status.success(), "{mode}: {payload}");
        assert!(payload.get("error").is_some(), "{mode}: {payload}");
        assert_eq!(
            std::fs::read_to_string(&source).unwrap(),
            "package main\nfunc prefixabc() {}\n",
            "{mode}: {payload}"
        );
    }
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

/// A home too deep for a Unix socket's path (104 bytes on macOS, 108 on
/// Linux) runs in process, as the direct run answers, instead of failing on
/// a daemon it could never reach.
#[test]
fn a_daemon_socket_path_too_long_to_bind_runs_in_process() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("h".repeat(120));
    let project = home.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("lib.rs"), "pub fn alpha() {}\n").unwrap();

    let out = project_command(
        Path::new(env!("CARGO_BIN_EXE_symora")),
        &home,
        &["--format", "compact", "search", "content", "alpha"],
    )
    .current_dir(&project)
    .output()
    .unwrap();
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["count"], 1, "{value}");
}

/// Under the same home the lifecycle commands answer what is true instead of
/// failing on the socket: none is running, so there is nothing to stop or
/// unload, and starting one asks for a shorter home, naming the path's limit.
#[test]
fn a_daemon_socket_path_too_long_to_bind_leaves_nothing_to_stop_or_start() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("h".repeat(120));
    let project = home.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let socket_dir = home.join(".symora").display().to_string();
    let run = |args: &[&str]| {
        let out = project_command(Path::new(env!("CARGO_BIN_EXE_symora")), &home, args)
            .current_dir(&project)
            .output()
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "{args:?}: {e}: {}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        });
        (out.status.success(), value)
    };

    for (args, field) in [
        (&["daemon", "status"][..], "running"),
        (&["daemon", "stop"], "stopped"),
        (&["daemon", "restart", "--project"], "restarted"),
    ] {
        let (succeeded, value) = run(args);
        assert!(succeeded, "{args:?}: {value}");
        assert!(value.get("error").is_none(), "{args:?}: {value}");
        assert_eq!(value[field], false, "{args:?}: {value}");
        let message = value["message"].as_str().unwrap();
        assert!(message.contains(&socket_dir), "{args:?}: {value}");
        assert!(
            message.contains("must be shorter than"),
            "{args:?}: {value}"
        );
    }
    for args in [&["daemon", "start"][..], &["daemon", "restart"]] {
        let (succeeded, value) = run(args);
        assert!(!succeeded, "{args:?}: {value}");
        let error = &value["error"];
        assert_eq!(error["code"], "precondition_failed", "{args:?}: {value}");
        let message = error["message"].as_str().unwrap();
        assert!(message.contains(&socket_dir), "{args:?}: {value}");
        assert!(
            message.contains("must be shorter than"),
            "{args:?}: {value}"
        );
        let hint = error["hint"].as_str().unwrap();
        assert!(hint.contains("HOME"), "{args:?}: {value}");
        assert!(hint.contains("runs in process"), "{args:?}: {value}");
    }
}

/// A sandbox that forbids the daemon's directory still gets answers: the
/// process runs in process instead of failing every command on the startup
/// lock it cannot open.
#[test]
fn an_unwritable_daemon_directory_runs_in_process() {
    use std::os::unix::fs::PermissionsExt;

    let home = tempfile::tempdir().unwrap();
    let project = home.path().join("project");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(project.join("lib.rs"), "pub fn alpha() {}\n").unwrap();
    std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
    let restore =
        || std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    if std::fs::create_dir(home.path().join("probe")).is_ok() {
        restore();
        return;
    }

    let out = project_command(
        Path::new(env!("CARGO_BIN_EXE_symora")),
        home.path(),
        &["--format", "compact", "search", "content", "alpha"],
    )
    .current_dir(&project)
    .output()
    .unwrap();
    restore();
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["count"], 1, "{value}");
    assert!(!home.path().join(".symora").exists());
}
