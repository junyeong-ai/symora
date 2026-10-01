#![cfg(unix)]

use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

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
        std::fs::copy(env!("CARGO_BIN_EXE_symora"), exe).unwrap();
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
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::time::{Duration, Instant};

    let home = tempfile::tempdir().unwrap();
    let exe = home.path().join("symora");
    std::fs::copy(env!("CARGO_BIN_EXE_symora"), &exe).unwrap();
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
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            let (mut stream, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(e) => panic!("{e}"),
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            let request: symora::daemon::protocol::Request = serde_json::from_str(&line).unwrap();
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
        panic!("stale daemon was not shut down");
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
        std::fs::copy(env!("CARGO_BIN_EXE_symora"), exe).unwrap();
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
