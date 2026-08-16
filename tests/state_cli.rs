use docker_maid::state::{ProtectionKind, ProtectionState};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_docker_maid")
}

fn temp_dir(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "docker-maid-state-cli-{label}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("create test directory");
    path
}

fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(binary())
        .args(args)
        .env("XDG_STATE_HOME", root.join("state"))
        .env("HOME", root.join("home"))
        .output()
        .expect("run docker_maid")
}

fn read_state(root: &Path) -> ProtectionState {
    let source = fs::read_to_string(root.join("state/docker_maid/protection.toml"))
        .expect("read protection state");
    toml::from_str(&source).expect("parse protection state")
}

#[test]
fn protect_and_unprotect_persist_typed_entries_across_processes() {
    let root = temp_dir("persistence");
    let added = run(&root, &["protect", "network", "shared", "second-network"]);
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    assert!(String::from_utf8_lossy(&added.stdout).contains("added 2, total 2"));

    let duplicate = run(&root, &["protect", "network", "shared"]);
    assert!(duplicate.status.success());
    assert!(String::from_utf8_lossy(&duplicate.stdout).contains("added 0, total 2"));

    let removed = run(&root, &["unprotect", "network", "shared"]);
    assert!(
        removed.status.success(),
        "{}",
        String::from_utf8_lossy(&removed.stderr)
    );
    let state = read_state(&root);
    assert_eq!(state.schema_version, 1);
    assert_eq!(state.entries.len(), 1);
    assert_eq!(state.entries[0].kind, ProtectionKind::Network);
    assert_eq!(state.entries[0].value, "second-network");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let directory_mode = fs::metadata(root.join("state/docker_maid"))
            .expect("state directory metadata")
            .permissions()
            .mode()
            & 0o777;
        let file_mode = fs::metadata(root.join("state/docker_maid/protection.toml"))
            .expect("state file metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(directory_mode, 0o700);
        assert_eq!(file_mode, 0o600);
    }
    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn concurrent_protect_processes_do_not_lose_updates() {
    let root = temp_dir("concurrency");
    let children = (0..16)
        .map(|index| {
            Command::new(binary())
                .args(["protect", "volume", &format!("volume-{index:02}")])
                .env("XDG_STATE_HOME", root.join("state"))
                .env("HOME", root.join("home"))
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn protect process")
        })
        .collect::<Vec<_>>();
    for child in children {
        let output = child.wait_with_output().expect("wait for protect process");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let state = read_state(&root);
    assert_eq!(state.entries.len(), 16);
    assert!(state
        .entries
        .iter()
        .all(|entry| entry.kind == ProtectionKind::Volume));
    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn unprotect_refuses_config_sourced_protection() {
    let root = temp_dir("config-source");
    let config = root.join("config.toml");
    fs::write(&config, "[protect]\nnames = ['^production$']\n").expect("write configuration");
    let output = run(
        &root,
        &[
            "--config",
            config.to_str().expect("UTF-8 config path"),
            "unprotect",
            "container",
            "production",
        ],
    );

    assert_eq!(output.status.code(), Some(6));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("protect.names[0]"));
    assert!(stderr.contains(&format!("{}:2", config.display())));
    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn unsafe_state_path_fails_with_exit_six_and_no_payload() {
    let root = temp_dir("state-error");
    let blocked = root.join("blocked");
    fs::write(&blocked, "not a directory").expect("write blocking file");
    let output = Command::new(binary())
        .args(["protect", "image", "example:test"])
        .env("XDG_STATE_HOME", &blocked)
        .env("HOME", root.join("home"))
        .output()
        .expect("run docker_maid");

    assert_eq!(output.status.code(), Some(6));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("state path"));
    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn state_falls_back_to_home_when_xdg_is_unset() {
    let root = temp_dir("home-fallback");
    let output = Command::new(binary())
        .args(["protect", "image", "fallback:test"])
        .env_remove("XDG_STATE_HOME")
        .env("HOME", root.join("home"))
        .output()
        .expect("run docker_maid");

    assert!(output.status.success());
    assert!(root
        .join("home/.local/state/docker_maid/protection.toml")
        .is_file());
    fs::remove_dir_all(root).expect("remove test directory");
}
