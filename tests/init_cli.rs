use serde_json::Value;
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
        "docker-maid-init-{label}-{}-{id}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("create test directory");
    path
}

/// Remove a fixture directory even when the test that created it panics.
///
/// Rust offers no test teardown, so a failing assertion would otherwise leave
/// directories behind on every run that caught a real defect.
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

/// Run the binary with Docker pointed at a closed port and HOME redirected.
///
/// Installing a skill is a file write, so it must answer without a daemon.
/// `HOME` is redirected because a test that installs into the developer's real
/// skills directory would be a defect of its own.
fn run(args: &[&str], current_dir: &Path, home: &Path) -> Output {
    Command::new(binary())
        .args(args)
        .current_dir(current_dir)
        .env("DOCKER_HOST", "tcp://127.0.0.1:1")
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("run docker_maid")
}

#[test]
fn installing_writes_the_skill_under_the_chosen_harness() {
    let directory = DirectoryGuard::new("harness");
    let root = directory.path();
    let home = root.join("home");
    fs::create_dir_all(&home).expect("create the fake home");
    let output = run(&["init", "--agents", "--target", "claude"], root, &home);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let installed = home.join(".claude/skills/docker-maid/SKILL.md");
    let document = fs::read_to_string(&installed).expect("the skill was installed");
    assert!(
        document.starts_with("---\n"),
        "the skill needs front matter"
    );
    assert!(document.contains("name: docker-maid"));
}

#[test]
fn installing_never_reads_or_writes_the_configuration_file() {
    // Policy is human-owned. This command must not open the configuration, let
    // alone edit it, so an unparseable one must not even be noticed.
    let directory = DirectoryGuard::new("config");
    let root = directory.path();
    let home = root.join("home");
    fs::create_dir_all(&home).expect("create the fake home");
    let config = root.join("docker_maid.toml");
    let original = "this is not = valid toml [[[";
    fs::write(&config, original).expect("write a broken local configuration");

    let output = run(&["init", "--agents", "--target", "codex"], root, &home);
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    assert_eq!(
        fs::read_to_string(&config).expect("read the configuration back"),
        original,
        "the configuration file was modified"
    );
    assert!(home.join(".codex/skills/docker-maid/SKILL.md").exists());
}

#[test]
fn the_machine_document_reports_where_the_skill_went() {
    let directory = DirectoryGuard::new("json");
    let root = directory.path();
    let home = root.join("home");
    let dest = root.join("skills");
    fs::create_dir_all(&home).expect("create the fake home");
    let output = run(
        &[
            "--json",
            "init",
            "--agents",
            "--target",
            "generic",
            "--dest",
            dest.to_str().expect("UTF-8 destination"),
        ],
        root,
        &home,
    );
    assert_eq!(output.status.code(), Some(0));
    let document: Value =
        serde_json::from_slice(&output.stdout).expect("init document parses as JSON");
    assert_eq!(document["schema_version"], 1);
    assert_eq!(document["command"], "init");
    assert_eq!(document["mode"], "agents");
    assert_eq!(document["target"], "generic");
    assert_eq!(document["status"], "written");
    let path = document["path"].as_str().expect("path string");
    assert_eq!(Path::new(path), dest.join("docker-maid/SKILL.md"));
    assert!(Path::new(path).exists());
}

#[test]
fn a_rerun_reports_unchanged_and_needs_no_force() {
    // Reinstalling is the normal case after an upgrade or a fresh checkout.
    let directory = DirectoryGuard::new("rerun");
    let root = directory.path();
    let home = root.join("home");
    fs::create_dir_all(&home).expect("create the fake home");
    let arguments = ["--json", "init", "--agents", "--target", "claude"];
    assert_eq!(run(&arguments, root, &home).status.code(), Some(0));
    let second = run(&arguments, root, &home);
    assert_eq!(second.status.code(), Some(0));
    let document: Value = serde_json::from_slice(&second.stdout).expect("init document");
    assert_eq!(document["status"], "unchanged");
}

#[test]
fn an_edited_skill_is_kept_until_force_says_otherwise() {
    // Someone may have adapted the installed skill. Silently overwriting it
    // would discard that with nothing to notice.
    let directory = DirectoryGuard::new("force");
    let root = directory.path();
    let home = root.join("home");
    fs::create_dir_all(&home).expect("create the fake home");
    let arguments = ["init", "--agents", "--target", "claude"];
    assert_eq!(run(&arguments, root, &home).status.code(), Some(0));
    let installed = home.join(".claude/skills/docker-maid/SKILL.md");
    fs::write(&installed, "---\nname: mine\n---\nlocal edit\n").expect("edit the skill");

    let refused = run(&arguments, root, &home);
    assert_eq!(refused.status.code(), Some(64));
    assert!(refused.stdout.is_empty());
    assert!(fs::read_to_string(&installed)
        .expect("read back")
        .contains("local edit"));

    let forced = run(
        &[
            "--json", "init", "--agents", "--target", "claude", "--force",
        ],
        root,
        &home,
    );
    assert_eq!(forced.status.code(), Some(0));
    let document: Value = serde_json::from_slice(&forced.stdout).expect("init document");
    assert_eq!(document["status"], "replaced");
}

#[test]
fn the_command_refuses_to_guess_what_to_install_or_where() {
    let directory = DirectoryGuard::new("refusals");
    let root = directory.path();
    let home = root.join("home");
    fs::create_dir_all(&home).expect("create the fake home");
    for arguments in [
        // No mode: --agents is the only one, but it still has to be asked for.
        vec!["--json", "init"],
        // No target: writing into a home directory nobody named is worse than
        // an error.
        vec!["--json", "init", "--agents"],
        // Generic with no destination is the case where guessing is wrong.
        vec!["--json", "init", "--agents", "--target", "generic"],
    ] {
        let output = run(&arguments, root, &home);
        assert_eq!(
            output.status.code(),
            Some(64),
            "{arguments:?} should be a usage error"
        );
        assert!(output.stdout.is_empty(), "{arguments:?} wrote stdout");
        let document: Value =
            serde_json::from_slice(&output.stderr).expect("error document parses as JSON");
        assert_eq!(document["error"]["kind"], "usage");
    }
    // Nothing was created by any of those refusals.
    assert!(!home.join(".claude").exists());
    assert!(!home.join(".codex").exists());
}

#[test]
fn the_installed_skill_matches_the_command_surface_of_this_build() {
    // The skill is only worth installing if every command it teaches exists.
    // Ask the binary itself rather than trusting a list written by hand.
    let directory = DirectoryGuard::new("surface");
    let root = directory.path();
    let home = root.join("home");
    let dest = root.join("skills");
    fs::create_dir_all(&home).expect("create the fake home");
    assert_eq!(
        run(
            &[
                "init",
                "--agents",
                "--target",
                "generic",
                "--dest",
                dest.to_str().expect("UTF-8 destination"),
            ],
            root,
            &home,
        )
        .status
        .code(),
        Some(0)
    );
    let document =
        fs::read_to_string(dest.join("docker-maid/SKILL.md")).expect("read the installed skill");

    let help = String::from_utf8(run(&["--help"], root, &home).stdout).expect("UTF-8 help");
    let commands = help
        .lines()
        .skip_while(|line| !line.starts_with("Commands:"))
        .skip(1)
        .take_while(|line| line.starts_with("  "))
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert!(
        commands.len() > 5,
        "could not read the command list: {help}"
    );

    let mut inside_code = false;
    for line in document.lines() {
        if line.trim_start().starts_with("```") {
            inside_code = !inside_code;
            continue;
        }
        if !inside_code {
            continue;
        }
        for tail in line.split("docker_maid ").skip(1) {
            let Some(word) = tail.split_whitespace().find(|word| !word.starts_with("--")) else {
                continue;
            };
            let word = word.trim_end_matches(')');
            assert!(
                commands.iter().any(|command| command == word),
                "the skill teaches `docker_maid {word}`, which this build does not offer"
            );
        }
    }
}
