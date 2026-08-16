use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_docker_maid")
}

fn temp_dir(label: &str) -> PathBuf {
    let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "docker-maid-cli-{label}-{}-{id}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("create test directory");
    path
}

fn run(args: &[&str], current_dir: &PathBuf) -> Output {
    Command::new(binary())
        .args(args)
        .current_dir(current_dir)
        .env("DOCKER_HOST", "tcp://127.0.0.1:1")
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("run docker_maid")
}

fn run_with_xdg(args: &[&str], current_dir: &PathBuf, xdg: &PathBuf) -> Output {
    Command::new(binary())
        .args(args)
        .current_dir(current_dir)
        .env("DOCKER_HOST", "tcp://127.0.0.1:1")
        .env("XDG_CONFIG_HOME", xdg)
        .output()
        .expect("run docker_maid")
}

fn run_with_closed_stdout(args: &[&str], current_dir: &PathBuf) -> Output {
    let (reader, writer) = os_pipe::pipe().expect("create stdout pipe");
    drop(reader);

    let child = Command::new(binary())
        .args(args)
        .current_dir(current_dir)
        .env("DOCKER_HOST", "tcp://127.0.0.1:1")
        .env_remove("XDG_CONFIG_HOME")
        .stdout(Stdio::from(writer))
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn docker_maid with closed stdout reader");
    child
        .wait_with_output()
        .expect("wait for docker_maid with closed stdout reader")
}

#[test]
fn generated_default_validates_without_docker() {
    let root = temp_dir("default");
    let generated = run(&["config", "default"], &root);
    assert!(generated.status.success());
    assert!(generated.stderr.is_empty());

    let source = String::from_utf8(generated.stdout).expect("UTF-8 generated config");
    let parsed = docker_maid::config::Config::parse(&source, std::path::Path::new("<default>"))
        .expect("parse generated config");
    parsed.validate().expect("validate generated config");
    assert!(parsed.rules.containers.is_empty());
    assert!(parsed.rules.images.is_empty());
    assert!(parsed.rules.volumes.is_empty());
    assert!(parsed.rules.networks.is_empty());
    assert!(parsed.rules.build_cache.is_none());

    let path = root.join("generated.toml");
    fs::write(&path, source).expect("write generated config");
    let checked = run(
        &[
            "--config",
            path.to_str().expect("utf-8 path"),
            "config",
            "check",
        ],
        &root,
    );
    assert!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stderr)
    );
    assert!(String::from_utf8_lossy(&checked.stdout).contains("configuration valid:"));

    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn invalid_configurator_override_fails_before_docker_contact() {
    let root = temp_dir("configurator-override");
    let output = run(
        &[
            "config",
            "propose",
            "--profile",
            "workstation",
            "--container-ttl",
            "0s",
            "--candidate",
            "anything",
        ],
        &root,
    );
    assert_eq!(output.status.code(), Some(3));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("stopped container TTL must be greater than zero"));
    assert!(!stderr.contains("Docker"));
    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn malformed_configuration_exits_three() {
    let root = temp_dir("invalid");
    let path = root.join("bad.toml");
    fs::write(&path, "[defaults]\nintervall = \"5m\"\n").expect("write bad config");

    let output = run(
        &[
            "config",
            "check",
            "--config",
            path.to_str().expect("utf-8 path"),
        ],
        &root,
    );
    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown field `intervall`"));

    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn print_emits_normalized_valid_toml() {
    let root = temp_dir("print");
    let path = root.join("docker_maid.toml");
    fs::write(
        &path,
        "[[rules.containers]]\nname='agents'\nstopped_ttl='2h'\nselect.names=['^agent-']\n",
    )
    .expect("write config");

    let output = run(&["config", "print"], &root);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("utf-8 output");
    assert!(stdout.contains("name = \"agents\""));
    assert!(stdout.contains("stopped_ttl = \"2h\""));

    let normalized = root.join("normalized.toml");
    fs::write(&normalized, stdout).expect("write normalized config");
    let rechecked = run(
        &[
            "config",
            "check",
            "--config",
            normalized.to_str().expect("utf-8 path"),
        ],
        &root,
    );
    assert!(rechecked.status.success());

    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn a_retired_adopt_key_is_refused_and_names_its_migration() {
    let root = temp_dir("retired-adopt");
    let path = root.join("legacy.toml");
    fs::write(
        &path,
        "[[rules.containers]]\nname = \"agents\"\nstopped_ttl = \"2h\"\nselect.names = [\"^agent-\"]\nadopt = true\n",
    )
    .expect("write legacy config");

    let output = run(
        &[
            "config",
            "check",
            "--config",
            path.to_str().expect("utf-8 path"),
        ],
        &root,
    );
    assert_eq!(output.status.code(), Some(3));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown field `adopt`"));
    // Rule tables flatten their common fields, so serde cannot list the keys
    // it expected instead. Without the retirement note the operator is told
    // only that a key is unknown, on the line of the table header.
    assert!(stderr.contains("retired key `adopt`:"));
    assert!(stderr.contains("Delete the line"));

    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn no_configuration_surface_emits_the_retired_adopt_key() {
    let root = temp_dir("retired-key-absent");
    let path = root.join("docker_maid.toml");
    fs::write(
        &path,
        "[[rules.containers]]\nname = \"agents\"\nstopped_ttl = \"2h\"\nselect.names = [\"^agent-\"]\n",
    )
    .expect("write config");

    // The human surfaces: the starter operators are told to copy, and the
    // normalized round-trip that rewrites a rule back out.
    for args in [&["config", "default"][..], &["config", "print"][..]] {
        let output = run(args, &root);
        assert!(
            output.status.success(),
            "command: {args:?}; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            !stdout.contains("adopt"),
            "command {args:?} emitted: {stdout}"
        );
    }

    // The machine surfaces. Only the configuration subtree is inspected: the
    // envelope carries the file path, which the test directory name would
    // otherwise satisfy by accident.
    for args in [
        &["--json", "config", "print"][..],
        &["--json", "config", "check"][..],
    ] {
        let output = run(args, &root);
        assert!(
            output.status.success(),
            "command: {args:?}; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let document: Value = serde_json::from_str(
            &String::from_utf8(output.stdout).expect("utf-8 machine document"),
        )
        .expect("valid JSON document");
        let configuration = document
            .get("configuration")
            .expect("machine document carries a configuration")
            .to_string();
        assert!(
            !configuration.contains("adopt"),
            "command {args:?} emitted: {configuration}"
        );
    }

    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn invalid_invocation_exits_sixty_four() {
    let root = temp_dir("usage");
    let output = run(&["config", "unknown"], &root);
    assert_eq!(output.status.code(), Some(64));
    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn xdg_configuration_is_used_when_local_file_is_absent() {
    let root = temp_dir("xdg");
    let current = root.join("current");
    let xdg = root.join("config-home");
    fs::create_dir_all(&current).expect("create current directory");
    fs::create_dir_all(xdg.join("docker_maid")).expect("create xdg directory");
    fs::write(
        xdg.join("docker_maid/config.toml"),
        "[defaults]\ninterval = \"5m\"\n",
    )
    .expect("write xdg config");

    let output = run_with_xdg(&["config", "check"], &current, &xdg);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("config-home"));

    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn payload_commands_exit_cleanly_when_stdout_reader_is_closed() {
    let root = temp_dir("broken-pipe");
    fs::write(
        root.join("docker_maid.toml"),
        "[defaults]\ninterval = \"5m\"\n",
    )
    .expect("write config");

    for args in [&["config", "default"][..], &["config", "print"][..]] {
        let output = run_with_closed_stdout(args, &root);
        assert_eq!(output.status.code(), Some(0), "command: {args:?}");
        assert!(
            output.stderr.is_empty(),
            "command: {args:?}; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fs::remove_dir_all(root).expect("remove test directory");
}
