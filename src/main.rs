use clap::{error::ErrorKind, Parser, Subcommand};
use docker_maid::config::{load_config, DEFAULT_CONFIG};
use docker_maid::executor::execute_plan;
use docker_maid::inventory::{collect_inventory, needs_container_state};
use docker_maid::plan::{build_plan, Action, Disposition};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

const EXIT_PENDING: u8 = 1;
const EXIT_PARTIAL: u8 = 2;
const EXIT_CONFIG: u8 = 3;
const EXIT_DOCKER: u8 = 5;
const EXIT_INTERNAL: u8 = 7;
const EXIT_USAGE: u8 = 64;

#[derive(Debug, Parser)]
#[command(name = "docker_maid", version, about)]
struct Cli {
    /// Read configuration from this exact path.
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print the policy-derived dry-run plan without changing Docker.
    Plan,
    /// Run one policy-derived cleanup pass; mutation requires --apply.
    Clean {
        /// Apply the generated plan without prompting.
        #[arg(long)]
        apply: bool,
    },
    /// Generate, validate, or normalize configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Validate the selected configuration file.
    Check,
    /// Parse and print the selected configuration in normalized TOML.
    Print,
    /// Print an annotated default configuration.
    Default,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let successful = matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            );
            let _ = error.print();
            return ExitCode::from(if successful { 0 } else { EXIT_USAGE });
        }
    };

    match run(cli).await {
        Ok(RunOutcome::Success) => ExitCode::SUCCESS,
        Ok(RunOutcome::PendingRemovals) => ExitCode::from(EXIT_PENDING),
        Ok(RunOutcome::PartialFailure) => ExitCode::from(EXIT_PARTIAL),
        Err(RunError::Output(error)) => {
            let code = output_error_exit_code(&error);
            if code != 0 {
                write_diagnostic(&format!("error: cannot write stdout: {error}"));
            }
            ExitCode::from(code)
        }
        Err(RunError::Config(error)) => {
            write_diagnostic(&format!("error: {error}"));
            ExitCode::from(EXIT_CONFIG)
        }
        Err(RunError::Docker(error)) => {
            write_diagnostic(&format!("error: {error}"));
            ExitCode::from(EXIT_DOCKER)
        }
        Err(RunError::Execution(error)) => {
            write_diagnostic(&format!("error: {error}"));
            ExitCode::from(EXIT_DOCKER)
        }
        Err(RunError::Internal(error)) => {
            write_diagnostic(&format!("error: {error}"));
            ExitCode::from(EXIT_INTERNAL)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunOutcome {
    Success,
    PendingRemovals,
    PartialFailure,
}

#[derive(Debug)]
enum RunError {
    Config(docker_maid::config::ConfigError),
    Docker(docker_maid::inventory::InventoryError),
    Execution(docker_maid::executor::ExecutionError),
    Internal(String),
    Output(io::Error),
}

impl From<docker_maid::config::ConfigError> for RunError {
    fn from(error: docker_maid::config::ConfigError) -> Self {
        Self::Config(error)
    }
}

impl From<io::Error> for RunError {
    fn from(error: io::Error) -> Self {
        Self::Output(error)
    }
}

impl From<docker_maid::inventory::InventoryError> for RunError {
    fn from(error: docker_maid::inventory::InventoryError) -> Self {
        Self::Docker(error)
    }
}

impl From<docker_maid::executor::ExecutionError> for RunError {
    fn from(error: docker_maid::executor::ExecutionError) -> Self {
        Self::Execution(error)
    }
}

async fn run(cli: Cli) -> Result<RunOutcome, RunError> {
    match cli.command {
        Command::Plan => run_cleanup(cli.config.as_deref(), false, false).await,
        Command::Clean { apply } => run_cleanup(cli.config.as_deref(), true, apply).await,
        Command::Config {
            command: ConfigCommand::Default,
        } => {
            write_payload(DEFAULT_CONFIG.as_bytes())?;
            Ok(RunOutcome::Success)
        }
        Command::Config { command } => {
            let loaded = load_selected_config(cli.config.as_deref())?;

            match command {
                ConfigCommand::Check => {
                    let message = format!("configuration valid: {}\n", display_path(&loaded.path));
                    write_payload(message.as_bytes())?;
                }
                ConfigCommand::Print => {
                    let normalized = loaded.config.to_normalized_toml()?;
                    write_payload(normalized.as_bytes())?;
                }
                ConfigCommand::Default => unreachable!("handled without loading configuration"),
            }
            Ok(RunOutcome::Success)
        }
    }
}

async fn run_cleanup(
    explicit_config: Option<&Path>,
    clean_command: bool,
    apply: bool,
) -> Result<RunOutcome, RunError> {
    let loaded = load_selected_config(explicit_config)?;
    if loaded.config.rules.build_cache.is_some() {
        write_diagnostic(
            "warning: rules.build_cache was not evaluated because build-cache inventory is not implemented",
        );
    }
    let inventory = collect_inventory(needs_container_state(&loaded.config)).await?;
    let plan = build_plan(&loaded.config, inventory, epoch_seconds()?)
        .map_err(|error| RunError::Internal(format!("cannot build plan: {error}")))?;

    if !clean_command || !apply {
        let pending = plan.has_pending_removals();
        write_payload(plan.render_table().as_bytes())?;
        return Ok(if pending {
            RunOutcome::PendingRemovals
        } else {
            RunOutcome::Success
        });
    }

    if !plan.has_pending_removals() {
        write_payload(plan.render_table().as_bytes())?;
        return Ok(RunOutcome::Success);
    }

    let authorized_unscoped = plan
        .decisions
        .iter()
        .filter(|decision| {
            decision.action == Action::Remove
                && decision.disposition == Disposition::AuthorizedUnscoped
        })
        .count();
    if authorized_unscoped != 0 {
        write_diagnostic(&format!(
            "warning: applying {authorized_unscoped} authorized-unscoped removal(s)"
        ));
    }

    let report = execute_plan(&loaded.path, &loaded.config, &loaded.source, &plan).await?;
    let partial = report.has_partial_failure();
    write_payload(report.render_table().as_bytes())?;
    Ok(if partial {
        RunOutcome::PartialFailure
    } else {
        RunOutcome::Success
    })
}

fn epoch_seconds() -> Result<i64, RunError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| RunError::Internal(format!("system clock is before 1970: {error}")))?
        .as_secs()
        .try_into()
        .map_err(|error| RunError::Internal(format!("system clock is out of range: {error}")))
}

fn load_selected_config(
    explicit: Option<&Path>,
) -> Result<docker_maid::config::LoadedConfig, RunError> {
    let current_dir =
        std::env::current_dir().map_err(|source| docker_maid::config::ConfigError::Read {
            path: PathBuf::from("."),
            source,
        })?;
    let xdg = config_home();
    load_config(explicit, &current_dir, xdg.as_deref()).map_err(RunError::from)
}

fn write_payload(payload: &[u8]) -> io::Result<()> {
    let stdout = io::stdout();
    let mut writer = stdout.lock();
    writer.write_all(payload)?;
    writer.flush()
}

fn write_diagnostic(message: &str) {
    let stderr = io::stderr();
    let mut writer = stderr.lock();
    let _ = writeln!(writer, "{message}");
}

fn output_error_exit_code(error: &io::Error) -> u8 {
    if error.kind() == io::ErrorKind::BrokenPipe {
        0
    } else {
        EXIT_INTERNAL
    }
}

fn config_home() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(".config"))
        })
}

fn display_path(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broken_pipe_is_success() {
        let error = io::Error::from(io::ErrorKind::BrokenPipe);
        assert_eq!(output_error_exit_code(&error), 0);
    }

    #[test]
    fn other_output_error_is_internal_failure() {
        let error = io::Error::from(io::ErrorKind::StorageFull);
        assert_eq!(output_error_exit_code(&error), EXIT_INTERNAL);
    }

    #[test]
    fn clean_is_dry_run_unless_apply_is_explicit() {
        let dry_run = Cli::try_parse_from(["docker_maid", "clean"]).expect("parse clean");
        assert!(matches!(dry_run.command, Command::Clean { apply: false }));

        let applied =
            Cli::try_parse_from(["docker_maid", "clean", "--apply"]).expect("parse apply");
        assert!(matches!(applied.command, Command::Clean { apply: true }));
    }
}
