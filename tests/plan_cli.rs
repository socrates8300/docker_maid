use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_docker_maid")
}

fn temp_dir(label: &str) -> PathBuf {
    let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "docker-maid-plan-{label}-{}-{id}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("create test directory");
    path
}

fn run_plan(root: &Path, config: &Path) -> Output {
    Command::new(binary())
        .args([
            "--config",
            config.to_str().expect("UTF-8 config path"),
            "plan",
        ])
        .current_dir(root)
        .env("DOCKER_HOST", "tcp://127.0.0.1:1")
        .output()
        .expect("run plan")
}

#[test]
fn unreachable_docker_exits_five_without_payload() {
    let root = temp_dir("docker-unreachable");
    let config = root.join("config.toml");
    fs::write(
        &config,
        "[[rules.networks]]\nname='agents'\nselect.names=['^agent-']\norphan=true\n",
    )
    .expect("write config");

    let output = run_plan(&root, &config);
    assert_eq!(output.status.code(), Some(5));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Docker"));

    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn invalid_selector_exits_three_before_docker_inventory() {
    let root = temp_dir("bad-selector");
    let config = root.join("config.toml");
    fs::write(
        &config,
        "[[rules.networks]]\nname='agents'\nselect.names=['(']\norphan=true\n",
    )
    .expect("write config");

    let output = run_plan(&root, &config);
    assert_eq!(output.status.code(), Some(3));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("select.names[0]"));

    fs::remove_dir_all(root).expect("remove test directory");
}
