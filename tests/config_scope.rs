use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::Command;

const SYMORA: &str = env!("CARGO_BIN_EXE_symora");

fn json_ok(root: &Path, home: &Path, args: &[&str]) -> serde_json::Value {
    let out = Command::new(SYMORA)
        .args(args)
        .current_dir(root)
        .env("XDG_CONFIG_HOME", home)
        .env("HOME", home)
        .env("SYMORA_NO_DAEMON", "1")
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
}

#[test]
fn config_show_daemon_scope() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join(".symora")).unwrap();
    std::fs::create_dir(home.path().join("symora")).unwrap();
    let global = home.path().join("symora/config.toml");
    let project = root
        .path()
        .canonicalize()
        .unwrap()
        .join(".symora/config.toml");
    std::fs::write(&global, "[daemon]\nidle_timeout_mins = 10\n").unwrap();
    std::fs::write(&project, "[daemon]\nidle_timeout_mins = 30\n").unwrap();
    let value = json_ok(root.path(), home.path(), &["config", "show"]);
    assert_eq!(value["config"]["daemon"]["idle_timeout_mins"], 10);
    assert_eq!(
        value["config_errors"],
        serde_json::json!([format!(
            "{}: `daemon.idle_timeout_mins` is read only from the global config ({})",
            project.display(),
            global.display()
        )])
    );
    let value = json_ok(root.path(), home.path(), &["config", "show", "--global"]);
    assert_eq!(value["config"]["daemon"]["idle_timeout_mins"], 10);
    std::fs::remove_file(global).unwrap();
    let value = json_ok(root.path(), home.path(), &["config", "show"]);
    assert_eq!(value["config"]["daemon"]["idle_timeout_mins"], 30);
    assert_eq!(value["config_errors"].as_array().unwrap().len(), 1);
}

#[test]
fn config_init_respects_scope() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let value = json_ok(root.path(), home.path(), &["config", "init"]);
    assert_eq!(value["status"], "created");
    let project_text = std::fs::read_to_string(root.path().join(".symora/config.toml")).unwrap();
    let project: toml::Table = toml::from_str(&project_text).unwrap();
    assert!(!project.contains_key("daemon"));
    let value = json_ok(root.path(), home.path(), &["config", "init", "--global"]);
    assert_eq!(value["status"], "created");
    let global_text = std::fs::read_to_string(home.path().join("symora/config.toml")).unwrap();
    assert_eq!(
        global_text,
        toml::to_string_pretty(&symora::models::config::SymoraConfig::default()).unwrap()
    );
    let global: toml::Table = toml::from_str(&global_text).unwrap();
    assert!(global.contains_key("daemon"));
    let daemon_start = global_text.find("[daemon]\n").unwrap();
    let daemon_end = daemon_start + global_text[daemon_start..].find("[output]\n").unwrap();
    let mut expected = global_text;
    expected.replace_range(daemon_start..daemon_end, "");
    assert_eq!(project_text, expected);
}

#[test]
fn project_init_respects_scope() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    json_ok(root.path(), home.path(), &["init"]);
    let text = std::fs::read_to_string(root.path().join(".symora/config.toml")).unwrap();
    let project: toml::Table = toml::from_str(&text).unwrap();
    assert!(!project.contains_key("daemon"));
    let value = json_ok(root.path(), home.path(), &["config", "show"]);
    assert!(value.get("config_errors").is_none(), "{value}");
}

#[test]
fn daemon_runtime_reads_global_config() {
    if std::env::var_os("SYMORA_TEST_DAEMON_SCOPE").is_some() {
        let config = symora::daemon::server::DaemonRuntimeConfig::load().unwrap();
        assert_eq!(config.idle_timeout.as_secs(), 600);
        assert_eq!(config.max_concurrent, 7);
        std::fs::write(
            ".symora/config.toml",
            "[daemon]\nidle_timeout_mins = 30\n[lsp]\ntimeout_secs = 9\n",
        )
        .unwrap();
        let merged =
            symora::services::config::load_merged_config_sync(&std::env::current_dir().unwrap())
                .unwrap();
        assert_eq!(merged.daemon.idle_timeout_mins, 10);
        assert_eq!(merged.ignored_keys.len(), 1);
        let lsp = symora::daemon::server::DaemonRuntimeConfig::load_lsp_config(
            &std::env::current_dir().unwrap(),
        );
        assert_eq!(
            lsp.timeout_for(symora::models::symbol::Language::Go, "textDocument/hover")
                .as_secs(),
            9
        );
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join(".symora")).unwrap();
    std::fs::create_dir(home.path().join("symora")).unwrap();
    std::fs::write(
        home.path().join("symora/config.toml"),
        "[daemon]\nidle_timeout_mins = 10\nmax_concurrent = 7\n",
    )
    .unwrap();
    // A project parse failure must not affect daemon-wide settings.
    std::fs::write(
        root.path().join(".symora/config.toml"),
        "[daemon]\nidle_timeout_mins = \"bad\"\n",
    )
    .unwrap();
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "daemon_runtime_reads_global_config",
            "--nocapture",
        ])
        .current_dir(root.path())
        .env("XDG_CONFIG_HOME", home.path())
        .env("HOME", home.path())
        .env("SYMORA_TEST_DAEMON_SCOPE", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn ignored_daemon_keys_reach_app_and_doctor() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join(".symora")).unwrap();
    let project = root
        .path()
        .canonicalize()
        .unwrap()
        .join(".symora/config.toml");
    std::fs::write(&project, "[daemon]\nidle_timeout_mins = 1\n[lsp.servers.rust]\ncommand = \"/missing/symora-test-rust-analyzer\"\n[lsp.servers.klingon]\ncommand = \"/missing/server\"\n").unwrap();
    let expected = serde_json::json!(format!(
        "{}: `daemon.idle_timeout_mins` is read only from the global config ({})",
        project.display(),
        home.path().join("symora/config.toml").display()
    ));
    for args in [&["config", "path"][..], &["doctor", "rust"][..]] {
        let value = json_ok(root.path(), home.path(), args);
        assert_eq!(value["config_errors"][0], expected, "{args:?}");
        assert_eq!(
            value["config_errors"].as_array().unwrap().len(),
            if args[0] == "doctor" { 2 } else { 1 }
        );
        if args[0] == "doctor" {
            assert!(
                value["config_errors"][1]
                    .as_str()
                    .unwrap()
                    .contains("lsp.servers.klingon")
            );
        }
    }
}

#[test]
fn daemon_runtime_paths_are_installation_scoped() {
    if std::env::var_os("SYMORA_TEST_DAEMON_PATHS").is_some() {
        let config = symora::daemon::DaemonRuntimeConfig::load().unwrap();
        let exe = std::env::current_exe().unwrap().canonicalize().unwrap();
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        for byte in exe.as_os_str().as_bytes() {
            hash = (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3);
        }
        let base = symora::services::dist::paths::daemon_dir();
        assert_eq!(
            base,
            std::path::PathBuf::from(std::env::var_os("HOME").unwrap()).join(".symora")
        );
        for (path, suffix) in [
            (&config.socket_path, "sock"),
            (&config.pid_path, "pid"),
            (&config.lock_path, "lock"),
            (&config.bind_lock_path, "bind.lock"),
        ] {
            assert_eq!(*path, base.join(format!("daemon-{hash:016x}.{suffix}")));
        }
        assert!(
            config.socket_path.as_os_str().as_bytes().len() < 104,
            "{:?}",
            config.socket_path
        );
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "daemon_runtime_paths_are_installation_scoped",
            "--nocapture",
        ])
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path())
        .env("SYMORA_TEST_DAEMON_PATHS", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
