use serde_json::Value;
use std::collections::BTreeMap;
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
        "docker-maid-stamp-{label}-{}-{id}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("create test directory");
    path
}

/// Run the binary with Docker pointed at a closed port.
///
/// The stamp is a build-time constant plus one argument, so this command must
/// answer without a daemon. Pointing `DOCKER_HOST` at a dead port proves that
/// rather than relying on whatever daemon the host happens to run.
fn run(args: &[&str], current_dir: &Path) -> Output {
    Command::new(binary())
        .args(args)
        .current_dir(current_dir)
        .env("DOCKER_HOST", "tcp://127.0.0.1:1")
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("run docker_maid")
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("UTF-8 stdout")
}

/// The `key=value` pairs the machine document says to apply.
fn document_labels(document: &Value) -> BTreeMap<String, String> {
    document["labels"]
        .as_object()
        .expect("labels object")
        .iter()
        .map(|(key, value)| {
            (
                key.clone(),
                value.as_str().expect("label value string").to_owned(),
            )
        })
        .collect()
}

#[test]
fn stamp_answers_without_a_docker_daemon_or_configuration() {
    // A broken local configuration file is the strongest available proof that
    // the command does not read one: any load attempt would exit 3 here.
    let root = temp_dir("no-daemon");
    fs::write(
        root.join("docker_maid.toml"),
        "this is not = valid toml [[[",
    )
    .expect("write a broken local configuration");
    let output = run(&["stamp"], &root);
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    assert!(stdout_of(&output).contains("dev.docker-maid.managed=true"));
    // Nothing was created beside the file the test wrote itself.
    let entries = fs::read_dir(&root)
        .expect("read test directory")
        .map(|entry| entry.expect("directory entry").file_name())
        .collect::<Vec<_>>();
    assert_eq!(entries.len(), 1, "stamp must not create files: {entries:?}");
    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn the_machine_document_is_a_versioned_stamp_document() {
    let root = temp_dir("json");
    let output = run(&["--json", "stamp", "--owner", "agent-7"], &root);
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    let document: Value =
        serde_json::from_slice(&output.stdout).expect("stamp document parses as JSON");
    assert_eq!(document["schema_version"], 1);
    assert_eq!(document["command"], "stamp");
    let labels = document_labels(&document);
    assert_eq!(
        labels.get("dev.docker-maid.managed").map(String::as_str),
        Some("true")
    );
    assert_eq!(
        labels.get("ai-agent.owner").map(String::as_str),
        Some("agent-7")
    );
    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn every_advertised_stamp_key_is_a_key_the_labels_command_advertises() {
    // The stamp is only useful if the survey recognises what it writes, and
    // `labels` is what the survey reads. A stamp key missing from that list
    // would label a resource with something nothing acts on.
    let root = temp_dir("vocabulary");
    let stamp: Value =
        serde_json::from_slice(&run(&["--json", "stamp", "--owner", "a"], &root).stdout)
            .expect("stamp document");
    let vocabulary: Value =
        serde_json::from_slice(&run(&["--json", "labels"], &root).stdout).expect("labels document");
    let advertised = vocabulary["keys"].as_array().expect("keys array");
    for key in document_labels(&stamp).keys() {
        let matched = advertised.iter().any(|entry| {
            let advertised_key = entry["key"].as_str().expect("key string");
            match entry["match"].as_str().expect("match string") {
                "exact" => key == advertised_key,
                "prefix" => key.starts_with(advertised_key),
                other => panic!("unknown match kind {other}"),
            }
        });
        assert!(
            matched,
            "stamp writes {key}, which `labels` does not advertise"
        );
    }
    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn the_three_surfaces_describe_one_stamp() {
    // A human reading the table, a tool reading the document, and a script
    // interpolating the flag line must all apply the same labels.
    let root = temp_dir("parity");
    let arguments = ["stamp", "--owner", "agent-7"];
    let table = stdout_of(&run(&arguments, &root));
    let document: Value =
        serde_json::from_slice(&run(&["--json", "stamp", "--owner", "agent-7"], &root).stdout)
            .expect("stamp document");
    let line = stdout_of(&run(
        &["stamp", "--owner", "agent-7", "--docker-args"],
        &root,
    ));

    let labels = document_labels(&document);
    assert!(!labels.is_empty(), "the stamp must not be empty");
    let mut from_line = BTreeMap::new();
    let fields = line.split_whitespace().collect::<Vec<_>>();
    assert_eq!(fields.len(), labels.len() * 2, "flag line: {line:?}");
    for pair in fields.chunks(2) {
        assert_eq!(pair[0], "--label");
        let (key, value) = pair[1].split_once('=').expect("flag carries key=value");
        from_line.insert(key.to_owned(), value.to_owned());
    }
    assert_eq!(from_line, labels, "the flag line and the document disagree");
    for (key, value) in &labels {
        assert!(
            table.contains(&format!("{key}={value}")),
            "the table does not show {key}={value}"
        );
    }
    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn the_flag_line_reaches_docker_as_separate_arguments() {
    // The line exists to be used as `$(docker_maid stamp --docker-args)`, so
    // let a real shell expand it and count what a Docker command would see.
    let root = temp_dir("shell");
    let script = format!(
        "set -- $(\"{}\" stamp --owner agent.7_b-c --docker-args); printf '%s\\n' \"$#\" \"$@\"",
        binary()
    );
    let output = Command::new("/bin/sh")
        .args(["-c", &script])
        .current_dir(&root)
        .env("DOCKER_HOST", "tcp://127.0.0.1:1")
        .output()
        .expect("expand the flag line in a shell");
    assert_eq!(output.status.code(), Some(0));
    let lines = stdout_of(&output)
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert_eq!(lines[0], "4", "expected four arguments, got {lines:?}");
    assert_eq!(lines[1], "--label");
    assert_eq!(lines[2], "ai-agent.owner=agent.7_b-c");
    assert_eq!(lines[3], "--label");
    assert_eq!(lines[4], "dev.docker-maid.managed=true");
    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn an_owner_the_stamp_will_not_write_is_a_usage_error() {
    // A name holding a space would split into two Docker arguments once the
    // flag line is expanded, so it is refused rather than quoted.
    let root = temp_dir("bad-owner");
    for owner in ["", "   ", "two words", "a$b", "a;b"] {
        let output = run(&["--json", "stamp", "--owner", owner], &root);
        assert_eq!(
            output.status.code(),
            Some(64),
            "owner {owner:?} must be a usage error"
        );
        assert!(output.stdout.is_empty(), "a refused stamp writes no stdout");
        let document: Value =
            serde_json::from_slice(&output.stderr).expect("error document parses as JSON");
        assert_eq!(document["schema_version"], 1);
        assert_eq!(document["error"]["kind"], "usage");
    }
    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn asking_for_two_output_shapes_at_once_is_refused() {
    // `--docker-args` is a third shape. Silently letting it win would make the
    // answer depend on argument order for a caller that piped into a parser.
    let root = temp_dir("conflict");
    for arguments in [
        vec!["--json", "stamp", "--docker-args"],
        vec!["stamp", "--json", "--docker-args"],
        vec!["--format", "json", "stamp", "--docker-args"],
        vec!["--format", "table", "stamp", "--docker-args"],
    ] {
        let output = run(&arguments, &root);
        assert_eq!(
            output.status.code(),
            Some(64),
            "{arguments:?} must be a usage error"
        );
        assert!(
            output.stdout.is_empty(),
            "{arguments:?} wrote stdout anyway"
        );
    }
    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn an_explicit_configuration_path_is_ignored_rather_than_loaded() {
    // `--config` is global, so it reaches this command. Stamping needs no
    // policy, and failing on a path it never reads would be a surprise.
    let root = temp_dir("explicit-config");
    let output = run(
        &["stamp", "--config", "/nonexistent/docker_maid.toml"],
        &root,
    );
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn a_broken_pipe_on_the_stamp_is_not_a_failure() {
    // Every other one-shot command survives `| head`, so this one must too.
    let root = temp_dir("pipe");
    let (reader, writer) = os_pipe::pipe().expect("create stdout pipe");
    drop(reader);
    let output = Command::new(binary())
        .args(["stamp"])
        .current_dir(&root)
        .env("DOCKER_HOST", "tcp://127.0.0.1:1")
        .stdout(writer)
        .output()
        .expect("run docker_maid with a closed stdout");
    assert_eq!(output.status.code(), Some(0));
    fs::remove_dir_all(root).expect("remove test directory");
}
