use clap::{error::ErrorKind, Parser, Subcommand, ValueEnum};
use docker_maid::activity::{stable_config_hash, ActivityJournal, CompletedPass, EventData};
use docker_maid::config::{load_config, DEFAULT_CONFIG};
use docker_maid::executor::{execute_plan, ExecutionReport};
use docker_maid::inventory::collect_inventory;
use docker_maid::plan::{build_plan_with_protection, Action, Disposition, Plan};
use docker_maid::state::{ProtectionKind, ProtectionStore, StatePaths};
use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

const EXIT_PENDING: u8 = 1;
const EXIT_PARTIAL: u8 = 2;
const EXIT_CONFIG: u8 = 3;
const EXIT_DOCKER: u8 = 5;
const EXIT_STATE: u8 = 6;
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
    /// Show current inventory disposition and the last completed cleanup pass.
    Status,
    /// Add one or more typed runtime protection entries.
    Protect {
        #[arg(value_enum)]
        kind: CliProtectionKind,
        #[arg(required = true)]
        values: Vec<String>,
    },
    /// Remove one or more typed runtime protection entries.
    Unprotect {
        #[arg(value_enum)]
        kind: CliProtectionKind,
        #[arg(required = true)]
        values: Vec<String>,
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

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliProtectionKind {
    Container,
    Volume,
    Image,
    Network,
}

impl From<CliProtectionKind> for ProtectionKind {
    fn from(kind: CliProtectionKind) -> Self {
        match kind {
            CliProtectionKind::Container => Self::Container,
            CliProtectionKind::Volume => Self::Volume,
            CliProtectionKind::Image => Self::Image,
            CliProtectionKind::Network => Self::Network,
        }
    }
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
            let code = if matches!(error, docker_maid::executor::ExecutionError::State(_)) {
                EXIT_STATE
            } else {
                EXIT_DOCKER
            };
            write_diagnostic(&format!("error: {error}"));
            ExitCode::from(code)
        }
        Err(RunError::State(error)) => {
            write_diagnostic(&format!("error: {error}"));
            ExitCode::from(EXIT_STATE)
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
    State(String),
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

impl From<docker_maid::state::StateError> for RunError {
    fn from(error: docker_maid::state::StateError) -> Self {
        Self::State(error.to_string())
    }
}

impl From<docker_maid::activity::ActivityError> for RunError {
    fn from(error: docker_maid::activity::ActivityError) -> Self {
        Self::State(error.to_string())
    }
}

async fn run(cli: Cli) -> Result<RunOutcome, RunError> {
    match cli.command {
        Command::Plan => run_cleanup(cli.config.as_deref(), false, false).await,
        Command::Clean { apply } => run_cleanup(cli.config.as_deref(), true, apply).await,
        Command::Status => run_status(cli.config.as_deref()).await,
        Command::Protect { kind, values } => {
            let store = ProtectionStore::from_env()?;
            let kind = ProtectionKind::from(kind);
            let added = store.add(kind, &values)?;
            let message = format!(
                "Protection state updated: added {added}, total {}.\n",
                store.snapshot()?.entries.len()
            );
            write_payload(message.as_bytes())?;
            Ok(RunOutcome::Success)
        }
        Command::Unprotect { kind, values } => {
            let kind = ProtectionKind::from(kind);
            refuse_config_sourced_unprotect(cli.config.as_deref(), kind, &values)?;
            let store = ProtectionStore::from_env()?;
            let removed = store.remove(kind, &values)?;
            let message = format!(
                "Protection state updated: removed {removed}, total {}.\n",
                store.snapshot()?.entries.len()
            );
            write_payload(message.as_bytes())?;
            Ok(RunOutcome::Success)
        }
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
            "warning: build-cache policy is authorized-unscoped because Docker cache records have no ownership metadata",
        );
    }
    let state_paths = StatePaths::from_env()?;
    let protection_store = ProtectionStore::new(state_paths.clone());
    let runtime_protection = protection_store.snapshot()?;
    let inventory = collect_inventory(&loaded.config).await?;
    let plan = build_plan_with_protection(
        &loaded.config,
        inventory,
        epoch_seconds()?,
        &runtime_protection,
    )
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

    let journal = ActivityJournal::new(state_paths);
    let started_at = epoch_seconds()?;
    let activity = journal.start_pass("clean", &stable_config_hash(&loaded.source), started_at)?;
    let report = if plan.has_pending_removals() {
        execute_plan(
            &loaded.path,
            &loaded.config,
            &loaded.source,
            &plan,
            &protection_store,
        )
        .await?
    } else {
        ExecutionReport {
            outcomes: Vec::new(),
        }
    };
    activity.finish(&plan, &report, epoch_seconds()?)?;
    let partial = report.has_partial_failure();
    let output = if plan.has_pending_removals() {
        report.render_table()
    } else {
        plan.render_table()
    };
    write_payload(output.as_bytes())?;
    Ok(if partial {
        RunOutcome::PartialFailure
    } else {
        RunOutcome::Success
    })
}

async fn run_status(explicit_config: Option<&Path>) -> Result<RunOutcome, RunError> {
    let loaded = load_selected_config(explicit_config)?;
    let paths = StatePaths::from_env()?;
    let protection = ProtectionStore::new(paths.clone()).snapshot()?;
    let inventory = collect_inventory(&loaded.config).await?;
    let plan = build_plan_with_protection(&loaded.config, inventory, epoch_seconds()?, &protection)
        .map_err(|error| RunError::Internal(format!("cannot build status: {error}")))?;
    let last_pass = ActivityJournal::new(paths).last_completed_pass()?;
    let output = render_status(&plan, protection.entries.len(), last_pass.as_ref());
    write_payload(output.as_bytes())?;
    Ok(RunOutcome::Success)
}

fn render_status(
    plan: &Plan,
    runtime_protection_count: usize,
    last: Option<&CompletedPass>,
) -> String {
    let protected = plan
        .decisions
        .iter()
        .filter(|decision| decision.disposition == Disposition::Protected)
        .count();
    let owned = plan
        .decisions
        .iter()
        .filter(|decision| decision.disposition == Disposition::Owned)
        .count();
    let authorized = plan
        .decisions
        .iter()
        .filter(|decision| decision.disposition == Disposition::AuthorizedUnscoped)
        .count();
    let unowned = plan
        .decisions
        .iter()
        .filter(|decision| decision.disposition == Disposition::Unowned)
        .count();
    let mut output = format!(
        "Inventory: total={}, protected={}, owned={}, authorized-unscoped={}, unowned={}, pending={}\nRuntime protection entries: {runtime_protection_count}\n",
        plan.decisions.len(),
        protected,
        owned,
        authorized,
        unowned,
        plan.pending_count()
    );
    let Some(last) = last else {
        output.push_str("Last completed cleanup pass: none\n");
        return output;
    };
    writeln!(
        output,
        "Last completed cleanup pass: {} ({} to {}, source={}, config={})",
        last.pass_id, last.started_at, last.completed_at, last.source, last.config_hash
    )
    .expect("writing status to a String cannot fail");
    for event in &last.actions {
        if let EventData::Action {
            action,
            resource_kind,
            resource_name,
            matched_rule,
            freed_bytes,
            ..
        } = &event.data
        {
            writeln!(
                output,
                "  {action} {resource_kind} {resource_name} rule={matched_rule} freed_bytes={freed_bytes}"
            )
            .expect("writing status to a String cannot fail");
        }
    }
    writeln!(
        output,
        "Last result: removed={}, skipped={}, failed={}, reclaimed_bytes={}",
        last.removed_count, last.skipped_count, last.failure_count, last.reclaimed_bytes
    )
    .expect("writing status to a String cannot fail");
    output
}

fn refuse_config_sourced_unprotect(
    explicit_config: Option<&Path>,
    kind: ProtectionKind,
    values: &[String],
) -> Result<(), RunError> {
    let loaded = match load_selected_config(explicit_config) {
        Ok(loaded) => Some(loaded),
        Err(RunError::Config(docker_maid::config::ConfigError::NotFound { .. }))
            if explicit_config.is_none() =>
        {
            None
        }
        Err(error) => return Err(error),
    };
    let Some(loaded) = loaded else {
        return Ok(());
    };
    for value in values {
        for (index, pattern) in loaded.config.protect.names.iter().enumerate() {
            let matches = regex::Regex::new(pattern).is_ok_and(|regex| regex.is_match(value));
            if matches {
                let line = protect_names_line(&loaded.source)
                    .map_or_else(String::new, |line| format!(":{line}"));
                return Err(RunError::State(format!(
                    "cannot unprotect {kind} {value:?}: it is protected by {}{line} (protect.names[{index}]); edit that configuration entry",
                    loaded.path.display(),
                )));
            }
        }
    }
    Ok(())
}

fn protect_names_line(source: &str) -> Option<usize> {
    let mut in_protect = false;
    for (index, line) in source.lines().enumerate() {
        let line = line.trim();
        if line == "[protect]" {
            in_protect = true;
            continue;
        }
        if in_protect && line.starts_with('[') {
            return None;
        }
        if in_protect
            && line
                .split_once('=')
                .is_some_and(|(key, _)| key.trim() == "names")
        {
            return Some(index + 1);
        }
    }
    None
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
