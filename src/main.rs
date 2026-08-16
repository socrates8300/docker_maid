use clap::{error::ErrorKind, Parser, Subcommand, ValueEnum};
use docker_maid::activity::{stable_config_hash, ActivityJournal, CompletedPass, EventData};
use docker_maid::config::{load_config, Config, LoadedConfig, DEFAULT_CONFIG};
use docker_maid::executor::{execute_plan, ExecutionReport};
use docker_maid::inventory::collect_inventory;
use docker_maid::machine;
use docker_maid::plan::{build_plan_with_protection, Action, Disposition, Plan};
use docker_maid::state::{ProtectionKind, ProtectionStore, StatePaths};
use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const EXIT_PENDING: u8 = 1;
const EXIT_PARTIAL: u8 = 2;
const EXIT_CONFIG: u8 = 3;
const EXIT_DOCKER: u8 = 5;
const EXIT_STATE: u8 = 6;
const EXIT_INTERNAL: u8 = 7;
const EXIT_USAGE: u8 = 64;
const DEFAULT_DAEMON_INTERVAL: &str = "5m";

#[derive(Debug, Parser)]
#[command(name = "docker_maid", version, about)]
struct Cli {
    /// Read configuration from this exact path.
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Select human-readable tables or the stable machine schema.
    #[arg(long, global = true, value_enum, default_value_t = OutputFormat::Table)]
    format: OutputFormat,

    /// Alias for --format json.
    #[arg(long, global = true, conflicts_with = "format")]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

impl Cli {
    fn output_format(&self) -> OutputFormat {
        if self.json {
            OutputFormat::Json
        } else {
            self.format
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum OutputFormat {
    Table,
    Json,
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
    /// Continuously run policy-derived cleanup passes on an interval.
    Daemon {
        /// Apply each generated plan without prompting.
        #[arg(long)]
        apply: bool,
        /// Override the configured pass interval (for example, 30s or 5m).
        #[arg(long, value_name = "DURATION")]
        interval: Option<String>,
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
    let arguments = std::env::args_os().collect::<Vec<_>>();
    let requested_format = requested_output_format(&arguments);
    let cli = match Cli::try_parse_from(&arguments) {
        Ok(cli) => cli,
        Err(error) => {
            if error.kind() == ErrorKind::DisplayVersion && requested_format == OutputFormat::Json {
                let result = write_json_payload(&machine::version_document());
                return ExitCode::from(
                    result.map_or_else(|write_error| output_error_exit_code(&write_error), |()| 0),
                );
            }
            if error.kind() == ErrorKind::DisplayHelp {
                let _ = error.print();
                return ExitCode::SUCCESS;
            }
            if requested_format == OutputFormat::Json {
                write_error_diagnostic(OutputFormat::Json, "usage", &error.to_string());
            } else {
                let _ = error.print();
            }
            return ExitCode::from(EXIT_USAGE);
        }
    };
    let format = cli.output_format();

    match run(cli, format).await {
        Ok(RunOutcome::Success) => ExitCode::SUCCESS,
        Ok(RunOutcome::PendingRemovals) => ExitCode::from(EXIT_PENDING),
        Ok(RunOutcome::PartialFailure) => {
            if format == OutputFormat::Json {
                write_error_diagnostic(
                    format,
                    "partial_failure",
                    "one or more planned removals were skipped or failed",
                );
            }
            ExitCode::from(EXIT_PARTIAL)
        }
        Err(RunError::Output(error)) => {
            let code = output_error_exit_code(&error);
            if code != 0 {
                write_error_diagnostic(
                    format,
                    "internal",
                    &format!("cannot write stdout: {error}"),
                );
            }
            ExitCode::from(code)
        }
        Err(error) => {
            let (code, kind) = run_error_classification(&error);
            write_error_diagnostic(format, kind, &run_error_message(&error));
            ExitCode::from(code)
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
    Usage(String),
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

async fn run(cli: Cli, format: OutputFormat) -> Result<RunOutcome, RunError> {
    match cli.command {
        Command::Plan => run_cleanup(cli.config.as_deref(), false, false, format).await,
        Command::Clean { apply } => run_cleanup(cli.config.as_deref(), true, apply, format).await,
        Command::Daemon { apply, interval } => {
            run_daemon(cli.config.as_deref(), apply, interval.as_deref(), format).await
        }
        Command::Status => run_status(cli.config.as_deref(), format).await,
        Command::Protect { kind, values } => {
            let store = ProtectionStore::from_env()?;
            let kind = ProtectionKind::from(kind);
            let added = store.add(kind, &values)?;
            let total = store.snapshot()?.entries.len();
            if format == OutputFormat::Json {
                write_json_payload(&machine::protection_document(
                    "protect",
                    &kind.to_string(),
                    added,
                    total,
                ))?;
            } else {
                let message = format!("Protection state updated: added {added}, total {total}.\n");
                write_payload(message.as_bytes())?;
            }
            Ok(RunOutcome::Success)
        }
        Command::Unprotect { kind, values } => {
            let kind = ProtectionKind::from(kind);
            refuse_config_sourced_unprotect(cli.config.as_deref(), kind, &values)?;
            let store = ProtectionStore::from_env()?;
            let removed = store.remove(kind, &values)?;
            let total = store.snapshot()?.entries.len();
            if format == OutputFormat::Json {
                write_json_payload(&machine::protection_document(
                    "unprotect",
                    &kind.to_string(),
                    removed,
                    total,
                ))?;
            } else {
                let message =
                    format!("Protection state updated: removed {removed}, total {total}.\n");
                write_payload(message.as_bytes())?;
            }
            Ok(RunOutcome::Success)
        }
        Command::Config {
            command: ConfigCommand::Default,
        } => {
            if format == OutputFormat::Json {
                let config = Config::parse(DEFAULT_CONFIG, Path::new("<default>"))?;
                config.validate()?;
                write_json_payload(&machine::config_document("config.default", None, &config))?;
            } else {
                write_payload(DEFAULT_CONFIG.as_bytes())?;
            }
            Ok(RunOutcome::Success)
        }
        Command::Config { command } => {
            let loaded = load_selected_config(cli.config.as_deref())?;

            match command {
                ConfigCommand::Check => {
                    if format == OutputFormat::Json {
                        write_json_payload(&machine::config_document(
                            "config.check",
                            Some(&display_path(&loaded.path)),
                            &loaded.config,
                        ))?;
                    } else {
                        let message =
                            format!("configuration valid: {}\n", display_path(&loaded.path));
                        write_payload(message.as_bytes())?;
                    }
                }
                ConfigCommand::Print => {
                    if format == OutputFormat::Json {
                        write_json_payload(&machine::config_document(
                            "config.print",
                            Some(&display_path(&loaded.path)),
                            &loaded.config,
                        ))?;
                    } else {
                        let normalized = loaded.config.to_normalized_toml()?;
                        write_payload(normalized.as_bytes())?;
                    }
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
    format: OutputFormat,
) -> Result<RunOutcome, RunError> {
    let prepared = prepare_cleanup(explicit_config, format).await?;
    let result = if clean_command && apply {
        apply_cleanup(&prepared, "clean", format).await?
    } else {
        dry_run_cleanup(&prepared)
    };
    if format == OutputFormat::Json {
        let command = if clean_command { "clean" } else { "plan" };
        write_json_payload(&machine::plan_document(
            command,
            clean_command && apply,
            &prepared.plan,
            result.report.as_ref(),
        ))?;
    } else {
        write_payload(result.output.as_bytes())?;
    }
    Ok(result.outcome)
}

struct PreparedCleanup {
    loaded: LoadedConfig,
    state_paths: StatePaths,
    protection_store: ProtectionStore,
    plan: Plan,
}

struct CleanupPassResult {
    output: String,
    outcome: RunOutcome,
    report: Option<ExecutionReport>,
}

async fn prepare_cleanup(
    explicit_config: Option<&Path>,
    format: OutputFormat,
) -> Result<PreparedCleanup, RunError> {
    let loaded = load_selected_config(explicit_config)?;
    if loaded.config.rules.build_cache.is_some() {
        write_warning_diagnostic(
            format,
            "authorized_unscoped",
            "build-cache policy is authorized-unscoped because Docker cache records have no ownership metadata",
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

    Ok(PreparedCleanup {
        loaded,
        state_paths,
        protection_store,
        plan,
    })
}

fn dry_run_cleanup(prepared: &PreparedCleanup) -> CleanupPassResult {
    CleanupPassResult {
        output: prepared.plan.render_table(),
        outcome: if prepared.plan.has_pending_removals() {
            RunOutcome::PendingRemovals
        } else {
            RunOutcome::Success
        },
        report: None,
    }
}

async fn apply_cleanup(
    prepared: &PreparedCleanup,
    source: &str,
    format: OutputFormat,
) -> Result<CleanupPassResult, RunError> {
    let authorized_unscoped = prepared
        .plan
        .decisions
        .iter()
        .filter(|decision| {
            decision.action == Action::Remove
                && decision.disposition == Disposition::AuthorizedUnscoped
        })
        .count();
    if authorized_unscoped != 0 {
        write_warning_diagnostic(
            format,
            "authorized_unscoped_apply",
            &format!("applying {authorized_unscoped} authorized-unscoped removal(s)"),
        );
    }

    let journal = ActivityJournal::new(prepared.state_paths.clone());
    let started_at = epoch_seconds()?;
    let activity = journal.start_pass(
        source,
        &stable_config_hash(&prepared.loaded.source),
        started_at,
    )?;
    let report = if prepared.plan.has_pending_removals() {
        execute_plan(
            &prepared.loaded.path,
            &prepared.loaded.config,
            &prepared.loaded.source,
            &prepared.plan,
            &prepared.protection_store,
        )
        .await?
    } else {
        ExecutionReport {
            outcomes: Vec::new(),
        }
    };
    activity.finish(&prepared.plan, &report, epoch_seconds()?)?;
    let partial = report.has_partial_failure();
    let output = if prepared.plan.has_pending_removals() {
        report.render_table()
    } else {
        prepared.plan.render_table()
    };
    Ok(CleanupPassResult {
        output,
        outcome: if partial {
            RunOutcome::PartialFailure
        } else {
            RunOutcome::Success
        },
        report: Some(report),
    })
}

async fn run_daemon(
    explicit_config: Option<&Path>,
    apply: bool,
    interval_override: Option<&str>,
    format: OutputFormat,
) -> Result<RunOutcome, RunError> {
    let override_interval = interval_override
        .map(|source| daemon_interval(&Config::default(), Some(source)))
        .transpose()?;
    let initial = load_selected_config(explicit_config)?;
    let mut interval = match override_interval {
        Some(interval) => interval,
        None => daemon_interval(&initial.config, None)?,
    };
    let mut signals = DaemonSignals::new()?;
    announce_daemon_start(format, apply, interval)?;
    let mut trigger = "startup";
    let mut pass_number = 0u64;

    loop {
        pass_number = pass_number.saturating_add(1);
        if format == OutputFormat::Json {
            write_json_payload(&machine::daemon_pass_started_event(
                pass_number,
                trigger,
                apply,
                epoch_seconds()?,
            ))?;
        }
        match run_daemon_pass(
            explicit_config,
            apply,
            interval_override,
            pass_number,
            trigger,
            format,
        )
        .await
        {
            Ok(next_interval) => interval = next_interval,
            Err(error @ RunError::Output(_)) => return Err(error),
            Err(error) => announce_daemon_pass_failure(format, pass_number, &error)?,
        }

        match signals.wait(interval).await {
            DaemonWake::Interval => trigger = "interval",
            DaemonWake::Reload => {
                announce_daemon_reload(format)?;
                trigger = "SIGHUP";
            }
            DaemonWake::Terminate => {
                announce_daemon_stop(format)?;
                return Ok(RunOutcome::Success);
            }
        }
    }
}

fn announce_daemon_start(
    format: OutputFormat,
    apply: bool,
    interval: Duration,
) -> Result<(), RunError> {
    let mode = if apply { "apply" } else { "dry-run" };
    if format == OutputFormat::Json {
        write_json_payload(&machine::daemon_event(
            "daemon_started",
            epoch_seconds()?,
            serde_json::json!({
                "mode": mode,
                "interval_seconds": interval.as_secs_f64(),
            }),
        ))?;
    } else {
        write_diagnostic(&format!(
            "daemon: started in {mode} mode; interval {}",
            humantime::format_duration(interval)
        ));
    }
    Ok(())
}

fn announce_daemon_pass_failure(
    format: OutputFormat,
    pass_number: u64,
    error: &RunError,
) -> Result<(), RunError> {
    if format == OutputFormat::Json {
        let (_, kind) = run_error_classification(error);
        write_json_payload(&machine::daemon_event(
            "pass_error",
            epoch_seconds()?,
            serde_json::json!({
                "pass_number": pass_number,
                "error": {
                    "kind": kind,
                    "message": run_error_message(error),
                    "details": [],
                },
            }),
        ))?;
    } else {
        write_diagnostic(&format!(
            "error: daemon pass {pass_number} failed: {}",
            run_error_message(error)
        ));
    }
    Ok(())
}

fn announce_daemon_reload(format: OutputFormat) -> Result<(), RunError> {
    if format == OutputFormat::Json {
        write_json_payload(&machine::daemon_event(
            "configuration_reload_requested",
            epoch_seconds()?,
            serde_json::json!({"signal": "SIGHUP"}),
        ))?;
    } else {
        write_diagnostic("daemon: SIGHUP received; reloading configuration");
    }
    Ok(())
}

fn announce_daemon_stop(format: OutputFormat) -> Result<(), RunError> {
    if format == OutputFormat::Json {
        write_json_payload(&machine::daemon_event(
            "daemon_stopped",
            epoch_seconds()?,
            serde_json::json!({"reason": "shutdown_requested"}),
        ))?;
    } else {
        write_diagnostic("daemon: shutdown requested; current pass complete");
    }
    Ok(())
}

async fn run_daemon_pass(
    explicit_config: Option<&Path>,
    apply: bool,
    interval_override: Option<&str>,
    pass_number: u64,
    trigger: &str,
    format: OutputFormat,
) -> Result<Duration, RunError> {
    let prepared = prepare_cleanup(explicit_config, format).await?;
    let next_interval = daemon_interval(&prepared.loaded.config, interval_override)?;
    let result = if apply {
        apply_cleanup(&prepared, "daemon", format).await?
    } else {
        dry_run_cleanup(&prepared)
    };
    if format == OutputFormat::Json {
        for event in machine::daemon_pass_result_events(
            pass_number,
            apply,
            &prepared.plan,
            result.report.as_ref(),
            epoch_seconds()?,
        ) {
            write_json_payload(&event)?;
        }
    } else {
        let mode = if apply { "apply" } else { "dry-run" };
        let output = format!(
            "Daemon pass {pass_number} ({trigger}, {mode})\n{}",
            result.output
        );
        write_payload(output.as_bytes())?;
    }
    Ok(next_interval)
}

fn daemon_interval(config: &Config, interval_override: Option<&str>) -> Result<Duration, RunError> {
    let (field, source) = interval_override.map_or_else(
        || {
            (
                "defaults.interval",
                config
                    .defaults
                    .interval
                    .as_deref()
                    .unwrap_or(DEFAULT_DAEMON_INTERVAL),
            )
        },
        |source| ("--interval", source),
    );
    let duration = humantime::parse_duration(source).map_err(|error| {
        RunError::Usage(format!("{field} must be a positive duration: {error}"))
    })?;
    if duration.is_zero() {
        return Err(RunError::Usage(format!(
            "{field} must be greater than zero"
        )));
    }
    Ok(duration)
}

fn run_error_message(error: &RunError) -> String {
    match error {
        RunError::Config(error) => error.to_string(),
        RunError::Docker(error) => error.to_string(),
        RunError::Execution(error) => error.to_string(),
        RunError::State(error) | RunError::Usage(error) | RunError::Internal(error) => {
            error.clone()
        }
        RunError::Output(error) => format!("cannot write stdout: {error}"),
    }
}

fn run_error_classification(error: &RunError) -> (u8, &'static str) {
    match error {
        RunError::Config(_) => (EXIT_CONFIG, "config_invalid"),
        RunError::Execution(docker_maid::executor::ExecutionError::State(_))
        | RunError::State(_) => (EXIT_STATE, "state_io"),
        RunError::Docker(_) | RunError::Execution(_) => (EXIT_DOCKER, "docker_unreachable"),
        RunError::Usage(_) => (EXIT_USAGE, "usage"),
        RunError::Internal(_) | RunError::Output(_) => (EXIT_INTERNAL, "internal"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DaemonWake {
    Interval,
    Reload,
    Terminate,
}

#[cfg(unix)]
struct DaemonSignals {
    reload: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
    interrupt: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl DaemonSignals {
    fn new() -> Result<Self, RunError> {
        use tokio::signal::unix::{signal, SignalKind};

        Ok(Self {
            reload: signal(SignalKind::hangup()).map_err(|error| {
                RunError::Internal(format!("cannot register SIGHUP handler: {error}"))
            })?,
            terminate: signal(SignalKind::terminate()).map_err(|error| {
                RunError::Internal(format!("cannot register SIGTERM handler: {error}"))
            })?,
            interrupt: signal(SignalKind::interrupt()).map_err(|error| {
                RunError::Internal(format!("cannot register SIGINT handler: {error}"))
            })?,
        })
    }

    async fn wait(&mut self, interval: Duration) -> DaemonWake {
        tokio::select! {
            _ = self.reload.recv() => DaemonWake::Reload,
            _ = self.terminate.recv() => DaemonWake::Terminate,
            _ = self.interrupt.recv() => DaemonWake::Terminate,
            () = tokio::time::sleep(interval) => DaemonWake::Interval,
        }
    }
}

#[cfg(not(unix))]
struct DaemonSignals;

#[cfg(not(unix))]
impl DaemonSignals {
    fn new() -> Result<Self, RunError> {
        Err(RunError::Internal(
            "daemon signal handling currently requires a Unix platform".to_owned(),
        ))
    }

    async fn wait(&mut self, _interval: Duration) -> DaemonWake {
        unreachable!("daemon construction fails on non-Unix platforms")
    }
}

async fn run_status(
    explicit_config: Option<&Path>,
    format: OutputFormat,
) -> Result<RunOutcome, RunError> {
    let loaded = load_selected_config(explicit_config)?;
    let paths = StatePaths::from_env()?;
    let protection = ProtectionStore::new(paths.clone()).snapshot()?;
    let inventory = collect_inventory(&loaded.config).await?;
    let plan = build_plan_with_protection(&loaded.config, inventory, epoch_seconds()?, &protection)
        .map_err(|error| RunError::Internal(format!("cannot build status: {error}")))?;
    let last_pass = ActivityJournal::new(paths).last_completed_pass()?;
    for rule in regressed_rules(&plan, last_pass.as_ref()) {
        write_warning_diagnostic(
            format,
            "rule_health_regressed",
            &format!("rule {rule:?} previously matched resources but now matches none"),
        );
    }
    if format == OutputFormat::Json {
        write_json_payload(&machine::status_document(
            &display_path(&loaded.path),
            &stable_config_hash(&loaded.source),
            &loaded.config,
            &plan,
            protection.entries.len(),
            last_pass.as_ref(),
        ))?;
    } else {
        let output = render_status(&plan, protection.entries.len(), last_pass.as_ref());
        write_payload(output.as_bytes())?;
    }
    Ok(RunOutcome::Success)
}

fn regressed_rules(plan: &Plan, last: Option<&CompletedPass>) -> Vec<String> {
    let Some(last) = last else {
        return Vec::new();
    };
    last.rule_match_counts
        .iter()
        .filter(|(rule, previous)| {
            **previous != 0
                && !plan.decisions.iter().any(|decision| {
                    decision
                        .matched_rule
                        .as_ref()
                        .is_some_and(|name| name == *rule)
                })
        })
        .map(|(rule, _)| rule.clone())
        .collect()
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

fn write_json_payload(value: &serde_json::Value) -> io::Result<()> {
    let payload = machine::to_line(value).map_err(io::Error::other)?;
    write_payload(&payload)
}

fn write_diagnostic(message: &str) {
    let stderr = io::stderr();
    let mut writer = stderr.lock();
    let _ = writeln!(writer, "{message}");
}

fn write_error_diagnostic(format: OutputFormat, kind: &str, message: &str) {
    if format == OutputFormat::Json {
        write_json_diagnostic(&machine::error_document(kind, message.trim_end()));
    } else {
        write_diagnostic(&format!("error: {}", message.trim_end()));
    }
}

fn write_warning_diagnostic(format: OutputFormat, kind: &str, message: &str) {
    if format == OutputFormat::Json {
        write_json_diagnostic(&machine::warning_document(kind, message));
    } else {
        write_diagnostic(&format!("warning: {message}"));
    }
}

fn write_json_diagnostic(value: &serde_json::Value) {
    let Ok(payload) = machine::to_line(value) else {
        return;
    };
    let stderr = io::stderr();
    let mut writer = stderr.lock();
    let _ = writer.write_all(&payload);
    let _ = writer.flush();
}

fn output_error_exit_code(error: &io::Error) -> u8 {
    if error.kind() == io::ErrorKind::BrokenPipe {
        0
    } else {
        EXIT_INTERNAL
    }
}

fn requested_output_format(arguments: &[OsString]) -> OutputFormat {
    let mut index = 1usize;
    while index < arguments.len() {
        let Some(argument) = arguments[index].to_str() else {
            index = index.saturating_add(1);
            continue;
        };
        if argument == "--json" || argument == "--format=json" {
            return OutputFormat::Json;
        }
        if argument == "--format"
            && arguments
                .get(index.saturating_add(1))
                .and_then(|value| value.to_str())
                == Some("json")
        {
            return OutputFormat::Json;
        }
        index = index.saturating_add(1);
    }
    OutputFormat::Table
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

    #[test]
    fn daemon_is_dry_run_unless_apply_is_explicit() {
        let monitor = Cli::try_parse_from(["docker_maid", "daemon"]).expect("parse daemon");
        assert!(matches!(
            monitor.command,
            Command::Daemon {
                apply: false,
                interval: None
            }
        ));

        let applied =
            Cli::try_parse_from(["docker_maid", "daemon", "--apply", "--interval", "30s"])
                .expect("parse applied daemon");
        assert!(matches!(
            applied.command,
            Command::Daemon {
                apply: true,
                interval: Some(ref value)
            } if value == "30s"
        ));
    }

    #[test]
    fn daemon_interval_must_be_positive() {
        let config = Config::default();
        assert_eq!(
            daemon_interval(&config, Some("250ms")).expect("duration"),
            Duration::from_millis(250)
        );
        assert!(matches!(
            daemon_interval(&config, Some("0s")),
            Err(RunError::Usage(message)) if message.contains("greater than zero")
        ));
        assert!(matches!(
            daemon_interval(&config, Some("later")),
            Err(RunError::Usage(message)) if message.contains("positive duration")
        ));
    }
}
