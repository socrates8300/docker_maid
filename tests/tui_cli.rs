use std::process::Command;

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_docker_maid")
}

#[test]
fn tui_refuses_non_terminal_streams_immediately_with_exit_four() {
    let output = Command::new(binary())
        .arg("tui")
        .env("DOCKER_HOST", "tcp://127.0.0.1:1")
        .output()
        .expect("run tui without a terminal");

    assert_eq!(output.status.code(), Some(4));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert_eq!(
        stderr,
        "error: tui requires both stdin and stdout to be terminals; use status --format json for headless use\n"
    );
    assert!(!stderr.contains("configuration"));
    assert!(!stderr.contains("Docker"));
}

#[test]
fn tui_rejects_machine_formats_and_unimplemented_attach_mode() {
    for arguments in [
        &["--json", "tui"][..],
        &["tui", "--format", "table"][..],
        &["tui", "--attach"][..],
    ] {
        let output = Command::new(binary())
            .args(arguments)
            .output()
            .expect("run rejected tui invocation");
        assert_eq!(output.status.code(), Some(64), "arguments={arguments:?}");
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&output.stderr).starts_with("error: "),
            "arguments={arguments:?}, stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn help_exposes_the_interactive_entrypoint() {
    let output = Command::new(binary())
        .arg("--help")
        .output()
        .expect("run help");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 help");
    assert!(stdout.contains("tui"));
    assert!(stdout.contains("Open the interactive terminal dashboard"));
}
