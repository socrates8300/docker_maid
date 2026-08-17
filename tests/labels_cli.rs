use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_docker_maid")
}

fn temp_dir(label: &str) -> PathBuf {
    let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "docker-maid-labels-{label}-{}-{id}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("create test directory");
    path
}

/// Run the binary with Docker pointed at a closed port.
///
/// The vocabulary is a build-time constant, so this command must answer
/// without a daemon. Pointing `DOCKER_HOST` at a dead port proves that rather
/// than relying on whatever daemon the host happens to run.
fn run(args: &[&str], current_dir: &PathBuf) -> Output {
    Command::new(binary())
        .args(args)
        .current_dir(current_dir)
        .env("DOCKER_HOST", "tcp://127.0.0.1:1")
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("run docker_maid")
}

fn advertised_keys(document: &Value) -> Vec<(String, String)> {
    document["keys"]
        .as_array()
        .expect("keys array")
        .iter()
        .map(|entry| {
            (
                entry["key"].as_str().expect("key string").to_owned(),
                entry["match"].as_str().expect("match string").to_owned(),
            )
        })
        .collect()
}

#[test]
fn labels_answers_without_a_docker_daemon() {
    let root = temp_dir("no-daemon");
    let output = run(&["labels"], &root);
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    let text = String::from_utf8(output.stdout).expect("UTF-8 label table");
    assert!(text.contains("com.docker.compose.project"));
    assert!(text.contains("ai-agent."));
    assert!(text.contains("devcontainer."));
    assert!(text.contains("dev.docker-maid.managed"));
    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn the_machine_document_is_a_versioned_labels_document() {
    let root = temp_dir("json");
    let output = run(&["--json", "labels"], &root);
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    let document: Value =
        serde_json::from_slice(&output.stdout).expect("labels document parses as JSON");
    assert_eq!(document["schema_version"], 1);
    assert_eq!(document["command"], "labels");
    let keys = advertised_keys(&document);
    assert!(!keys.is_empty(), "the vocabulary must not be empty");
    for (key, matching) in &keys {
        assert!(!key.is_empty(), "an advertised key must not be blank");
        assert!(
            matching == "exact" || matching == "prefix",
            "{key} has an unknown match kind {matching}"
        );
        if matching == "prefix" {
            assert!(
                key.ends_with('.'),
                "prefix key {key} must end at a namespace boundary"
            );
        }
    }
    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn the_human_and_machine_surfaces_advertise_the_same_keys() {
    // Two renderers read one table. If a future change teaches one of them a
    // key the other does not know, an operator reading the table and a tool
    // reading the document would disagree about what is owned.
    let root = temp_dir("parity");
    let table = String::from_utf8(run(&["labels"], &root).stdout).expect("UTF-8 label table");
    let document: Value =
        serde_json::from_slice(&run(&["--json", "labels"], &root).stdout).expect("labels document");
    for (key, matching) in advertised_keys(&document) {
        // The table writes a prefix the way an operator would type it in a
        // selector, with a trailing star.
        let rendered = if matching == "prefix" {
            format!("{key}*")
        } else {
            key.clone()
        };
        assert!(
            table.contains(&rendered),
            "the machine document advertises {key} but the table does not show {rendered}"
        );
    }
    // Count the other direction too, so a key shown only in the table is
    // caught as well. A key row is an unindented line whose first field is one
    // of the rendered keys, which does not depend on the surrounding prose.
    let rendered = advertised_keys(&document)
        .into_iter()
        .map(|(key, matching)| {
            if matching == "prefix" {
                format!("{key}*")
            } else {
                key
            }
        })
        .collect::<Vec<_>>();
    let table_rows = table
        .lines()
        .filter(|line| !line.starts_with(char::is_whitespace))
        .filter_map(|line| line.split_whitespace().next())
        .filter(|first| rendered.iter().any(|key| key == first))
        .count();
    assert_eq!(
        table_rows,
        rendered.len(),
        "the table and the machine document disagree about how many keys exist"
    );
    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn a_broken_pipe_on_the_label_table_is_not_a_failure() {
    // Every other one-shot command survives `| head`, so this one must too.
    let root = temp_dir("pipe");
    let (reader, writer) = os_pipe::pipe().expect("create stdout pipe");
    drop(reader);
    let output = Command::new(binary())
        .args(["labels"])
        .current_dir(&root)
        .env("DOCKER_HOST", "tcp://127.0.0.1:1")
        .stdout(writer)
        .output()
        .expect("run docker_maid with a closed stdout");
    assert_eq!(output.status.code(), Some(0));
    fs::remove_dir_all(root).expect("remove test directory");
}
