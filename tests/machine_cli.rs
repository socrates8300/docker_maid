use bollard::models::NetworkCreateRequest;
use bollard::Docker;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_docker_maid")
}

fn temp_dir(label: &str) -> PathBuf {
    let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "docker-maid-machine-{label}-{}-{id}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("create machine test directory");
    path
}

fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(binary())
        .args(args)
        .current_dir(root)
        .env("DOCKER_HOST", "tcp://127.0.0.1:1")
        .env("XDG_STATE_HOME", root.join("state"))
        .env("HOME", root.join("home"))
        .output()
        .expect("run docker_maid machine command")
}

fn document(bytes: &[u8]) -> Value {
    let source = std::str::from_utf8(bytes).expect("UTF-8 machine output");
    assert_eq!(source.lines().count(), 1, "expected one JSON document");
    let value: Value = serde_json::from_str(source).expect("valid JSON document");
    assert_eq!(value["schema_version"], 1);
    value
}

fn error_document(output: &Output, expected_code: i32, expected_kind: &str) -> Value {
    assert_eq!(output.status.code(), Some(expected_code));
    assert!(output.stdout.is_empty());
    let value = document(&output.stderr);
    assert_eq!(value["error"]["kind"], expected_kind);
    assert!(value["error"]["message"].is_string());
    assert!(value["error"]["details"].is_array());
    value
}

#[test]
fn config_and_version_json_are_single_versioned_documents() {
    let root = temp_dir("config");
    fs::write(
        root.join("docker_maid.toml"),
        "[defaults]\ninterval = '5m'\n",
    )
    .expect("write configuration");

    let generated = run(&root, &["--format", "json", "config", "default"]);
    assert!(generated.status.success());
    assert!(generated.stderr.is_empty());
    assert_eq!(document(&generated.stdout)["command"], "config.default");

    let checked = run(&root, &["config", "check", "--json"]);
    assert!(checked.status.success());
    assert!(checked.stderr.is_empty());
    assert_eq!(document(&checked.stdout)["command"], "config.check");

    let printed = run(&root, &["--json", "config", "print"]);
    assert!(printed.status.success());
    assert!(printed.stderr.is_empty());
    assert_eq!(document(&printed.stdout)["command"], "config.print");

    let version = run(&root, &["--version", "--format", "json"]);
    assert!(version.status.success());
    assert!(version.stderr.is_empty());
    let version = document(&version.stdout);
    assert_eq!(version["command"], "version");
    assert!(version["version"].is_string());

    fs::remove_dir_all(root).expect("remove machine test directory");
}

#[test]
fn json_errors_preserve_exit_codes_and_leave_stdout_empty() {
    let root = temp_dir("errors");

    let usage = run(&root, &["--format", "json", "unknown"]);
    error_document(&usage, 64, "usage");

    fs::write(
        root.join("docker_maid.toml"),
        "[defaults]\nintervall='5m'\n",
    )
    .expect("write invalid configuration");
    let config = run(&root, &["--json", "config", "check"]);
    error_document(&config, 3, "config_invalid");

    fs::write(
        root.join("docker_maid.toml"),
        "[[rules.networks]]\nname='machine'\nselect.names=['^machine-']\norphan=true\n",
    )
    .expect("write valid configuration");
    let docker = run(&root, &["plan", "--format", "json"]);
    error_document(&docker, 5, "docker_unreachable");

    let blocked = root.join("blocked");
    fs::write(&blocked, "not a directory").expect("write blocking state path");
    let state = Command::new(binary())
        .args(["--json", "protect", "image", "machine:test"])
        .env("XDG_STATE_HOME", &blocked)
        .env("HOME", root.join("home"))
        .output()
        .expect("run state failure");
    error_document(&state, 6, "state_io");

    fs::remove_dir_all(root).expect("remove machine test directory");
}

#[test]
fn protection_commands_emit_machine_results_without_docker() {
    let root = temp_dir("protection");
    let protected = run(
        &root,
        &["protect", "network", "machine-net", "--format", "json"],
    );
    assert!(protected.status.success());
    assert!(protected.stderr.is_empty());
    let protected = document(&protected.stdout);
    assert_eq!(protected["command"], "protect");
    assert_eq!(protected["resource_kind"], "network");
    assert_eq!(protected["changed"], 1);
    assert_eq!(protected["total_runtime_protection_entries"], 1);

    let unprotected = run(&root, &["--json", "unprotect", "network", "machine-net"]);
    assert!(unprotected.status.success());
    assert!(unprotected.stderr.is_empty());
    let unprotected = document(&unprotected.stdout);
    assert_eq!(unprotected["command"], "unprotect");
    assert_eq!(unprotected["changed"], 1);
    assert_eq!(unprotected["total_runtime_protection_entries"], 0);

    fs::remove_dir_all(root).expect("remove machine test directory");
}

#[test]
fn json_payload_broken_pipe_exits_zero_without_panic_output() {
    let root = temp_dir("broken-pipe");
    let (reader, writer) = os_pipe::pipe().expect("create stdout pipe");
    drop(reader);
    let child = Command::new(binary())
        .args(["--json", "config", "default"])
        .current_dir(&root)
        .stdout(Stdio::from(writer))
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn JSON command with closed reader");
    let output = child.wait_with_output().expect("wait for JSON command");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    fs::remove_dir_all(root).expect("remove machine test directory");
}

#[cfg(target_os = "linux")]
#[test]
fn json_payload_write_failure_exits_seven_with_machine_error() {
    let root = temp_dir("write-error");
    let full = fs::OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .expect("open /dev/full");
    let output = Command::new(binary())
        .args(["--json", "config", "default"])
        .current_dir(&root)
        .stdout(Stdio::from(full))
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn JSON command with full stdout")
        .wait_with_output()
        .expect("wait for JSON command");
    error_document(&output, 7, "internal");
    fs::remove_dir_all(root).expect("remove machine test directory");
}

#[cfg(unix)]
fn send_term(child: &Child) {
    let status = Command::new("sh")
        .args([
            "-c",
            "kill -TERM \"$1\"",
            "docker-maid-machine-signal",
            &child.id().to_string(),
        ])
        .status()
        .expect("send SIGTERM");
    assert!(status.success());
}

#[cfg(unix)]
#[test]
fn daemon_json_is_parseable_ndjson_during_failure_retry_and_shutdown() {
    let root = temp_dir("daemon");
    let config = root.join("daemon.toml");
    let stdout_path = root.join("daemon.ndjson");
    fs::write(
        &config,
        "[defaults]\ninterval='50ms'\n[[rules.networks]]\nname='machine'\nselect.names=['^machine-']\norphan=true\n",
    )
    .expect("write daemon configuration");
    let stdout = fs::File::create(&stdout_path).expect("create NDJSON capture");
    let child = Command::new(binary())
        .args([
            "--config",
            config.to_str().expect("UTF-8 config path"),
            "--format",
            "json",
            "daemon",
        ])
        .env("DOCKER_HOST", "unix:///definitely/missing/docker.sock")
        .env("XDG_STATE_HOME", root.join("state"))
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn JSON daemon");

    let deadline = Instant::now() + Duration::from_secs(3);
    while !fs::read_to_string(&stdout_path)
        .is_ok_and(|source| source.contains("\"event\":\"daemon_started\""))
    {
        assert!(
            Instant::now() < deadline,
            "daemon did not emit startup event"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(180));
    send_term(&child);
    let output = child.wait_with_output().expect("wait for JSON daemon");
    assert!(output.status.success());
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let source = fs::read_to_string(&stdout_path).expect("read NDJSON capture");
    let events = source
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("valid NDJSON event"))
        .collect::<Vec<_>>();
    assert!(events.len() >= 5, "events: {events:#?}");
    assert!(events.iter().all(|event| event["schema_version"] == 1));
    assert_eq!(
        events.first().expect("startup event")["event"],
        "daemon_started"
    );
    assert!(events.iter().any(|event| event["event"] == "pass_started"));
    assert!(events.iter().any(|event| event["event"] == "pass_error"));
    assert_eq!(
        events.last().expect("shutdown event")["event"],
        "daemon_stopped"
    );

    fs::remove_dir_all(root).expect("remove machine test directory");
}

#[tokio::test]
async fn live_plan_clean_apply_and_status_have_json_parity() {
    if std::env::var_os("DOCKER_MAID_LIVE_TEST").is_none() {
        return;
    }

    let root = temp_dir("live");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let label = format!("machine-{nonce}");
    let network_name = format!("docker-maid-machine-{nonce}");
    let config = root.join("machine.toml");
    fs::write(
        &config,
        format!(
            "[[rules.networks]]\nname='machine-json'\nselect.labels=['docker-maid.machine={label}']\norphan=true\n"
        ),
    )
    .expect("write live configuration");

    let docker = Docker::connect_with_defaults().expect("connect to Docker");
    docker
        .create_network(NetworkCreateRequest {
            name: network_name.clone(),
            labels: Some(HashMap::from([("docker-maid.machine".to_owned(), label)])),
            ..Default::default()
        })
        .await
        .expect("create machine network");

    let live_run = |command: &[&str]| {
        let mut args = vec![
            "--config",
            config.to_str().expect("UTF-8 config path"),
            "--json",
        ];
        args.extend_from_slice(command);
        Command::new(binary())
            .args(args)
            .env("XDG_STATE_HOME", root.join("state"))
            .output()
            .expect("run live machine command")
    };

    let plan = live_run(&["plan"]);
    assert_eq!(plan.status.code(), Some(1));
    assert!(plan.stderr.is_empty());
    let plan = document(&plan.stdout);
    assert_eq!(plan["command"], "plan");
    assert_eq!(plan["pending_removals"], 1);
    assert!(plan["items"].as_array().is_some_and(|items| items
        .iter()
        .any(|item| item["name"] == network_name && item["action"] == "remove")));

    let dry = live_run(&["clean"]);
    assert_eq!(dry.status.code(), Some(1));
    assert_eq!(document(&dry.stdout)["pending_removals"], 1);
    assert!(docker.inspect_network(&network_name, None).await.is_ok());

    let applied = live_run(&["clean", "--apply"]);
    assert!(applied.status.success());
    assert!(applied.stderr.is_empty());
    let applied = document(&applied.stdout);
    assert_eq!(applied["result"]["removed"], 1);
    assert_eq!(applied["result"]["failed"], 0);
    assert!(docker.inspect_network(&network_name, None).await.is_err());

    let status = live_run(&["status"]);
    assert!(status.status.success());
    let warning = document(&status.stderr);
    assert_eq!(warning["warning"]["kind"], "rule_health_regressed");
    let status = document(&status.stdout);
    assert_eq!(status["command"], "status");
    assert!(status["configuration"]["hash"].is_string());
    assert!(status["inventory"].is_object());
    assert!(status["items"].is_array());
    assert!(status["disk_usage"].is_object());
    assert!(status["last_completed_pass"].is_object());

    let _ = docker.remove_network(&network_name).await;
    fs::remove_dir_all(root).expect("remove machine test directory");
}

#[cfg(unix)]
#[tokio::test]
async fn live_daemon_json_emits_plan_action_summary_and_shutdown() {
    if std::env::var_os("DOCKER_MAID_LIVE_TEST").is_none() {
        return;
    }

    let root = temp_dir("live-daemon");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let label = format!("daemon-machine-{nonce}");
    let network_name = format!("docker-maid-machine-daemon-{nonce}");
    let config = root.join("daemon.toml");
    let stdout_path = root.join("daemon.ndjson");
    fs::write(
        &config,
        format!(
            "[defaults]\ninterval='30s'\n[[rules.networks]]\nname='machine-daemon'\nselect.labels=['docker-maid.machine={label}']\norphan=true\n"
        ),
    )
    .expect("write live daemon configuration");

    let docker = Docker::connect_with_defaults().expect("connect to Docker");
    docker
        .create_network(NetworkCreateRequest {
            name: network_name.clone(),
            labels: Some(HashMap::from([("docker-maid.machine".to_owned(), label)])),
            ..Default::default()
        })
        .await
        .expect("create live daemon network");

    let stdout = fs::File::create(&stdout_path).expect("create live NDJSON capture");
    let child = Command::new(binary())
        .args([
            "--config",
            config.to_str().expect("UTF-8 config path"),
            "--json",
            "daemon",
            "--apply",
        ])
        .env("XDG_STATE_HOME", root.join("state"))
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn live JSON daemon");

    let deadline = Instant::now() + Duration::from_secs(10);
    while !fs::read_to_string(&stdout_path)
        .is_ok_and(|source| source.contains("\"event\":\"pass_summary\""))
    {
        assert!(
            Instant::now() < deadline,
            "daemon did not complete startup pass"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    send_term(&child);
    let output = child.wait_with_output().expect("wait for live JSON daemon");
    assert!(output.status.success());
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(docker.inspect_network(&network_name, None).await.is_err());

    let source = fs::read_to_string(&stdout_path).expect("read live NDJSON capture");
    let events = source
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("valid live NDJSON event"))
        .collect::<Vec<_>>();
    let names = events
        .iter()
        .map(|event| event["event"].as_str().expect("event name"))
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            "daemon_started",
            "pass_started",
            "plan",
            "action",
            "pass_summary",
            "daemon_stopped"
        ]
    );
    let action = events
        .iter()
        .find(|event| event["event"] == "action")
        .expect("action event");
    assert_eq!(action["resource_name"], network_name);
    assert_eq!(action["result"], "removed");
    let summary = events
        .iter()
        .find(|event| event["event"] == "pass_summary")
        .expect("summary event");
    assert_eq!(summary["result"]["removed"], 1);
    assert_eq!(summary["result"]["failed"], 0);

    let _ = docker.remove_network(&network_name).await;
    fs::remove_dir_all(root).expect("remove machine test directory");
}
