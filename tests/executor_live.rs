use bollard::models::{
    ContainerCreateBody, EndpointSettings, NetworkCreateRequest, NetworkingConfig,
};
use bollard::query_parameters::{CreateContainerOptionsBuilder, RemoveContainerOptionsBuilder};
use bollard::Docker;
use docker_maid::config::{load_config, Config};
use docker_maid::executor::{execute_plan, ExecutionReport, TargetStatus};
use docker_maid::inventory::collect_inventory;
use docker_maid::plan::{build_plan, Action, Plan, ResourceKind};
use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

fn live_test_enabled() -> bool {
    std::env::var_os("DOCKER_MAID_LIVE_TEST").is_some()
}

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_docker_maid")
}

fn temp_dir() -> PathBuf {
    let suffix = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos()
    );
    let path = std::env::temp_dir().join(format!("docker-maid-executor-live-{suffix}"));
    fs::create_dir_all(&path).expect("create live test directory");
    path
}

fn now_epoch_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs()
        .try_into()
        .expect("time fits i64")
}

fn network_config(rule_label: &str, protected_name: Option<&str>) -> String {
    let protection = protected_name.map_or_else(String::new, |name| {
        format!("[protect]\nnames = ['^{name}$']\n\n")
    });
    format!(
        "{protection}[[rules.networks]]\nname = 'live-race'\nselect.labels = ['docker-maid.live={rule_label}']\norphan = true\n"
    )
}

async fn capture_plan(config_path: &Path) -> (Config, String, Plan) {
    let loaded = load_config(Some(config_path), Path::new("."), None).expect("load live config");
    let inventory = collect_inventory(false)
        .await
        .expect("collect live inventory");
    let plan = build_plan(&loaded.config, inventory, now_epoch_seconds()).expect("build live plan");
    (loaded.config, loaded.source, plan)
}

async fn create_network(docker: &Docker, name: &str, label: &str) {
    docker
        .create_network(NetworkCreateRequest {
            name: name.to_owned(),
            labels: Some(HashMap::from([(
                "docker-maid.live".to_owned(),
                label.to_owned(),
            )])),
            ..Default::default()
        })
        .await
        .expect("create live network");
}

async fn create_network_reference(docker: &Docker, name: &str, network: &str) -> String {
    let options = CreateContainerOptionsBuilder::default().name(name).build();
    let endpoints = HashMap::from([(network.to_owned(), EndpointSettings::default())]);
    docker
        .create_container(
            Some(options),
            ContainerCreateBody {
                image: Some("busybox:latest".to_owned()),
                cmd: Some(vec!["true".to_owned()]),
                networking_config: Some(NetworkingConfig {
                    endpoints_config: Some(endpoints),
                }),
                ..Default::default()
            },
        )
        .await
        .expect("create live reference container")
        .id
}

async fn remove_container(docker: &Docker, id: &str) {
    let options = RemoveContainerOptionsBuilder::default()
        .force(true)
        .v(false)
        .build();
    let _ = docker.remove_container(id, Some(options)).await;
}

fn assert_network_target(plan: &Plan, name: &str) {
    assert!(plan.decisions.iter().any(|decision| {
        decision.resource.kind == ResourceKind::Network
            && decision.resource.name == name
            && decision.action == Action::Remove
    }));
}

fn assert_one_skip(report: &ExecutionReport, detail: &str) {
    assert!(report.has_partial_failure());
    assert_eq!(report.outcomes.len(), 1);
    assert_eq!(report.outcomes[0].status, TargetStatus::Skipped);
    assert!(report.outcomes[0].detail.contains(detail));
}

fn run_applied_with_stdout(config: &Path, stdout: impl Into<Stdio>) -> Output {
    Command::new(binary())
        .args([
            "--config",
            config.to_str().expect("UTF-8 live config path"),
            "clean",
            "--apply",
        ])
        .stdout(stdout)
        .stderr(Stdio::piped())
        .output()
        .expect("run live clean")
}

#[tokio::test]
async fn live_revalidation_rejects_new_references_and_changed_protection() {
    if !live_test_enabled() {
        return;
    }

    let docker = Docker::connect_with_defaults().expect("connect to live Docker");
    let root = temp_dir();
    let unique = root.file_name().expect("temporary name").to_string_lossy();

    let referenced_network = format!("{unique}-referenced");
    let reference_container = format!("{unique}-keeper");
    let referenced_label = format!("{unique}-reference");
    let referenced_config = root.join("referenced.toml");
    create_network(&docker, &referenced_network, &referenced_label).await;
    fs::write(&referenced_config, network_config(&referenced_label, None))
        .expect("write referenced config");
    let (initial_config, initial_source, plan) = capture_plan(&referenced_config).await;
    assert_network_target(&plan, &referenced_network);
    let keeper_id =
        create_network_reference(&docker, &reference_container, &referenced_network).await;
    let referenced_report =
        execute_plan(&referenced_config, &initial_config, &initial_source, &plan)
            .await
            .expect("execute referenced plan");
    let referenced_exists = docker
        .inspect_network(&referenced_network, None)
        .await
        .is_ok();
    remove_container(&docker, &keeper_id).await;
    let _ = docker.remove_network(&referenced_network).await;

    let protected_network = format!("{unique}-protected");
    let protected_label = format!("{unique}-protection");
    let protected_config = root.join("protected.toml");
    create_network(&docker, &protected_network, &protected_label).await;
    fs::write(&protected_config, network_config(&protected_label, None))
        .expect("write initial protection config");
    let (initial_config, initial_source, plan) = capture_plan(&protected_config).await;
    assert_network_target(&plan, &protected_network);
    fs::write(
        &protected_config,
        network_config(&protected_label, Some(&protected_network)),
    )
    .expect("change protection after plan capture");
    let protected_report = execute_plan(&protected_config, &initial_config, &initial_source, &plan)
        .await
        .expect("execute stale plan");
    let protected_exists = docker
        .inspect_network(&protected_network, None)
        .await
        .is_ok();
    let _ = docker.remove_network(&protected_network).await;
    fs::remove_dir_all(root).expect("remove live test directory");

    assert!(referenced_exists, "newly referenced network was deleted");
    assert_one_skip(&referenced_report, "became ineligible");
    assert!(protected_exists, "newly protected network was deleted");
    assert_one_skip(&protected_report, "configuration changed");
}

#[tokio::test]
async fn live_cleanup_keeps_output_failures_deterministic() {
    if !live_test_enabled() {
        return;
    }

    let docker = Docker::connect_with_defaults().expect("connect to live Docker");
    let root = temp_dir();
    let unique = root.file_name().expect("temporary name").to_string_lossy();

    let epipe_network = format!("{unique}-epipe");
    let epipe_label = format!("{unique}-epipe");
    let epipe_config = root.join("epipe.toml");
    create_network(&docker, &epipe_network, &epipe_label).await;
    fs::write(&epipe_config, network_config(&epipe_label, None)).expect("write EPIPE config");
    let (reader, writer) = os_pipe::pipe().expect("create closed-reader pipe");
    drop(reader);
    let epipe_output = run_applied_with_stdout(&epipe_config, Stdio::from(writer));
    let epipe_removed = docker.inspect_network(&epipe_network, None).await.is_err();
    let _ = docker.remove_network(&epipe_network).await;

    let mut full_result = None;
    if Path::new("/dev/full").exists() {
        let full_network = format!("{unique}-full");
        let full_label = format!("{unique}-full");
        let full_config = root.join("full.toml");
        create_network(&docker, &full_network, &full_label).await;
        fs::write(&full_config, network_config(&full_label, None))
            .expect("write full-device config");
        let full = File::options()
            .write(true)
            .open("/dev/full")
            .expect("open /dev/full");
        let output = run_applied_with_stdout(&full_config, Stdio::from(full));
        let removed = docker.inspect_network(&full_network, None).await.is_err();
        let _ = docker.remove_network(&full_network).await;
        full_result = Some((output, removed));
    }
    fs::remove_dir_all(root).expect("remove live test directory");

    assert_eq!(epipe_output.status.code(), Some(0));
    assert!(epipe_output.stderr.is_empty());
    assert!(epipe_removed, "EPIPE target was not removed");
    if let Some((output, removed)) = full_result {
        assert_eq!(output.status.code(), Some(7));
        assert!(String::from_utf8_lossy(&output.stderr).contains("cannot write stdout"));
        assert!(removed, "/dev/full target was not removed");
    }
}
