use bollard::models::NetworkCreateRequest;
use bollard::Docker;
use docker_maid::activity::ActivityJournal;
use docker_maid::state::StatePaths;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_docker_maid")
}

fn fixture_config() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/read_only_plan.toml")
}

/// These tests run concurrently in one process, and the system clock can
/// report the same microsecond for two of them. A counter keeps every root
/// distinct so one test never deletes another's directory.
static NEXT_ROOT_ID: AtomicU64 = AtomicU64::new(0);

fn temp_dir(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let sequence = NEXT_ROOT_ID.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "docker-maid-daemon-{label}-{}-{nonce}-{sequence}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("create daemon test directory");
    path
}

#[cfg(unix)]
fn send_signal(child: &Child, signal: &str) {
    let signal = match signal {
        "HUP" => "-HUP",
        "TERM" => "-TERM",
        other => panic!("unsupported test signal: {other}"),
    };
    let status = Command::new("sh")
        .args([
            "-c",
            "kill \"$1\" \"$2\"",
            "docker-maid-test-signal",
            signal,
            &child.id().to_string(),
        ])
        .status()
        .expect("send daemon signal");
    assert!(status.success(), "kill -{signal} failed");
}

#[test]
fn invalid_interval_exits_sixty_four_before_docker_contact() {
    let root = temp_dir("invalid-interval");
    let output = Command::new(binary())
        .args([
            "--config",
            fixture_config().to_str().expect("UTF-8 fixture path"),
            "daemon",
            "--interval",
            "never",
        ])
        .env("DOCKER_HOST", "unix:///definitely/missing/docker.sock")
        .env("XDG_STATE_HOME", root.join("state"))
        .output()
        .expect("run invalid daemon interval");

    assert_eq!(output.status.code(), Some(64));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--interval"));
    fs::remove_dir_all(root).expect("remove daemon test directory");
}

#[cfg(unix)]
#[test]
fn daemon_retries_docker_failures_on_the_interval_and_stops_cleanly() {
    let root = temp_dir("retry");
    let stderr_path = root.join("daemon.stderr");
    let stderr_file = fs::File::create(&stderr_path).expect("create daemon stderr capture");
    let child = Command::new(binary())
        .args([
            "--config",
            fixture_config().to_str().expect("UTF-8 fixture path"),
            "daemon",
            "--interval",
            "50ms",
        ])
        .env("DOCKER_HOST", "unix:///definitely/missing/docker.sock")
        .env("XDG_STATE_HOME", root.join("state"))
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .expect("spawn retrying daemon");

    let startup_deadline = Instant::now() + Duration::from_secs(3);
    while !fs::read_to_string(&stderr_path).is_ok_and(|stderr| stderr.contains("daemon: started")) {
        assert!(
            Instant::now() < startup_deadline,
            "daemon did not report startup readiness"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(220));
    send_signal(&child, "TERM");
    let output = child.wait_with_output().expect("wait for daemon");
    let stderr = fs::read_to_string(&stderr_path).expect("read daemon stderr capture");
    let attempts = stderr.matches("daemon pass ").count();

    assert!(output.status.success(), "{stderr}");
    assert!(output.stdout.is_empty());
    assert!(
        (2..=8).contains(&attempts),
        "unexpected attempt count: {attempts}"
    );
    assert!(stderr.contains("shutdown requested"));
    fs::remove_dir_all(root).expect("remove daemon test directory");
}

fn live_test_enabled() -> bool {
    std::env::var_os("DOCKER_MAID_LIVE_TEST").is_some()
}

fn live_config(label: &str, protected_name: Option<&str>) -> String {
    let protection = protected_name.map_or_else(String::new, |name| {
        format!("[protect]\nnames = ['^{name}$']\n\n")
    });
    format!(
        "[defaults]\ninterval = '30s'\n\n{protection}[[rules.networks]]\nname = 'daemon-live'\nselect.labels = ['docker-maid.daemon={label}']\norphan = true\norphan_for = '1s'\n"
    )
}

async fn create_network(docker: &Docker, name: &str, label: &str) {
    docker
        .create_network(NetworkCreateRequest {
            name: name.to_owned(),
            labels: Some(HashMap::from([(
                "docker-maid.daemon".to_owned(),
                label.to_owned(),
            )])),
            ..Default::default()
        })
        .await
        .expect("create daemon live network");
}

/// Start the observed-unreferenced clock for fresh fixtures and wait past the
/// one-second floor, so the daemon's first pass has a measurement to act on.
async fn observe_past_floor(config: &Path, state_home: &Path) {
    let output = Command::new(binary())
        .args([
            "--config",
            config.to_str().expect("UTF-8 live config path"),
            "plan",
        ])
        .env("XDG_STATE_HOME", state_home)
        .output()
        .expect("run warm-up plan");
    assert!(
        output.status.success() || output.status.code() == Some(1),
        "warm-up plan failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    tokio::time::sleep(Duration::from_millis(1_200)).await;
}

async fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !predicate() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for daemon state"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_for_network_removal(docker: &Docker, name: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while docker.inspect_network(name, None).await.is_ok() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for daemon to remove {name}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn completed_pass_count(journal: &Path) -> usize {
    fs::read_to_string(journal).map_or(0, |source| {
        source.matches("\"kind\":\"pass_summary\"").count()
    })
}

fn pass_started(journal: &Path) -> bool {
    fs::read_to_string(journal).is_ok_and(|source| source.contains("\"kind\":\"pass_started\""))
}

#[cfg(unix)]
#[tokio::test]
async fn live_daemon_reloads_on_sighup_applies_and_stops_on_sigterm() {
    if !live_test_enabled() {
        return;
    }

    let docker = Docker::connect_with_defaults().expect("connect to live Docker");
    let root = temp_dir("live");
    let unique = root.file_name().expect("temporary name").to_string_lossy();
    let label = format!("{unique}-label");
    let first = format!("{unique}-startup");
    let reloaded = format!("{unique}-reloaded");
    let config = root.join("daemon.toml");
    let state_home = root.join("state");
    let journal_path = state_home.join("docker_maid/activity.jsonl");

    create_network(&docker, &first, &label).await;
    fs::write(&config, live_config(&label, None)).expect("write daemon config");
    observe_past_floor(&config, &state_home).await;
    let child = Command::new(binary())
        .args([
            "--config",
            config.to_str().expect("UTF-8 live config path"),
            "daemon",
            "--apply",
            "--interval",
            "30s",
        ])
        .env("XDG_STATE_HOME", &state_home)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn live daemon");

    wait_for_network_removal(&docker, &first, Duration::from_secs(5)).await;
    wait_until(Duration::from_secs(2), || {
        completed_pass_count(&journal_path) >= 1
    })
    .await;

    create_network(&docker, &reloaded, &label).await;
    fs::write(&config, live_config(&label, Some(&reloaded))).expect("protect reloaded network");
    send_signal(&child, "HUP");
    wait_until(Duration::from_secs(3), || {
        completed_pass_count(&journal_path) >= 2
    })
    .await;
    let protected_survived = docker.inspect_network(&reloaded, None).await.is_ok();

    fs::write(&config, live_config(&label, None)).expect("remove config protection");
    // The reloaded network's observed clock started on the pass above, so wait
    // past the one-second floor before asking for the pass that removes it.
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    send_signal(&child, "HUP");
    wait_for_network_removal(&docker, &reloaded, Duration::from_secs(5)).await;

    send_signal(&child, "TERM");
    let output: Output = child.wait_with_output().expect("wait for live daemon");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let last = ActivityJournal::new(StatePaths::new(state_home.join("docker_maid")))
        .last_completed_pass()
        .expect("read daemon activity")
        .expect("completed daemon pass");

    let _ = docker.remove_network(&first).await;
    let _ = docker.remove_network(&reloaded).await;
    fs::remove_dir_all(root).expect("remove live daemon directory");

    assert!(
        protected_survived,
        "SIGHUP reload ignored config protection"
    );
    assert!(output.status.success(), "{stderr}");
    assert!(stdout.matches("Daemon pass ").count() >= 3, "{stdout}");
    assert!(stderr.matches("SIGHUP received").count() >= 2, "{stderr}");
    assert!(stderr.contains("shutdown requested"), "{stderr}");
    assert_eq!(last.source, "daemon");
}

#[cfg(unix)]
#[tokio::test]
async fn live_sigterm_drains_the_active_pass_before_exit() {
    if !live_test_enabled() {
        return;
    }

    let docker = Docker::connect_with_defaults().expect("connect to live Docker");
    let root = temp_dir("drain");
    let unique = root.file_name().expect("temporary name").to_string_lossy();
    let label = format!("{unique}-label");
    let networks = (0..8)
        .map(|index| format!("{unique}-drain-{index:02}"))
        .collect::<Vec<_>>();
    for network in &networks {
        create_network(&docker, network, &label).await;
    }

    let config = root.join("daemon.toml");
    let state_home = root.join("state");
    let journal_path = state_home.join("docker_maid/activity.jsonl");
    fs::write(&config, live_config(&label, None)).expect("write drain config");
    observe_past_floor(&config, &state_home).await;
    let child = Command::new(binary())
        .args([
            "--config",
            config.to_str().expect("UTF-8 drain config path"),
            "daemon",
            "--apply",
            "--interval",
            "30s",
        ])
        .env("XDG_STATE_HOME", &state_home)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn draining daemon");

    wait_until(Duration::from_secs(3), || pass_started(&journal_path)).await;
    send_signal(&child, "TERM");
    let output = child.wait_with_output().expect("wait for draining daemon");
    let all_removed = {
        let mut removed = true;
        for network in &networks {
            removed &= docker.inspect_network(network, None).await.is_err();
        }
        removed
    };
    let last = ActivityJournal::new(StatePaths::new(state_home.join("docker_maid")))
        .last_completed_pass()
        .expect("read drained activity")
        .expect("completed drained pass");

    for network in &networks {
        let _ = docker.remove_network(network).await;
    }
    fs::remove_dir_all(root).expect("remove drain test directory");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(all_removed, "SIGTERM interrupted the active deletion pass");
    assert_eq!(last.source, "daemon");
    assert_eq!(
        last.removed_count,
        u64::try_from(networks.len()).expect("network count fits u64")
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("shutdown requested"));
}

#[cfg(unix)]
#[tokio::test]
async fn live_daemon_is_read_only_without_apply() {
    if !live_test_enabled() {
        return;
    }

    let docker = Docker::connect_with_defaults().expect("connect to live Docker");
    let root = temp_dir("dry-run");
    let unique = root.file_name().expect("temporary name").to_string_lossy();
    let label = format!("{unique}-label");
    let network = format!("{unique}-kept");
    let config = root.join("daemon.toml");
    let stderr_path = root.join("daemon.stderr");
    let stderr_file = fs::File::create(&stderr_path).expect("create daemon stderr capture");

    create_network(&docker, &network, &label).await;
    fs::write(&config, live_config(&label, None)).expect("write dry-run daemon config");
    let child = Command::new(binary())
        .args([
            "--config",
            config.to_str().expect("UTF-8 dry-run config path"),
            "daemon",
            "--interval",
            "100ms",
        ])
        .env("XDG_STATE_HOME", root.join("state"))
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .expect("spawn dry-run daemon");

    wait_until(Duration::from_secs(3), || {
        fs::read_to_string(&stderr_path)
            .is_ok_and(|stderr| stderr.contains("daemon: started in dry-run mode"))
    })
    .await;
    tokio::time::sleep(Duration::from_millis(350)).await;
    send_signal(&child, "TERM");
    let output = child.wait_with_output().expect("wait for dry-run daemon");
    let survived = docker.inspect_network(&network, None).await.is_ok();

    let _ = docker.remove_network(&network).await;
    fs::remove_dir_all(root).expect("remove dry-run daemon directory");

    assert!(output.status.success());
    assert!(survived, "daemon without --apply mutated Docker");
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .matches("Daemon pass ")
            .count()
            >= 1
    );
}
