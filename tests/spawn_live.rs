use bollard::query_parameters::{
    CreateImageOptionsBuilder, InspectContainerOptions, RemoveContainerOptionsBuilder,
};
use bollard::Docker;
use futures_util::TryStreamExt;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT_ROOT_ID: AtomicU64 = AtomicU64::new(0);

/// A small image that exists in the suite already, pulled if the daemon is new.
const REFERENCE_IMAGE: &str = "busybox:latest";

fn live_test_enabled() -> bool {
    std::env::var_os("DOCKER_MAID_LIVE_TEST").is_some()
}

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_docker_maid")
}

fn temp_dir(label: &str) -> PathBuf {
    let id = NEXT_ROOT_ID.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "docker-maid-spawn-{label}-{}-{id}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("create test directory");
    path
}

fn docker() -> Docker {
    Docker::connect_with_defaults().expect("connect to the live daemon")
}

/// Make sure the reference image exists before a sandbox asks for it.
///
/// `spawn` never pulls, by design, so a fresh daemon would fail every test
/// here for the wrong reason. Pulling in the fixture keeps the suite honest
/// about what it is measuring.
async fn ensure_reference_image(docker: &Docker) {
    if docker.inspect_image(REFERENCE_IMAGE).await.is_ok() {
        return;
    }
    let options = CreateImageOptionsBuilder::default()
        .from_image(REFERENCE_IMAGE)
        .build();
    docker
        .create_image(Some(options), None, None)
        .try_collect::<Vec<_>>()
        .await
        .expect("pull the live reference image");
}

/// Remove a fixture container whether or not it exists or is running.
async fn remove_container(docker: &Docker, name: &str) {
    let options = RemoveContainerOptionsBuilder::default()
        .force(true)
        .v(true)
        .build();
    let _ = docker.remove_container(name, Some(options)).await;
}

/// Remove a fixture container even when the test that created it panics.
///
/// Rust offers no test teardown, so a failing assertion would otherwise leave
/// a running container on a shared daemon, and the next test would count it.
/// A cleanup tool whose own suite leaks containers has no business shipping.
///
/// The removal runs on its own thread with its own runtime, because `Drop`
/// cannot await and blocking inside the test's runtime would panic.
struct ContainerGuard {
    name: String,
}

impl ContainerGuard {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
        }
    }
}

impl Drop for ContainerGuard {
    fn drop(&mut self) {
        let name = self.name.clone();
        let _ = std::thread::spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            runtime.block_on(async {
                if let Ok(docker) = Docker::connect_with_defaults() {
                    remove_container(&docker, &name).await;
                }
            });
        })
        .join();
    }
}

/// Remove a fixture directory even when the test that created it panics.
struct DirectoryGuard {
    path: PathBuf,
}

impl DirectoryGuard {
    fn new(label: &str) -> Self {
        Self {
            path: temp_dir(label),
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for DirectoryGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn run(args: &[&str], current_dir: &Path) -> Output {
    Command::new(binary())
        .args(args)
        .current_dir(current_dir)
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("run docker_maid")
}

#[tokio::test]
async fn a_spawned_sandbox_carries_the_stamp_and_is_left_alone() {
    if !live_test_enabled() {
        return;
    }
    let docker = docker();
    ensure_reference_image(&docker).await;
    let directory = DirectoryGuard::new("stamped");
    let root = directory.path();
    let name = format!("dm-spawn-live-stamped-{}", std::process::id());
    remove_container(&docker, &name).await;
    let _guard = ContainerGuard::new(&name);

    let output = run(
        &[
            "--json",
            "spawn",
            "--image",
            REFERENCE_IMAGE,
            "--owner",
            "dm-spawn-live",
            "--name",
            &name,
            "sleep",
            "120",
        ],
        root,
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: Value =
        serde_json::from_slice(&output.stdout).expect("spawn document parses as JSON");
    assert_eq!(document["schema_version"], 1);
    assert_eq!(document["command"], "spawn");
    assert_eq!(document["name"], name.as_str());
    assert_eq!(document["auto_remove"], false);

    let inspected = docker
        .inspect_container(&name, None::<InspectContainerOptions>)
        .await
        .expect("the sandbox exists on the daemon");

    // The labels Docker recorded are exactly the ones the document promised,
    // and exactly the ones `stamp` would have emitted for the same owner.
    let recorded = inspected
        .config
        .as_ref()
        .and_then(|config| config.labels.clone())
        .expect("a spawned sandbox is labelled");
    let promised = document["labels"].as_object().expect("labels object");
    assert_eq!(recorded.len(), promised.len());
    for (key, value) in promised {
        assert_eq!(recorded.get(key).map(String::as_str), value.as_str());
    }
    let emitted: Value =
        serde_json::from_slice(&run(&["--json", "stamp", "--owner", "dm-spawn-live"], root).stdout)
            .expect("stamp document");
    assert_eq!(
        document["labels"], emitted["labels"],
        "spawn must apply the same stamp the stamp command emits"
    );

    // Nothing was attached and nothing will remove it for us.
    let host_config = inspected.host_config.expect("host config");
    assert_eq!(host_config.auto_remove, Some(false));
    let config = inspected.config.expect("config");
    assert_eq!(config.tty, Some(false));
    assert_eq!(config.open_stdin, Some(false));
    assert_eq!(config.attach_stdout, Some(false));

    // The command has already exited, and the sandbox is still running.
    let state = inspected.state.expect("state");
    assert_eq!(state.running, Some(true), "the sandbox must outlive spawn");
}

#[tokio::test]
async fn spawn_returns_without_waiting_for_the_sandbox() {
    if !live_test_enabled() {
        return;
    }
    let docker = docker();
    ensure_reference_image(&docker).await;
    let directory = DirectoryGuard::new("nonparenting");
    let root = directory.path();
    let name = format!("dm-spawn-live-nonparenting-{}", std::process::id());
    remove_container(&docker, &name).await;
    let _guard = ContainerGuard::new(&name);

    // The sandbox sleeps far longer than the command may take. If spawn
    // supervised it in any way, this call could not return first.
    let started = Instant::now();
    let output = run(
        &[
            "spawn",
            "--image",
            REFERENCE_IMAGE,
            "--name",
            &name,
            "sleep",
            "180",
        ],
        root,
    );
    let elapsed = started.elapsed();
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        elapsed < Duration::from_secs(30),
        "spawn took {elapsed:?}, which suggests it waited for the sandbox"
    );

    // The command process is gone and the sandbox is still running.
    let state = docker
        .inspect_container(&name, None::<InspectContainerOptions>)
        .await
        .expect("the sandbox exists")
        .state
        .expect("state");
    assert_eq!(
        state.running,
        Some(true),
        "the sandbox died with the command that created it"
    );
}

#[tokio::test]
async fn a_finished_sandbox_is_still_there_to_be_inventoried() {
    if !live_test_enabled() {
        return;
    }
    let docker = docker();
    ensure_reference_image(&docker).await;
    let directory = DirectoryGuard::new("workspace");
    let root = directory.path();
    let workspace = root.join("work");
    fs::create_dir_all(&workspace).expect("create the workspace directory");
    let name = format!("dm-spawn-live-workspace-{}", std::process::id());
    remove_container(&docker, &name).await;
    let _guard = ContainerGuard::new(&name);

    let output = run(
        &[
            "spawn",
            "--image",
            REFERENCE_IMAGE,
            "--name",
            &name,
            "--workspace",
            workspace.to_str().expect("UTF-8 workspace path"),
            "true",
        ],
        root,
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Wait for the sandbox to finish its one-line command.
    let mut finished = false;
    for _ in 0..100 {
        let state = docker
            .inspect_container(&name, None::<InspectContainerOptions>)
            .await
            .expect("the sandbox exists")
            .state
            .expect("state");
        if state.running == Some(false) {
            finished = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(finished, "the sandbox never finished its command");

    // AutoRemove is off, so an exited sandbox is still inventoriable. This is
    // what lets a rule adopt and later reclaim it.
    let inspected = docker
        .inspect_container(&name, None::<InspectContainerOptions>)
        .await
        .expect("an exited sandbox is still on the daemon");
    assert_eq!(
        inspected
            .host_config
            .as_ref()
            .expect("host config")
            .auto_remove,
        Some(false)
    );

    // Docker recorded the bind we asked for, at the documented destination,
    // and the sandbox started there. This asserts the request rather than the
    // contents of the host directory on purpose: a VM-backed daemon such as
    // colima only shares some host paths, so reading the file back would test
    // the developer's mount configuration instead of this command.
    let mounts = inspected
        .mounts
        .expect("a bound sandbox records its mounts");
    assert_eq!(mounts.len(), 1, "expected exactly one bind: {mounts:?}");
    assert_eq!(mounts[0].source.as_deref(), workspace.to_str());
    assert_eq!(mounts[0].destination.as_deref(), Some("/workspace"));
    assert_eq!(
        inspected
            .config
            .and_then(|config| config.working_dir)
            .as_deref(),
        Some("/workspace")
    );
}

#[tokio::test]
async fn a_spawned_sandbox_is_offered_for_adoption_by_the_survey() {
    if !live_test_enabled() {
        return;
    }
    let docker = docker();
    ensure_reference_image(&docker).await;
    let directory = DirectoryGuard::new("survey");
    let root = directory.path();
    let owner = format!("dm-spawn-live-survey-{}", std::process::id());
    let name = format!("dm-spawn-live-survey-{}", std::process::id());
    remove_container(&docker, &name).await;
    let _guard = ContainerGuard::new(&name);

    let output = run(
        &[
            "spawn",
            "--image",
            REFERENCE_IMAGE,
            "--owner",
            &owner,
            "--name",
            &name,
            "sleep",
            "120",
        ],
        root,
    );
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // The whole point of stamping at creation: no configuration was written,
    // yet the survey already sees a family to adopt.
    let config = root.join("docker_maid.toml");
    fs::write(&config, run(&["config", "default"], root).stdout)
        .expect("write the default configuration");
    let survey: Value = serde_json::from_slice(
        &run(
            &[
                "--config",
                config.to_str().expect("UTF-8 config path"),
                "--json",
                "config",
                "survey",
            ],
            root,
        )
        .stdout,
    )
    .expect("survey document");
    let mut found = false;
    for candidate in survey["candidates"].as_array().expect("candidates array") {
        let selector = &candidate["selector"];
        if selector["key"] == "ai-agent.owner" && selector["value"] == owner.as_str() {
            let members = candidate["resources"].as_array().expect("resources array");
            assert!(
                members.iter().any(|member| member["name"] == name.as_str()),
                "the family does not contain the sandbox"
            );
            found = true;
        }
    }
    assert!(found, "the survey did not offer the spawned sandbox");
}
