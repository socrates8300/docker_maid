//! Interactive terminal frontend over the shared inventory, policy, state, and executor core.

use super::{epoch_seconds, load_selected_config, RunError, RunOutcome};
use crossterm::cursor::{Hide, Show};
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use docker_maid::activity::{stable_config_hash, ActivityJournal, CompletedPass, EventData};
use docker_maid::config::{Config, ConfigError, LoadedConfig};
use docker_maid::configurator::{
    add_name_prefix_candidate, configuration_target_path, propose_configuration, survey_inventory,
    write_proposal, ConfigProposal, ConfiguratorError, ConfiguratorSurvey, PolicyProfile,
    PolicySettings, ProposalRequest, MANAGED_ID_PREFIX,
};
use docker_maid::executor::{execute_plan, ExecutionReport, TargetStatus};
use docker_maid::inventory::collect_inventory_for_configuration;
use docker_maid::plan::{
    build_plan_with_protection, Action, Decision, Disposition, Plan, ResourceKind,
};
use docker_maid::state::{ProtectionKind, ProtectionStore, StatePaths};
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, Gauge, List, ListItem, Paragraph, Row, Sparkline, Table,
    TableState, Tabs, Wrap,
};
use ratatui::{Frame, Terminal};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::time::Duration;

type CrosstermTerminal = Terminal<CrosstermBackend<Stdout>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    Dashboard,
    Inventory,
    Plan,
    Activity,
    Configure,
}

impl View {
    const ALL: [Self; 5] = [
        Self::Dashboard,
        Self::Inventory,
        Self::Plan,
        Self::Activity,
        Self::Configure,
    ];

    fn index(self) -> usize {
        match self {
            Self::Dashboard => 0,
            Self::Inventory => 1,
            Self::Plan => 2,
            Self::Activity => 3,
            Self::Configure => 4,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Dashboard => "Dashboard",
            Self::Inventory => "Inventory",
            Self::Plan => "Plan",
            Self::Activity => "Activity",
            Self::Configure => "Configure",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Overlay {
    None,
    Help,
    Confirm,
    CacheConfirm,
    ConfigSave,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlanValidity {
    Valid,
    Stale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Editor {
    None,
    Filter,
    Prefix,
    Policy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PolicyField {
    Containers,
    Images,
    Volumes,
    CacheAge,
    CacheBytes,
}

impl PolicyField {
    const ALL: [Self; 5] = [
        Self::Containers,
        Self::Images,
        Self::Volumes,
        Self::CacheAge,
        Self::CacheBytes,
    ];

    const fn title(self) -> &'static str {
        match self {
            Self::Containers => "stopped containers",
            Self::Images => "images",
            Self::Volumes => "volumes",
            Self::CacheAge => "build-cache age",
            Self::CacheBytes => "build-cache bytes",
        }
    }
}

struct App {
    explicit_config: Option<PathBuf>,
    loaded: LoadedConfig,
    built_in_config: bool,
    state_paths: StatePaths,
    protection_store: ProtectionStore,
    plan: Plan,
    plan_id: String,
    plan_created_at: i64,
    config_hash: String,
    plan_validity: PlanValidity,
    history: Vec<CompletedPass>,
    view: View,
    inventory_kind: ResourceKind,
    selected: usize,
    filter: String,
    editor: Editor,
    detail_focused: bool,
    overlay: Overlay,
    confirm_scroll: usize,
    activity_scroll: u16,
    rules_scroll: u16,
    survey: ConfiguratorSurvey,
    configure_selected: BTreeSet<String>,
    configure_row: usize,
    configure_profile: PolicyProfile,
    configure_policy: PolicySettings,
    policy_field: usize,
    config_proposal: Option<ConfigProposal>,
    prefix_input: String,
    status: String,
}

impl App {
    async fn load(explicit_config: Option<&Path>) -> Result<Self, RunError> {
        let (loaded, built_in_config) = load_tui_config(explicit_config)?;
        let state_paths = StatePaths::from_env()?;
        let protection_store = ProtectionStore::new(state_paths.clone());
        let protection = protection_store.snapshot()?;
        let plan_created_at = epoch_seconds()?;
        let config_hash = stable_config_hash(&loaded.source);
        let (plan, survey, docker_error) = match collect_inventory_for_configuration().await {
            Ok(inventory) => (
                build_plan_with_protection(
                    &loaded.config,
                    inventory.clone(),
                    plan_created_at,
                    &protection,
                )
                .map_err(|error| {
                    RunError::Internal(format!("cannot build TUI snapshot: {error}"))
                })?,
                survey_inventory(&inventory),
                None,
            ),
            Err(error) => (
                Plan {
                    decisions: Vec::new(),
                },
                survey_inventory(&[]),
                Some(error.to_string()),
            ),
        };
        let plan_id = tui_plan_id(&config_hash, plan_created_at, &plan);
        let history = ActivityJournal::new(state_paths.clone()).completed_passes()?;
        let plan_validity = if docker_error.is_none() {
            PlanValidity::Valid
        } else {
            PlanValidity::Stale
        };
        let status = docker_error.map_or_else(
            || startup_status(&loaded, built_in_config, &plan),
            |error| format!("Docker unavailable: {error} • fix the endpoint and press r"),
        );
        let configure_selected = managed_candidate_ids(&loaded.config, &survey);
        Ok(Self {
            explicit_config: explicit_config.map(Path::to_path_buf),
            loaded,
            built_in_config,
            state_paths,
            protection_store,
            plan,
            plan_id,
            plan_created_at,
            config_hash,
            plan_validity,
            history,
            view: if built_in_config {
                View::Configure
            } else {
                View::Dashboard
            },
            inventory_kind: ResourceKind::Container,
            selected: 0,
            filter: String::new(),
            editor: Editor::None,
            detail_focused: false,
            overlay: Overlay::None,
            confirm_scroll: 0,
            activity_scroll: 0,
            rules_scroll: 0,
            survey,
            configure_selected,
            configure_row: 0,
            configure_profile: PolicyProfile::Workstation,
            configure_policy: PolicyProfile::Workstation.settings(),
            policy_field: 0,
            config_proposal: None,
            prefix_input: String::new(),
            status,
        })
    }

    fn refresh_interval(&self) -> Duration {
        self.loaded
            .config
            .tui
            .refresh
            .as_deref()
            .and_then(|value| humantime::parse_duration(value).ok())
            .filter(|duration| !duration.is_zero())
            .unwrap_or_else(|| Duration::from_mins(5))
    }

    async fn refresh(&mut self) -> Result<(), RunError> {
        let (loaded, built_in_config) = load_tui_config(self.explicit_config.as_deref())?;
        let protection = self.protection_store.snapshot()?;
        let inventory = collect_inventory_for_configuration().await?;
        let plan_created_at = epoch_seconds()?;
        let config_hash = stable_config_hash(&loaded.source);
        let plan = build_plan_with_protection(
            &loaded.config,
            inventory.clone(),
            plan_created_at,
            &protection,
        )
        .map_err(|error| RunError::Internal(format!("cannot refresh TUI snapshot: {error}")))?;
        let history = ActivityJournal::new(self.state_paths.clone()).completed_passes()?;
        self.loaded = loaded;
        self.built_in_config = built_in_config;
        self.plan = plan;
        self.plan_id = tui_plan_id(&config_hash, plan_created_at, &self.plan);
        self.plan_created_at = plan_created_at;
        self.config_hash = config_hash;
        self.plan_validity = PlanValidity::Valid;
        self.history = history;
        self.survey = survey_inventory(&inventory);
        self.configure_selected.retain(|id| {
            self.survey
                .candidates
                .iter()
                .any(|candidate| &candidate.id == id)
        });
        if self.configure_selected.is_empty() {
            self.configure_selected = managed_candidate_ids(&self.loaded.config, &self.survey);
        }
        self.config_proposal = None;
        self.configure_row = self
            .configure_row
            .min(self.survey.candidates.len().saturating_sub(1));
        self.clamp_selection();
        self.status = format!(
            "Refreshed: {} resources, {} pending removals",
            self.plan.decisions.len(),
            self.plan.pending_count()
        );
        if self.loaded.config.rules.build_cache.is_some() {
            self.status
                .push_str(" • WARNING: build cache is authorized-unscoped");
        }
        Ok(())
    }

    fn refresh_activity(&mut self) -> Result<(), RunError> {
        let history = ActivityJournal::new(self.state_paths.clone()).completed_passes()?;
        if history != self.history {
            self.history = history;
            replace_status(&mut self.status, "Activity history updated");
        }
        Ok(())
    }

    fn filtered_inventory(&self) -> Vec<&Decision> {
        self.plan
            .decisions
            .iter()
            .filter(|decision| decision.resource.kind == self.inventory_kind)
            .filter(|decision| {
                self.filter.is_empty()
                    || fuzzy_match(&decision.resource.name, &self.filter)
                    || fuzzy_match(&decision.resource.id, &self.filter)
                    || decision
                        .matched_rule
                        .as_deref()
                        .is_some_and(|rule| fuzzy_match(rule, &self.filter))
            })
            .collect()
    }

    fn plan_targets(&self) -> Vec<&Decision> {
        self.plan
            .decisions
            .iter()
            .filter(|decision| decision.action == Action::Remove)
            .collect()
    }

    fn selected_inventory(&self) -> Option<&Decision> {
        self.filtered_inventory().get(self.selected).copied()
    }

    fn clamp_selection(&mut self) {
        let length = match self.view {
            View::Inventory => self.filtered_inventory().len(),
            View::Plan => self.plan_targets().len(),
            View::Configure => self.survey.candidates.len(),
            View::Dashboard | View::Activity => 0,
        };
        self.selected = self.selected.min(length.saturating_sub(1));
    }

    fn move_selection(&mut self, delta: isize) {
        match self.view {
            View::Inventory | View::Plan => {
                let length = if self.view == View::Inventory {
                    self.filtered_inventory().len()
                } else {
                    self.plan_targets().len()
                };
                if length == 0 {
                    self.selected = 0;
                } else {
                    self.selected = self.selected.saturating_add_signed(delta).min(length - 1);
                }
            }
            View::Activity => {
                self.activity_scroll = move_scroll(self.activity_scroll, delta);
            }
            View::Configure => {
                if self.survey.candidates.is_empty() {
                    self.configure_row = 0;
                } else {
                    self.configure_row = self
                        .configure_row
                        .saturating_add_signed(delta)
                        .min(self.survey.candidates.len() - 1);
                }
            }
            View::Dashboard => {}
        }
    }

    fn selected_candidate(&self) -> Option<&docker_maid::configurator::CandidateFamily> {
        self.survey.candidates.get(self.configure_row)
    }

    fn toggle_selected_candidate(&mut self) {
        let Some(candidate) = self.selected_candidate() else {
            replace_status(&mut self.status, "No ownership candidate selected");
            return;
        };
        let id = candidate.id.clone();
        let build_cache = matches!(
            candidate.selector,
            docker_maid::configurator::CandidateSelector::BuildCache
        );
        if self.configure_selected.remove(&id) {
            self.config_proposal = None;
            self.status = format!("Excluded candidate {id}");
        } else if build_cache {
            self.overlay = Overlay::CacheConfirm;
        } else {
            self.configure_selected.insert(id.clone());
            self.config_proposal = None;
            self.status = format!("Selected candidate {id}");
        }
    }

    fn cycle_profile(&mut self, delta: isize) {
        let current = PolicyProfile::ALL
            .iter()
            .position(|profile| *profile == self.configure_profile)
            .unwrap_or(1);
        let next = current
            .saturating_add_signed(delta)
            .min(PolicyProfile::ALL.len() - 1);
        self.configure_profile = PolicyProfile::ALL[next];
        self.configure_policy = self.configure_profile.settings();
        self.config_proposal = None;
        self.status = format!("Policy profile: {}", self.configure_profile.title());
    }

    fn cycle_policy_field(&mut self, delta: isize) {
        self.policy_field = self
            .policy_field
            .saturating_add_signed(delta)
            .min(PolicyField::ALL.len() - 1);
        self.status = format!(
            "Editable policy field: {} • press e",
            PolicyField::ALL[self.policy_field].title()
        );
    }

    fn start_policy_editor(&mut self) {
        let field = PolicyField::ALL[self.policy_field];
        self.prefix_input = policy_field_value(field, &self.configure_policy);
        self.editor = Editor::Policy;
        self.status = format!(
            "Edit {} • durations use 15m/2h/7d; cache bytes accept 10GiB",
            field.title()
        );
    }

    fn accept_policy_value(&mut self) -> Result<(), RunError> {
        let field = PolicyField::ALL[self.policy_field];
        set_policy_field(field, &mut self.configure_policy, self.prefix_input.trim())?;
        self.configure_policy.validate()?;
        self.config_proposal = None;
        self.editor = Editor::None;
        self.status = format!(
            "Updated {} to {}",
            field.title(),
            policy_field_value(field, &self.configure_policy)
        );
        Ok(())
    }

    fn preview_configuration(&mut self) -> Result<(), RunError> {
        let target = configuration_target_path(
            self.explicit_config.as_deref(),
            (!self.built_in_config).then_some(self.loaded.path.as_path()),
            std::env::var_os("XDG_CONFIG_HOME")
                .filter(|value| !value.is_empty())
                .as_deref()
                .map(Path::new),
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .as_deref()
                .map(Path::new),
        )?;
        let inventory = self
            .plan
            .decisions
            .iter()
            .map(|decision| decision.resource.clone())
            .collect::<Vec<_>>();
        let ids = self.configure_selected.iter().cloned().collect::<Vec<_>>();
        let proposal = propose_configuration(&ProposalRequest {
            base_source: &self.loaded.source,
            source_existed: !self.built_in_config,
            target_path: &target,
            survey: &self.survey,
            inventory: &inventory,
            profile: self.configure_profile,
            policy: Some(&self.configure_policy),
            candidate_ids: &ids,
            now_epoch_seconds: self.plan_created_at,
        })?;
        self.status = format!(
            "Proposal {}: {} pending removals; press s to review save",
            proposal.proposal_id, proposal.preview.after_pending
        );
        self.config_proposal = Some(proposal);
        Ok(())
    }

    fn start_prefix_editor(&mut self) {
        let Some(decision) = self.selected_inventory() else {
            replace_status(&mut self.status, "No inventory object selected");
            return;
        };
        if decision.resource.kind == ResourceKind::BuildCache {
            replace_status(
                &mut self.status,
                "Build cache has no name ownership surface",
            );
            return;
        }
        self.prefix_input = decision.resource.name.clone();
        self.editor = Editor::Prefix;
        replace_status(
            &mut self.status,
            "Edit the exact prefix; Enter adds it, Esc cancels",
        );
    }

    fn accept_prefix(&mut self) -> Result<(), RunError> {
        let kind = self.inventory_kind;
        let inventory = self
            .plan
            .decisions
            .iter()
            .map(|decision| decision.resource.clone())
            .collect::<Vec<_>>();
        let id = add_name_prefix_candidate(
            &mut self.survey,
            &inventory,
            kind,
            self.prefix_input.trim(),
        )?;
        self.configure_selected.insert(id.clone());
        self.configure_row = self
            .survey
            .candidates
            .iter()
            .position(|candidate| candidate.id == id)
            .unwrap_or(0);
        self.config_proposal = None;
        self.editor = Editor::None;
        self.view = View::Configure;
        self.status = format!("Added operator-approved prefix candidate {id}");
        Ok(())
    }

    async fn save_configuration(&mut self) -> Result<(), RunError> {
        let proposal = self.config_proposal.clone().ok_or_else(|| {
            RunError::Internal("configuration save opened without a proposal".to_owned())
        })?;
        let inventory = collect_inventory_for_configuration().await?;
        let result = write_proposal(&proposal, &inventory)?;
        self.explicit_config = Some(result.path.clone());
        self.refresh().await?;
        self.view = View::Plan;
        self.status = format!(
            "Saved {} • reviewed plan refreshed • apply remains a separate confirmation",
            result.path.display()
        );
        Ok(())
    }

    fn change_inventory_kind(&mut self, delta: isize) {
        const KINDS: [ResourceKind; 5] = [
            ResourceKind::Container,
            ResourceKind::Image,
            ResourceKind::Volume,
            ResourceKind::Network,
            ResourceKind::BuildCache,
        ];
        let current = KINDS
            .iter()
            .position(|kind| *kind == self.inventory_kind)
            .unwrap_or(0);
        let next = current
            .saturating_add_signed(delta)
            .min(KINDS.len().saturating_sub(1));
        self.inventory_kind = KINDS[next];
        self.selected = 0;
    }

    async fn toggle_protection(&mut self) -> Result<(), RunError> {
        let Some(decision) = self.selected_inventory().cloned() else {
            replace_status(&mut self.status, "No inventory object selected");
            return Ok(());
        };
        let Some(kind) = protection_kind(decision.resource.kind) else {
            replace_status(
                &mut self.status,
                "Build-cache records cannot be protected: Docker exposes no stable owner metadata",
            );
            return Ok(());
        };
        let snapshot = self.protection_store.snapshot()?;
        if let Some(entry) = snapshot.matching_entry(&decision.resource) {
            let value = entry.value.clone();
            self.protection_store
                .remove(kind, std::slice::from_ref(&value))?;
            self.status = format!("Removed runtime protection: {kind} {value}");
        } else if decision.disposition == Disposition::Protected {
            self.status = format!(
                "{} is protected by configuration; edit {} to remove it",
                decision.resource.name,
                self.loaded.path.display()
            );
            return Ok(());
        } else {
            let value = decision.resource.id.clone();
            self.protection_store
                .add(kind, std::slice::from_ref(&value))?;
            self.status = format!("Protected: {kind} {}", decision.resource.name);
        }
        self.refresh().await
    }

    async fn apply_confirmed_plan(&mut self) -> Result<(), RunError> {
        if self.plan_validity == PlanValidity::Stale {
            replace_status(
                &mut self.status,
                "Plan is stale because refresh failed; fix the error and press r",
            );
            return Ok(());
        }
        if !self.plan.has_pending_removals() {
            replace_status(&mut self.status, "No removals pending");
            return Ok(());
        }
        if self.built_in_config {
            return Err(RunError::Internal(
                "built-in safe mode cannot contain removal targets".to_owned(),
            ));
        }
        let current = match load_tui_config(self.explicit_config.as_deref()) {
            Ok((loaded, false)) => loaded,
            Ok((_, true)) => {
                self.plan_validity = PlanValidity::Stale;
                replace_status(
                    &mut self.status,
                    "Configuration disappeared; press r to generate a new plan",
                );
                return Ok(());
            }
            Err(error) => {
                self.plan_validity = PlanValidity::Stale;
                self.status = format!(
                    "Configuration cannot authorize this plan: {}",
                    super::run_error_message(&error)
                );
                return Ok(());
            }
        };
        if current.source != self.loaded.source || current.config != self.loaded.config {
            self.plan_validity = PlanValidity::Stale;
            replace_status(
                &mut self.status,
                "Configuration changed; press r and confirm the new plan",
            );
            return Ok(());
        }

        let started_at = epoch_seconds()?;
        let journal = ActivityJournal::new(self.state_paths.clone());
        let activity =
            journal.start_pass("tui", &stable_config_hash(&self.loaded.source), started_at)?;
        let report = execute_plan(
            &self.loaded.path,
            &self.loaded.config,
            &self.loaded.source,
            &self.plan,
            &self.protection_store,
        )
        .await?;
        activity.finish(&self.plan, &report, epoch_seconds()?)?;
        self.status = execution_status(&report);
        self.refresh().await?;
        self.status = execution_status(&report);
        Ok(())
    }
}

struct TerminalGuard {
    terminal: CrosstermTerminal,
}

impl TerminalGuard {
    fn enter() -> Result<Self, RunError> {
        install_panic_hook();
        enable_raw_mode().map_err(|error| {
            RunError::Internal(format!("cannot enable terminal raw mode: {error}"))
        })?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen, Hide) {
            restore_terminal();
            return Err(RunError::Internal(format!(
                "cannot enter terminal screen: {error}"
            )));
        }
        let terminal = Terminal::new(CrosstermBackend::new(stdout)).map_err(|error| {
            restore_terminal();
            RunError::Internal(format!("cannot initialize terminal: {error}"))
        })?;
        Ok(Self { terminal })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// Run the standalone interactive terminal frontend.
pub(super) async fn run(explicit_config: Option<&Path>) -> Result<RunOutcome, RunError> {
    let mut app = App::load(explicit_config).await?;
    #[cfg(unix)]
    let signals = TuiSignals::new()?;
    let mut terminal = TerminalGuard::enter()?;
    terminal
        .terminal
        .clear()
        .map_err(|error| RunError::Internal(format!("cannot clear terminal: {error}")))?;
    #[cfg(unix)]
    run_event_loop(&mut terminal.terminal, &mut app, signals).await?;
    #[cfg(not(unix))]
    run_event_loop(&mut terminal.terminal, &mut app).await?;
    Ok(RunOutcome::Success)
}

#[cfg(unix)]
struct TuiSignals {
    terminate: tokio::signal::unix::Signal,
    interrupt: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl TuiSignals {
    fn new() -> Result<Self, RunError> {
        use tokio::signal::unix::{signal, SignalKind};

        Ok(Self {
            terminate: signal(SignalKind::terminate())
                .map_err(|error| RunError::Internal(format!("cannot register SIGTERM: {error}")))?,
            interrupt: signal(SignalKind::interrupt())
                .map_err(|error| RunError::Internal(format!("cannot register SIGINT: {error}")))?,
        })
    }
}

#[cfg(unix)]
async fn run_event_loop(
    terminal: &mut CrosstermTerminal,
    app: &mut App,
    mut signals: TuiSignals,
) -> Result<(), RunError> {
    let mut events = EventStream::new();
    let mut refresh = tokio::time::interval(app.refresh_interval());
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    refresh.tick().await;
    let mut activity = tokio::time::interval(Duration::from_secs(1));
    activity.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    activity.tick().await;

    loop {
        draw(terminal, app)?;
        tokio::select! {
            _ = signals.terminate.recv() => break,
            _ = signals.interrupt.recv() => break,
            _ = refresh.tick(), if matches!(app.overlay, Overlay::None | Overlay::Help) => {
                if let Err(error) = app.refresh().await {
                    app.plan_validity = PlanValidity::Stale;
                    app.overlay = Overlay::None;
                    app.status = format!("Refresh failed: {}", super::run_error_message(&error));
                }
                refresh = tokio::time::interval(app.refresh_interval());
                refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                refresh.tick().await;
            }
            _ = activity.tick(), if matches!(app.overlay, Overlay::None | Overlay::Help) => {
                if let Err(error) = app.refresh_activity() {
                    app.status = format!("Activity refresh failed: {}", super::run_error_message(&error));
                }
            }
            event = events.next() => {
                let Some(event) = event else {
                    break;
                };
                let event = event.map_err(|error| RunError::Internal(format!("terminal input failed: {error}")))?;
                if handle_event(terminal, app, event).await? {
                    break;
                }
            }
        }
    }
    Ok(())
}

#[cfg(not(unix))]
async fn run_event_loop(terminal: &mut CrosstermTerminal, app: &mut App) -> Result<(), RunError> {
    let mut events = EventStream::new();
    loop {
        draw(terminal, app)?;
        let Some(event) = events.next().await else {
            break;
        };
        let event =
            event.map_err(|error| RunError::Internal(format!("terminal input failed: {error}")))?;
        if handle_event(terminal, app, event).await? {
            break;
        }
    }
    Ok(())
}

async fn handle_event(
    terminal: &mut CrosstermTerminal,
    app: &mut App,
    event: Event,
) -> Result<bool, RunError> {
    let Event::Key(key) = event else {
        return Ok(false);
    };
    if key.kind != crossterm::event::KeyEventKind::Press {
        return Ok(false);
    }
    if app.editor == Editor::Prefix {
        handle_prefix_key(app, key)?;
        return Ok(false);
    }
    if app.editor == Editor::Policy {
        handle_policy_key(app, key)?;
        return Ok(false);
    }
    if app.editor == Editor::Filter {
        handle_filter_key(app, key);
        return Ok(false);
    }
    if handle_overlay_key(terminal, app, key).await? {
        return Ok(false);
    }
    handle_main_key(app, key).await
}

async fn handle_main_key(app: &mut App, key: KeyEvent) -> Result<bool, RunError> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Ok(true);
    }
    match key.code {
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Char('?') => app.overlay = Overlay::Help,
        KeyCode::Char('1') => switch_view(app, View::Dashboard),
        KeyCode::Char('2') => switch_view(app, View::Inventory),
        KeyCode::Char('3') => switch_view(app, View::Plan),
        KeyCode::Char('4') => switch_view(app, View::Activity),
        KeyCode::Char('5') => switch_view(app, View::Configure),
        KeyCode::Down | KeyCode::Char('j') => app.move_selection(1),
        KeyCode::Up | KeyCode::Char('k') => app.move_selection(-1),
        KeyCode::Left | KeyCode::Char('h') if app.view == View::Inventory => {
            app.change_inventory_kind(-1);
        }
        KeyCode::Right | KeyCode::Char('l') if app.view == View::Inventory => {
            app.change_inventory_kind(1);
        }
        KeyCode::Left | KeyCode::Char('h') if app.view == View::Configure => {
            app.cycle_profile(-1);
        }
        KeyCode::Right | KeyCode::Char('l') if app.view == View::Configure => {
            app.cycle_profile(1);
        }
        KeyCode::Char('[') if app.view == View::Configure => app.cycle_policy_field(-1),
        KeyCode::Char(']') if app.view == View::Configure => app.cycle_policy_field(1),
        KeyCode::Char('e') if app.view == View::Configure => app.start_policy_editor(),
        KeyCode::Char('/') if app.view == View::Inventory => {
            app.editor = Editor::Filter;
            replace_status(
                &mut app.status,
                "Filter inventory; Enter accepts, Esc clears",
            );
        }
        KeyCode::Enter if app.view == View::Inventory => {
            app.detail_focused = !app.detail_focused;
        }
        KeyCode::Char('p') if app.view == View::Inventory => {
            app.toggle_protection().await?;
        }
        KeyCode::Char('c') if app.view == View::Inventory => app.start_prefix_editor(),
        KeyCode::Enter | KeyCode::Char(' ') if app.view == View::Configure => {
            app.toggle_selected_candidate();
        }
        KeyCode::Char('v') if app.view == View::Configure => {
            if let Err(error) = app.preview_configuration() {
                app.status = format!("Proposal blocked: {}", super::run_error_message(&error));
            }
        }
        KeyCode::Char('s') if app.view == View::Configure => {
            if app.config_proposal.is_some() {
                app.overlay = Overlay::ConfigSave;
            } else {
                replace_status(&mut app.status, "Preview the proposal with v before saving");
            }
        }
        KeyCode::Char('a') if matches!(app.view, View::Inventory | View::Plan) => {
            open_confirmation(app);
        }
        KeyCode::Char('y') if app.view == View::Plan => open_confirmation(app),
        KeyCode::Char('r') => {
            if let Err(error) = app.refresh().await {
                app.plan_validity = PlanValidity::Stale;
                app.status = format!("Refresh failed: {}", super::run_error_message(&error));
            }
        }
        KeyCode::PageDown => app.move_selection(10),
        KeyCode::PageUp => app.move_selection(-10),
        _ => {}
    }
    Ok(false)
}

async fn handle_overlay_key(
    terminal: &mut CrosstermTerminal,
    app: &mut App,
    key: KeyEvent,
) -> Result<bool, RunError> {
    match app.overlay {
        Overlay::None => return Ok(false),
        Overlay::Help => {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('?' | 'q')) {
                app.overlay = Overlay::None;
            }
        }
        Overlay::Confirm => handle_plan_confirmation(terminal, app, key).await?,
        Overlay::CacheConfirm => match key.code {
            KeyCode::Esc | KeyCode::Char('n') => {
                app.overlay = Overlay::None;
                replace_status(&mut app.status, "Build-cache policy not enabled");
            }
            KeyCode::Enter | KeyCode::Char('y') => {
                if let Some(candidate) = app.selected_candidate() {
                    app.configure_selected.insert(candidate.id.clone());
                    app.config_proposal = None;
                }
                app.overlay = Overlay::None;
                replace_status(
                    &mut app.status,
                    "Enabled authorized-unscoped build-cache proposal; preview before save",
                );
            }
            _ => {}
        },
        Overlay::ConfigSave => match key.code {
            KeyCode::Esc | KeyCode::Char('n') => {
                app.overlay = Overlay::None;
                replace_status(&mut app.status, "Configuration save cancelled");
            }
            KeyCode::Enter | KeyCode::Char('y') => {
                app.overlay = Overlay::None;
                replace_status(&mut app.status, "Writing validated configuration…");
                draw(terminal, app)?;
                if let Err(error) = app.save_configuration().await {
                    app.status = format!(
                        "Save blocked; refresh and review again: {}",
                        super::run_error_message(&error)
                    );
                }
            }
            _ => {}
        },
    }
    Ok(true)
}

async fn handle_plan_confirmation(
    terminal: &mut CrosstermTerminal,
    app: &mut App,
    key: KeyEvent,
) -> Result<(), RunError> {
    match key.code {
        KeyCode::Esc | KeyCode::Char('n') => {
            app.overlay = Overlay::None;
            replace_status(&mut app.status, "Plan application cancelled");
        }
        KeyCode::Enter => {
            app.overlay = Overlay::None;
            replace_status(&mut app.status, "Applying the confirmed immutable plan…");
            draw(terminal, app)?;
            app.apply_confirmed_plan().await?;
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.confirm_scroll = app
                .confirm_scroll
                .saturating_add(1)
                .min(app.plan.pending_count().saturating_sub(1));
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.confirm_scroll = app.confirm_scroll.saturating_sub(1);
        }
        KeyCode::PageDown => {
            app.confirm_scroll = app
                .confirm_scroll
                .saturating_add(10)
                .min(app.plan.pending_count().saturating_sub(1));
        }
        KeyCode::PageUp => {
            app.confirm_scroll = app.confirm_scroll.saturating_sub(10);
        }
        _ => {}
    }
    Ok(())
}

fn handle_filter_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            app.filter.clear();
            app.editor = Editor::None;
            app.selected = 0;
            replace_status(&mut app.status, "Filter cleared");
        }
        KeyCode::Enter => {
            app.editor = Editor::None;
            app.selected = 0;
            app.status = format!("Inventory filter: {:?}", app.filter);
        }
        KeyCode::Backspace => {
            app.filter.pop();
            app.selected = 0;
        }
        KeyCode::Char(character)
            if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
        {
            app.filter.push(character);
            app.selected = 0;
        }
        _ => {}
    }
}

fn handle_prefix_key(app: &mut App, key: KeyEvent) -> Result<(), RunError> {
    match key.code {
        KeyCode::Esc => {
            app.editor = Editor::None;
            app.prefix_input.clear();
            replace_status(&mut app.status, "Name-prefix candidate cancelled");
        }
        KeyCode::Enter => app.accept_prefix()?,
        KeyCode::Backspace => {
            app.prefix_input.pop();
        }
        KeyCode::Char(character)
            if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
        {
            app.prefix_input.push(character);
        }
        _ => {}
    }
    Ok(())
}

fn handle_policy_key(app: &mut App, key: KeyEvent) -> Result<(), RunError> {
    match key.code {
        KeyCode::Esc => {
            app.editor = Editor::None;
            app.prefix_input.clear();
            replace_status(&mut app.status, "Policy edit cancelled");
        }
        KeyCode::Enter => app.accept_policy_value()?,
        KeyCode::Backspace => {
            app.prefix_input.pop();
        }
        KeyCode::Char(character)
            if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
        {
            app.prefix_input.push(character);
        }
        _ => {}
    }
    Ok(())
}

fn open_confirmation(app: &mut App) {
    if app.plan_validity == PlanValidity::Stale {
        replace_status(
            &mut app.status,
            "Plan is stale because refresh failed; fix the error and press r",
        );
    } else if app.plan.has_pending_removals() {
        app.confirm_scroll = 0;
        app.overlay = Overlay::Confirm;
    } else {
        replace_status(&mut app.status, "No removals pending");
    }
}

fn switch_view(app: &mut App, view: View) {
    app.view = view;
    app.selected = 0;
    app.detail_focused = false;
}

fn draw(terminal: &mut CrosstermTerminal, app: &mut App) -> Result<(), RunError> {
    terminal
        .draw(|frame| render(frame, app))
        .map(|_| ())
        .map_err(|error| RunError::Internal(format!("cannot draw terminal: {error}")))
}

fn render(frame: &mut Frame<'_>, app: &mut App) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .split(frame.area());
    render_header(frame, app, areas[0]);
    render_tabs(frame, app, areas[1]);

    match app.view {
        View::Dashboard => render_dashboard(frame, app, areas[2]),
        View::Inventory => render_inventory(frame, app, areas[2]),
        View::Plan => render_plan(frame, app, areas[2]),
        View::Activity => render_activity(frame, app, areas[2]),
        View::Configure => render_configure(frame, app, areas[2]),
    }
    render_footer(frame, app, areas[3]);

    match app.overlay {
        Overlay::None => {}
        Overlay::Help => render_help(frame),
        Overlay::Confirm => render_confirmation(frame, app),
        Overlay::CacheConfirm => render_cache_confirmation(frame, app),
        Overlay::ConfigSave => render_config_save(frame, app),
    }
}

fn render_header(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let config = if app.built_in_config {
        "safe built-in config"
    } else {
        app.loaded
            .path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("config")
    };
    let mut spans = vec![
        Span::styled(
            " docker_maid ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            " standalone • {config} • {} resources • {} pending",
            app.plan.decisions.len(),
            app.plan.pending_count()
        )),
    ];
    if app.loaded.config.rules.build_cache.is_some() {
        spans.push(Span::styled(
            " • WARNING: authorized-unscoped build cache",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_tabs(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let titles = View::ALL
        .iter()
        .enumerate()
        .map(|(index, view)| Line::from(format!("[{}] {}", index + 1, view.title())))
        .collect::<Vec<_>>();
    let tabs = Tabs::new(titles)
        .select(app.view.index())
        .block(Block::default().borders(Borders::BOTTOM))
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .divider("  ");
    frame.render_widget(tabs, area);
}

fn render_dashboard(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(68), Constraint::Percentage(32)])
        .split(area);
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Min(7),
            Constraint::Length(7),
        ])
        .split(columns[0]);

    render_dashboard_gauges(frame, app, left[0]);
    render_disposition_table(frame, &app.plan, left[1]);
    render_reclaimed_sparkline(frame, &app.history, left[2]);
    frame.render_widget(
        Paragraph::new(dashboard_detail(app))
            .block(Block::default().borders(Borders::ALL).title("Detail"))
            .wrap(Wrap { trim: false }),
        columns[1],
    );
}

fn render_dashboard_gauges(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let known_bytes = app
        .plan
        .decisions
        .iter()
        .filter_map(|decision| decision.resource.size)
        .fold(0u64, u64::saturating_add);
    let pending_bytes = app
        .plan
        .decisions
        .iter()
        .filter(|decision| decision.action == Action::Remove)
        .filter_map(|decision| decision.resource.size)
        .fold(0u64, u64::saturating_add);
    let protected = disposition_count(&app.plan, Disposition::Protected);
    let gauges = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Length(2)])
        .split(area);
    frame.render_widget(
        Gauge::default()
            .block(Block::default().title("Known Docker inventory"))
            .gauge_style(Style::default().fg(Color::Yellow))
            .percent(percentage_u64(pending_bytes, known_bytes))
            .label(format!(
                "{} pending / {} known",
                format_bytes(pending_bytes),
                format_bytes(known_bytes)
            )),
        gauges[0],
    );
    frame.render_widget(
        Gauge::default()
            .block(Block::default().title("Protected"))
            .gauge_style(Style::default().fg(Color::Green))
            .percent(percentage_usize(protected, app.plan.decisions.len()))
            .label(format!("{protected} protected")),
        gauges[1],
    );
}

fn render_disposition_table(frame: &mut Frame<'_>, plan: &Plan, area: Rect) {
    let rows = resource_kinds()
        .into_iter()
        .map(|kind| {
            Row::new(vec![
                Cell::from(kind.to_string()),
                Cell::from(kind_disposition_count(plan, kind, Disposition::Protected).to_string()),
                Cell::from(kind_disposition_count(plan, kind, Disposition::Owned).to_string()),
                Cell::from(
                    kind_disposition_count(plan, kind, Disposition::AuthorizedUnscoped).to_string(),
                ),
                Cell::from(kind_disposition_count(plan, kind, Disposition::Unowned).to_string()),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Table::new(
            rows,
            [
                Constraint::Length(14),
                Constraint::Length(10),
                Constraint::Length(8),
                Constraint::Length(12),
                Constraint::Length(9),
            ],
        )
        .header(
            Row::new(["TYPE", "PROTECTED", "OWNED", "UNSCOPED", "UNOWNED"])
                .style(Style::default().add_modifier(Modifier::BOLD)),
        )
        .block(Block::default().borders(Borders::ALL).title("Dispositions")),
        area,
    );
}

fn render_reclaimed_sparkline(frame: &mut Frame<'_>, history: &[CompletedPass], area: Rect) {
    let spark_data = history
        .iter()
        .rev()
        .take(40)
        .map(|pass| pass.reclaimed_bytes)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>();
    frame.render_widget(
        Sparkline::default()
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Reclaimed bytes by completed pass"),
            )
            .data(&spark_data)
            .style(Style::default().fg(Color::Cyan)),
        area,
    );
}

fn dashboard_detail(app: &App) -> String {
    if let Some(last) = app.history.last() {
        format!(
            "Last completed pass\n\nSource: {}\nStarted: {}\nCompleted: {}\nRemoved: {}\nSkipped: {}\nFailed: {}\nReclaimed: {}\n\nRefresh: {}",
            last.source,
            last.started_at,
            last.completed_at,
            last.removed_count,
            last.skipped_count,
            last.failure_count,
            format_bytes(last.reclaimed_bytes),
            humantime::format_duration(app.refresh_interval())
        )
    } else {
        format!(
            "Last completed pass\n\nNone recorded.\n\nMode: standalone\nRefresh: {}\n\n{}",
            humantime::format_duration(app.refresh_interval()),
            if app.built_in_config {
                "Safe built-in mode has no removal rules.\nCreate one with:\ndocker_maid config default > docker_maid.toml"
            } else {
                "Configuration loaded."
            }
        )
    }
}

fn render_inventory(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(70), Constraint::Percentage(30)])
        .split(area);
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(4)])
        .split(columns[0]);
    let kinds = resource_kinds()
        .into_iter()
        .map(|kind| Line::from(kind.to_string()))
        .collect::<Vec<_>>();
    let kind_index = resource_kinds()
        .iter()
        .position(|kind| *kind == app.inventory_kind)
        .unwrap_or(0);
    frame.render_widget(
        Tabs::new(kinds)
            .select(kind_index)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Resource type"),
            )
            .highlight_style(Style::default().fg(Color::Cyan)),
        left[0],
    );

    let decisions = app.filtered_inventory();
    let rows = decisions
        .iter()
        .map(|decision| {
            Row::new(vec![
                Cell::from(decision.resource.name.clone()),
                Cell::from(decision.resource.state.to_string()),
                Cell::from(format_age(decision.age_seconds)),
                Cell::from(
                    decision
                        .resource
                        .size
                        .map_or_else(|| "unknown".to_owned(), format_bytes),
                ),
                Cell::from(decision.disposition.to_string()),
                Cell::from(
                    decision
                        .matched_rule
                        .clone()
                        .unwrap_or_else(|| "-".to_owned()),
                ),
            ])
            .style(disposition_style(decision.disposition))
        })
        .collect::<Vec<_>>();
    let title = if app.filter.is_empty() {
        format!("Inventory ({})", decisions.len())
    } else {
        format!("Inventory ({}) • filter={:?}", decisions.len(), app.filter)
    };
    let table = Table::new(
        rows,
        [
            Constraint::Percentage(25),
            Constraint::Length(10),
            Constraint::Length(9),
            Constraint::Length(10),
            Constraint::Length(20),
            Constraint::Percentage(25),
        ],
    )
    .header(
        Row::new(["NAME", "STATE", "AGE", "SIZE", "DISPOSITION", "RULE"])
            .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .block(Block::default().borders(Borders::ALL).title(title))
    .row_highlight_style(
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol("▶ ");
    let mut state = TableState::default().with_selected(Some(app.selected));
    frame.render_stateful_widget(table, left[1], &mut state);

    let detail = app
        .selected_inventory()
        .map_or_else(|| "No object selected".to_owned(), inventory_detail);
    let title = if app.detail_focused {
        "Detail • focused"
    } else {
        "Detail"
    };
    frame.render_widget(
        Paragraph::new(detail)
            .block(Block::default().borders(Borders::ALL).title(title))
            .wrap(Wrap { trim: false }),
        columns[1],
    );
}

fn render_plan(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(72), Constraint::Percentage(28)])
        .split(area);
    let targets = app.plan_targets();
    let rows = targets
        .iter()
        .map(|decision| {
            Row::new(vec![
                Cell::from(decision.resource.kind.to_string()),
                Cell::from(decision.resource.name.clone()),
                Cell::from(format_age(decision.age_seconds)),
                Cell::from(
                    decision
                        .resource
                        .size
                        .map_or_else(|| "unknown".to_owned(), format_bytes),
                ),
                Cell::from(
                    decision
                        .matched_rule
                        .clone()
                        .unwrap_or_else(|| "-".to_owned()),
                ),
                Cell::from(decision.disposition.to_string()),
            ])
        })
        .collect::<Vec<_>>();
    let table = Table::new(
        rows,
        [
            Constraint::Length(12),
            Constraint::Percentage(27),
            Constraint::Length(9),
            Constraint::Length(10),
            Constraint::Percentage(25),
            Constraint::Length(20),
        ],
    )
    .header(
        Row::new(["TYPE", "NAME", "AGE", "SIZE", "RULE", "DISPOSITION"])
            .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!("Immutable plan • {} targets", targets.len())),
    )
    .row_highlight_style(Style::default().bg(Color::DarkGray))
    .highlight_symbol("▶ ");
    let mut state = TableState::default().with_selected(Some(app.selected));
    frame.render_stateful_widget(table, columns[0], &mut state);

    let mut groups = BTreeMap::<String, (usize, u64)>::new();
    for decision in &targets {
        let entry = groups
            .entry(
                decision
                    .matched_rule
                    .clone()
                    .unwrap_or_else(|| "-".to_owned()),
            )
            .or_default();
        entry.0 += 1;
        entry.1 = entry
            .1
            .saturating_add(decision.resource.size.unwrap_or_default());
    }
    let mut detail = format!(
        "Plan summary\n\nID: {}\nCreated: {}\nConfig: {}\nValid: {}\nTargets: {}\nEstimated reclaim: {}\n\nBy rule:\n",
        app.plan_id,
        app.plan_created_at,
        app.config_hash,
        app.plan_validity == PlanValidity::Valid,
        targets.len(),
        format_bytes(
            targets
                .iter()
                .filter_map(|decision| decision.resource.size)
                .fold(0u64, u64::saturating_add)
        )
    );
    for (rule, (count, bytes)) in groups {
        let _ = writeln!(detail, "• {rule}: {count} / {}", format_bytes(bytes));
    }
    if targets.is_empty() {
        detail.push_str("\nNo removals pending.");
    } else {
        detail.push_str("\nPress y or a to review the exact target set.");
    }
    frame.render_widget(
        Paragraph::new(detail)
            .block(Block::default().borders(Borders::ALL).title("Detail"))
            .wrap(Wrap { trim: false }),
        columns[1],
    );
}

fn render_activity(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let mut items = Vec::new();
    for pass in app.history.iter().rev() {
        items.push(ListItem::new(Line::from(vec![
            Span::styled(
                format!("{} ", pass.completed_at),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                format!("{} pass", pass.source),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(
                " • removed={} skipped={} failed={} reclaimed={}",
                pass.removed_count,
                pass.skipped_count,
                pass.failure_count,
                format_bytes(pass.reclaimed_bytes)
            )),
        ])));
        for event in &pass.actions {
            if let EventData::Action {
                action,
                resource_kind,
                resource_name,
                matched_rule,
                freed_bytes,
                ..
            } = &event.data
            {
                items.push(ListItem::new(format!(
                    "  {action:8} {resource_kind:12} {resource_name} • {matched_rule} • {}",
                    format_bytes(*freed_bytes)
                )));
            }
        }
    }
    if items.is_empty() {
        items.push(ListItem::new("No completed cleanup passes recorded."));
    }
    let items = items
        .into_iter()
        .skip(usize::from(app.activity_scroll))
        .collect::<Vec<_>>();
    frame.render_widget(
        List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!("Activity • {} completed passes", app.history.len())),
        ),
        area,
    );
}

fn render_configure(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(61), Constraint::Percentage(39)])
        .split(area);
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(4), Constraint::Min(5)])
        .split(columns[0]);
    render_configure_header(frame, app, left[0]);
    render_configure_candidates(frame, app, left[1]);
    frame.render_widget(
        Paragraph::new(configure_detail(app))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Policy and before/after preview"),
            )
            .scroll((app.rules_scroll, 0))
            .wrap(Wrap { trim: false }),
        columns[1],
    );
}

fn render_configure_header(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let current_stage = if app.config_proposal.is_some() {
        "Preview ready • s saves"
    } else if app.configure_selected.is_empty() {
        "Select ownership candidates"
    } else {
        "Press v to build the real plan preview"
    };
    frame.render_widget(
        Paragraph::new(format!(
            "Profile: {} [h/l] • TTL c={} i={} v={} • cache={}/{}\nSelected: {} of {} • {current_stage} • [{}] field, e edit",
            app.configure_profile.title(),
            app.configure_policy.stopped_container_ttl,
            app.configure_policy.image_ttl,
            app.configure_policy.volume_ttl,
            app.configure_policy.build_cache_ttl,
            format_bytes(app.configure_policy.build_cache_max_bytes),
            app.configure_selected.len(),
            app.survey.candidates.len(),
            PolicyField::ALL[app.policy_field].title()
        ))
        .block(Block::default().borders(Borders::ALL).title(format!(
            "Survey {} • {} unowned",
            app.survey.snapshot_id, app.survey.summary.unowned_resources
        ))),
        area,
    );
}

fn render_configure_candidates(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let rows = app
        .survey
        .candidates
        .iter()
        .map(|candidate| {
            let selected = if app.configure_selected.contains(&candidate.id) {
                "[x]"
            } else {
                "[ ]"
            };
            let style = if candidate.warning.is_some() {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default()
            };
            Row::new(vec![
                Cell::from(selected),
                Cell::from(candidate.title.clone()),
                Cell::from(candidate.resources.len().to_string()),
                Cell::from(format_bytes(candidate.known_bytes)),
            ])
            .style(style)
        })
        .collect::<Vec<_>>();
    let table = Table::new(
        rows,
        [
            Constraint::Length(4),
            Constraint::Percentage(62),
            Constraint::Length(10),
            Constraint::Length(12),
        ],
    )
    .header(
        Row::new(["USE", "OWNERSHIP EVIDENCE", "OBJECTS", "KNOWN SIZE"])
            .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title("Candidates • Space/Enter toggles"),
    )
    .row_highlight_style(Style::default().bg(Color::DarkGray))
    .highlight_symbol("▶ ");
    let mut table_state = TableState::default().with_selected(Some(app.configure_row));
    frame.render_stateful_widget(table, area, &mut table_state);
}

fn configure_detail(app: &App) -> String {
    if let Some(proposal) = &app.config_proposal {
        let warnings = if proposal.warnings.is_empty() {
            "none".to_owned()
        } else {
            proposal.warnings.join("\n")
        };
        format!(
            "Proposal {}\n\nTarget: {}\nProfile: {}\nCandidates: {}\nSelected objects: {}\n\nPending before: {}\nPending after: {}\nNewly pending: {}\nEstimated reclaim: {}\n\nWarnings:\n{}\n\nThe save changes configuration only. It never deletes Docker objects. After save, the Plan view refreshes and apply remains a separate confirmation.",
            proposal.proposal_id,
            proposal.target_path.display(),
            proposal.profile,
            proposal.candidate_ids.len(),
            proposal.preview.selected_resources,
            proposal.preview.before_pending,
            proposal.preview.after_pending,
            proposal.preview.newly_pending,
            format_bytes(proposal.preview.estimated_reclaim_bytes),
            warnings
        )
    } else if let Some(candidate) = app.selected_candidate() {
        let mut detail = format!(
            "{}\n\nID: {}\nEvidence: {}\nObjects: {}\nKnown size: {}\n",
            candidate.title,
            candidate.id,
            candidate.evidence,
            candidate.resources.len(),
            format_bytes(candidate.known_bytes)
        );
        if let Some(warning) = &candidate.warning {
            let _ = writeln!(detail, "\n{warning}");
            detail.push_str("Enable only when daemon-wide cache cleanup is intentional.\n");
        }
        detail.push_str("\nObserved objects:\n");
        for resource in candidate.resources.iter().take(12) {
            let _ = writeln!(
                detail,
                "• {} {}{}{}",
                resource.resource_kind,
                resource.name,
                if resource.running { " • running" } else { "" },
                if resource.referenced {
                    " • referenced"
                } else {
                    ""
                }
            );
        }
        if candidate.resources.len() > 12 {
            let _ = writeln!(detail, "• … {} more", candidate.resources.len() - 12);
        }
        detail
    } else {
        "No exact agent or Compose ownership evidence was found.\n\nUnlabeled objects remain unowned. In Inventory, select an object and press c to explicitly approve a name prefix."
            .to_owned()
    }
}

fn render_footer(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let status = if app.editor == Editor::Prefix {
        format!("prefix={}▌", app.prefix_input)
    } else if app.editor == Editor::Policy {
        format!(
            "{}={}▌",
            PolicyField::ALL[app.policy_field].title(),
            app.prefix_input
        )
    } else if app.editor == Editor::Filter {
        format!("/{}▌", app.filter)
    } else {
        app.status.clone()
    };
    let line = Line::from(vec![
        Span::styled(
            " ↑↓/jk move · 1–5 views · c prefix · p protect · h/l profile · [/] field · e edit · v preview · s save · a apply · q quit ",
            Style::default().fg(Color::Black).bg(Color::Cyan),
        ),
        Span::raw(format!("  {status}")),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn render_help(frame: &mut Frame<'_>) {
    let area = centered_rect(70, 70, frame.area());
    frame.render_widget(Clear, area);
    let help = Text::from(vec![
        Line::from("docker_maid keyboard help"),
        Line::from(""),
        Line::from("1–5        Switch views"),
        Line::from("↑/↓ or j/k  Move selection or scroll"),
        Line::from("←/→ or h/l  Change inventory resource type"),
        Line::from("/           Filter inventory; Enter accepts; Esc clears"),
        Line::from("Enter       Focus inventory detail"),
        Line::from("p           Toggle typed runtime protection"),
        Line::from("c           Create an explicit name-prefix candidate from Inventory"),
        Line::from("Space/Enter Select a Configure ownership candidate"),
        Line::from("h/l         Change the Configure policy profile"),
        Line::from("[/] then e  Select and edit a profile value"),
        Line::from("v           Preview the exact proposed config and removal plan"),
        Line::from("s           Save a reviewed proposal (configuration only)"),
        Line::from("a or y      Review the policy-generated plan"),
        Line::from("r           Refresh configuration, state, and Docker inventory"),
        Line::from("? or Esc    Close help"),
        Line::from("q           Quit"),
        Line::from(""),
        Line::from("No key deletes one object. Apply can only execute the confirmed plan."),
    ]);
    frame.render_widget(
        Paragraph::new(help)
            .block(Block::default().borders(Borders::ALL).title("Help"))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_cache_confirmation(frame: &mut Frame<'_>, app: &App) {
    let area = centered_rect(70, 45, frame.area());
    frame.render_widget(Clear, area);
    let count = app
        .selected_candidate()
        .map_or(0, |candidate| candidate.resources.len());
    let text = format!(
        "Build cache is not attributable to an agent, project, label, or name.\n\nThis enables an authorized-unscoped proposal for {count} current cache records. The {} profile suggests an age floor and byte budget.\n\nNo deletion happens here. The next stage shows the exact plan.\n\nEnter/y: enable candidate    Esc/n: cancel",
        app.configure_profile.title()
    );
    frame.render_widget(
        Paragraph::new(text)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("WARNING • daemon-wide build cache"),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_config_save(frame: &mut Frame<'_>, app: &App) {
    let area = centered_rect(76, 58, frame.area());
    frame.render_widget(Clear, area);
    let text = app.config_proposal.as_ref().map_or_else(
        || "No proposal is available.".to_owned(),
        |proposal| {
            format!(
                "Write reviewed configuration?\n\nPath: {}\nProposal: {}\nProfile: {}\nSelected objects: {}\nPending removals after save: {}\nNewly pending: {}\nEstimated reclaim: {}\n\nThe writer checks the source hash and Docker inventory again. Existing config is backed up. Manual rules and comments remain outside the managed region.\n\nThis action does not delete Docker objects.\n\nEnter/y: save    Esc/n: cancel",
                proposal.target_path.display(),
                proposal.proposal_id,
                proposal.profile,
                proposal.preview.selected_resources,
                proposal.preview.after_pending,
                proposal.preview.newly_pending,
                format_bytes(proposal.preview.estimated_reclaim_bytes)
            )
        },
    );
    frame.render_widget(
        Paragraph::new(text)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Confirm configuration write"),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_confirmation(frame: &mut Frame<'_>, app: &App) {
    let area = centered_rect(78, 78, frame.area());
    frame.render_widget(Clear, area);
    let targets = app.plan_targets();
    let inner_height = usize::from(area.height.saturating_sub(11));
    let mut lines = vec![
        Line::from(Span::styled(
            format!("Apply this immutable {}-target plan?", targets.len()),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(format!("Plan ID: {}", app.plan_id)),
        Line::from(format!(
            "Created: {} • config: {}",
            app.plan_created_at, app.config_hash
        )),
        Line::from(""),
    ];
    let authorized_unscoped = targets
        .iter()
        .filter(|decision| decision.disposition == Disposition::AuthorizedUnscoped)
        .count();
    if authorized_unscoped != 0 {
        lines.push(Line::from(Span::styled(
            format!("WARNING: {authorized_unscoped} target(s) are authorized-unscoped"),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )));
    }
    for decision in targets.iter().skip(app.confirm_scroll).take(inner_height) {
        lines.push(Line::from(format!(
            "{}  {}  [{}]",
            decision.resource.kind,
            decision.resource.name,
            decision
                .matched_rule
                .as_deref()
                .unwrap_or("no matched rule")
        )));
    }
    let shown_end = app
        .confirm_scroll
        .saturating_add(inner_height)
        .min(targets.len());
    if targets.len() > inner_height {
        lines.push(Line::from(format!(
            "Showing {}–{} of {} • j/k or PgUp/PgDn scroll",
            app.confirm_scroll.saturating_add(1),
            shown_end,
            targets.len()
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(
        "Enter confirms this exact target set. Esc or n cancels.",
    ));
    lines.push(Line::from(
        "Every target is revalidated immediately before deletion; no target can be added.",
    ));
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Yellow))
                    .title("Confirm plan application"),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

fn policy_field_value(field: PolicyField, policy: &PolicySettings) -> String {
    match field {
        PolicyField::Containers => policy.stopped_container_ttl.clone(),
        PolicyField::Images => policy.image_ttl.clone(),
        PolicyField::Volumes => policy.volume_ttl.clone(),
        PolicyField::CacheAge => policy.build_cache_ttl.clone(),
        PolicyField::CacheBytes => policy.build_cache_max_bytes.to_string(),
    }
}

fn set_policy_field(
    field: PolicyField,
    policy: &mut PolicySettings,
    value: &str,
) -> Result<(), ConfiguratorError> {
    if value.is_empty() {
        return Err(ConfiguratorError::Invalid(
            "policy value must not be blank".to_owned(),
        ));
    }
    match field {
        PolicyField::Containers => value.clone_into(&mut policy.stopped_container_ttl),
        PolicyField::Images => value.clone_into(&mut policy.image_ttl),
        PolicyField::Volumes => value.clone_into(&mut policy.volume_ttl),
        PolicyField::CacheAge => value.clone_into(&mut policy.build_cache_ttl),
        PolicyField::CacheBytes => policy.build_cache_max_bytes = parse_byte_budget(value)?,
    }
    Ok(())
}

fn parse_byte_budget(value: &str) -> Result<u64, ConfiguratorError> {
    let compact = value.trim().replace('_', "");
    let lower = compact.to_ascii_lowercase();
    let suffixes = [
        ("gib", 1024u64.pow(3)),
        ("mib", 1024u64.pow(2)),
        ("kib", 1024),
        ("gb", 1_000_000_000),
        ("mb", 1_000_000),
        ("kb", 1_000),
    ];
    let (number, multiplier) = suffixes
        .iter()
        .find_map(|(suffix, multiplier)| {
            lower
                .strip_suffix(suffix)
                .map(|number| (number.trim(), *multiplier))
        })
        .unwrap_or((lower.as_str(), 1));
    let number = number.parse::<u64>().map_err(|error| {
        ConfiguratorError::Invalid(format!(
            "byte budget {value:?} is invalid; use bytes or a suffix such as 10GiB: {error}"
        ))
    })?;
    number
        .checked_mul(multiplier)
        .ok_or_else(|| ConfiguratorError::Invalid(format!("byte budget {value:?} is too large")))
}

fn load_tui_config(explicit: Option<&Path>) -> Result<(LoadedConfig, bool), RunError> {
    match load_selected_config(explicit) {
        Ok(loaded) => Ok((loaded, false)),
        Err(RunError::Config(ConfigError::NotFound { .. })) if explicit.is_none() => {
            let path = configuration_target_path(
                None,
                None,
                std::env::var_os("XDG_CONFIG_HOME")
                    .filter(|value| !value.is_empty())
                    .as_deref()
                    .map(Path::new),
                std::env::var_os("HOME")
                    .filter(|value| !value.is_empty())
                    .as_deref()
                    .map(Path::new),
            )?;
            let config = Config::default();
            config.validate()?;
            Ok((
                LoadedConfig {
                    path,
                    config,
                    source: String::new(),
                },
                true,
            ))
        }
        Err(RunError::Config(ConfigError::Read { path, source }))
            if explicit.is_some() && source.kind() == io::ErrorKind::NotFound =>
        {
            let config = Config::default();
            config.validate()?;
            Ok((
                LoadedConfig {
                    path,
                    config,
                    source: String::new(),
                },
                true,
            ))
        }
        Err(error) => Err(error),
    }
}

fn startup_status(loaded: &LoadedConfig, built_in_config: bool, plan: &Plan) -> String {
    if built_in_config {
        format!(
            "No config found • Configure opened • proposed writes target {}",
            loaded.path.display()
        )
    } else if loaded.config.rules.build_cache.is_some() {
        format!(
            "Loaded {} • WARNING: build cache is authorized-unscoped",
            loaded.path.display()
        )
    } else {
        format!(
            "Loaded {} • {} pending removals",
            loaded.path.display(),
            plan.pending_count()
        )
    }
}

fn managed_candidate_ids(config: &Config, survey: &ConfiguratorSurvey) -> BTreeSet<String> {
    let mut ids = config
        .rules
        .containers
        .iter()
        .map(|rule| rule.common.id.as_deref())
        .chain(
            config
                .rules
                .images
                .iter()
                .map(|rule| rule.common.id.as_deref()),
        )
        .chain(
            config
                .rules
                .volumes
                .iter()
                .map(|rule| rule.common.id.as_deref()),
        )
        .chain(
            config
                .rules
                .networks
                .iter()
                .map(|rule| rule.common.id.as_deref()),
        )
        .flatten()
        .filter_map(|id| id.strip_prefix(MANAGED_ID_PREFIX))
        .filter_map(|suffix| suffix.rsplit_once('/').map(|(candidate, _)| candidate))
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    if config
        .rules
        .build_cache
        .as_ref()
        .and_then(|rule| rule.id.as_deref())
        == Some("docker-maid.configure/build-cache")
    {
        ids.insert("build-cache".to_owned());
    }
    ids.retain(|id| {
        survey
            .candidates
            .iter()
            .any(|candidate| &candidate.id == id)
    });
    ids
}

fn execution_status(report: &ExecutionReport) -> String {
    let removed = report
        .outcomes
        .iter()
        .filter(|outcome| outcome.status == TargetStatus::Removed)
        .count();
    let skipped = report
        .outcomes
        .iter()
        .filter(|outcome| outcome.status == TargetStatus::Skipped)
        .count();
    let failed = report
        .outcomes
        .iter()
        .filter(|outcome| outcome.status == TargetStatus::Failed)
        .count();
    format!("Apply complete: removed {removed}, skipped {skipped}, failed {failed}")
}

fn protection_kind(kind: ResourceKind) -> Option<ProtectionKind> {
    match kind {
        ResourceKind::Container => Some(ProtectionKind::Container),
        ResourceKind::Image => Some(ProtectionKind::Image),
        ResourceKind::Volume => Some(ProtectionKind::Volume),
        ResourceKind::Network => Some(ProtectionKind::Network),
        ResourceKind::BuildCache => None,
    }
}

fn fuzzy_match(haystack: &str, needle: &str) -> bool {
    let mut wanted = needle.chars().flat_map(char::to_lowercase);
    let mut current = wanted.next();
    if current.is_none() {
        return true;
    }
    for candidate in haystack.chars().flat_map(char::to_lowercase) {
        if current == Some(candidate) {
            current = wanted.next();
            if current.is_none() {
                return true;
            }
        }
    }
    false
}

fn inventory_detail(decision: &Decision) -> String {
    let mut detail = format!(
        "{}\n\nType: {}\nID: {}\nState: {}\nAge: {}\nSize: {}\nDisposition: {}\nRule: {}\nAction: {}\nReferenced: {}\nDangling: {}\nSystem: {}\n\nWhy:\n{}",
        decision.resource.name,
        decision.resource.kind,
        decision.resource.id,
        decision.resource.state,
        format_age(decision.age_seconds),
        decision
            .resource
            .size
            .map_or_else(|| "unknown".to_owned(), format_bytes),
        decision.disposition,
        decision.matched_rule.as_deref().unwrap_or("-"),
        decision.action,
        decision.resource.referenced,
        decision.resource.dangling,
        decision.resource.system,
        decision.reason,
    );
    if !decision.resource.labels.is_empty() {
        detail.push_str("\n\nLabels:\n");
        for (key, value) in &decision.resource.labels {
            let _ = writeln!(detail, "{key}={value}");
        }
    }
    if !decision.resource.mounts.is_empty() {
        detail.push_str("\nMounts:\n");
        for mount in &decision.resource.mounts {
            let _ = writeln!(detail, "{mount}");
        }
    }
    detail
}

fn disposition_style(disposition: Disposition) -> Style {
    match disposition {
        Disposition::Protected => Style::default().fg(Color::Green),
        Disposition::Owned => Style::default().fg(Color::Cyan),
        Disposition::AuthorizedUnscoped => Style::default().fg(Color::Yellow),
        Disposition::Unowned => Style::default().fg(Color::DarkGray),
    }
}

fn disposition_count(plan: &Plan, disposition: Disposition) -> usize {
    plan.decisions
        .iter()
        .filter(|decision| decision.disposition == disposition)
        .count()
}

fn kind_disposition_count(plan: &Plan, kind: ResourceKind, disposition: Disposition) -> usize {
    plan.decisions
        .iter()
        .filter(|decision| decision.resource.kind == kind && decision.disposition == disposition)
        .count()
}

fn resource_kinds() -> [ResourceKind; 5] {
    [
        ResourceKind::Container,
        ResourceKind::Image,
        ResourceKind::Volume,
        ResourceKind::Network,
        ResourceKind::BuildCache,
    ]
}

fn format_age(seconds: Option<u64>) -> String {
    seconds.map_or_else(
        || "unknown".to_owned(),
        |value| humantime::format_duration(Duration::from_secs(value)).to_string(),
    )
}

fn tui_plan_id(config_hash: &str, created_at: i64, plan: &Plan) -> String {
    let mut identity = format!("{config_hash}:{created_at}");
    for decision in plan
        .decisions
        .iter()
        .filter(|decision| decision.action == Action::Remove)
    {
        let _ = write!(
            identity,
            ":{}:{}",
            decision.resource.kind, decision.resource.id
        );
    }
    stable_config_hash(&identity)
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut divisor = 1u64;
    let mut unit = 0usize;
    while bytes / divisor >= 1024 && unit + 1 < UNITS.len() {
        divisor = divisor.saturating_mul(1024);
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        let whole = bytes / divisor;
        let fraction = bytes % divisor * 10 / divisor;
        format!("{whole}.{fraction} {}", UNITS[unit])
    }
}

fn percentage_u64(numerator: u64, denominator: u64) -> u16 {
    if denominator == 0 {
        return 0;
    }
    let value = u128::from(numerator)
        .saturating_mul(100)
        .checked_div(u128::from(denominator))
        .unwrap_or_default()
        .min(100);
    u16::try_from(value).unwrap_or(100)
}

fn percentage_usize(numerator: usize, denominator: usize) -> u16 {
    percentage_u64(
        u64::try_from(numerator).unwrap_or(u64::MAX),
        u64::try_from(denominator).unwrap_or(u64::MAX),
    )
}

fn move_scroll(current: u16, delta: isize) -> u16 {
    let magnitude = u16::try_from(delta.unsigned_abs()).unwrap_or(u16::MAX);
    if delta.is_negative() {
        current.saturating_sub(magnitude)
    } else {
        current.saturating_add(magnitude)
    }
}

fn replace_status(status: &mut String, message: &str) {
    status.clear();
    status.push_str(message);
}

fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        previous(info);
    }));
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let mut stdout = io::stdout();
    let _ = execute!(stdout, Show, LeaveAlternateScreen);
}

#[cfg(test)]
mod tests {
    use super::*;
    use docker_maid::plan::{InventoryItem, ResourceState};
    use ratatui::backend::TestBackend;

    #[test]
    fn fuzzy_filter_matches_subsequences_case_insensitively() {
        assert!(fuzzy_match("Agent-Sandbox-123", "as13"));
        assert!(fuzzy_match("postgres", "PG"));
        assert!(!fuzzy_match("volume", "network"));
    }

    #[test]
    fn build_cache_has_no_typed_protection_kind() {
        assert_eq!(protection_kind(ResourceKind::BuildCache), None);
        assert_eq!(
            protection_kind(ResourceKind::Container),
            Some(ProtectionKind::Container)
        );
    }

    #[test]
    fn editable_cache_budget_accepts_binary_suffixes_and_rejects_garbage() {
        assert_eq!(
            parse_byte_budget("10GiB").expect("budget"),
            10 * 1024u64.pow(3)
        );
        assert_eq!(parse_byte_budget("500 MB").expect("budget"), 500_000_000);
        assert!(parse_byte_budget("many").is_err());
    }

    #[test]
    fn inventory_detail_exposes_policy_reason_and_labels() {
        let decision = sample_decision();
        let detail = inventory_detail(&decision);
        assert!(detail.contains("matched agent label"));
        assert!(detail.contains("ai-agent.owner=test"));
        assert!(detail.contains("agents"));
        assert!(detail.contains("workspace → /workspace"));
    }

    #[test]
    fn plan_identity_is_stable_and_binds_the_exact_target_set() {
        let plan = Plan {
            decisions: vec![sample_decision()],
        };
        let identity = tui_plan_id("config", 100, &plan);
        assert_eq!(identity, tui_plan_id("config", 100, &plan));

        let empty = Plan {
            decisions: Vec::new(),
        };
        assert_ne!(identity, tui_plan_id("config", 100, &empty));
        assert_ne!(identity, tui_plan_id("other-config", 100, &plan));
    }

    #[tokio::test]
    async fn changed_configuration_invalidates_confirmation_before_docker_or_journal() {
        let root =
            std::env::temp_dir().join(format!("docker-maid-tui-stale-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("create test root");
        let config_path = root.join("docker_maid.toml");
        let source = "[[rules.networks]]\nname='agents'\norphan=true\nselect.names=['^agent-']\n";
        std::fs::write(&config_path, source).expect("write initial config");
        let config = Config::parse(source, &config_path).expect("parse initial config");
        config.validate().expect("validate initial config");
        let paths = StatePaths::new(root.join("state"));
        let plan = Plan {
            decisions: vec![sample_decision()],
        };
        let survey = survey_inventory(
            &plan
                .decisions
                .iter()
                .map(|decision| decision.resource.clone())
                .collect::<Vec<_>>(),
        );
        let mut app = App {
            explicit_config: Some(config_path.clone()),
            loaded: LoadedConfig {
                path: config_path.clone(),
                config,
                source: source.to_owned(),
            },
            built_in_config: false,
            state_paths: paths.clone(),
            protection_store: ProtectionStore::new(paths.clone()),
            plan_id: tui_plan_id("config", 1, &plan),
            plan,
            plan_created_at: 1,
            config_hash: "config".to_owned(),
            plan_validity: PlanValidity::Valid,
            history: Vec::new(),
            view: View::Plan,
            inventory_kind: ResourceKind::Network,
            selected: 0,
            filter: String::new(),
            editor: Editor::None,
            detail_focused: false,
            overlay: Overlay::None,
            confirm_scroll: 0,
            activity_scroll: 0,
            rules_scroll: 0,
            survey,
            configure_selected: BTreeSet::new(),
            configure_row: 0,
            configure_profile: PolicyProfile::Workstation,
            configure_policy: PolicyProfile::Workstation.settings(),
            policy_field: 0,
            config_proposal: None,
            prefix_input: String::new(),
            status: String::new(),
        };
        std::fs::write(&config_path, format!("{source}# changed\n")).expect("change config");

        app.apply_confirmed_plan()
            .await
            .expect("stale plan is handled in the TUI");

        assert_eq!(app.plan_validity, PlanValidity::Stale);
        assert!(app.status.contains("Configuration changed"));
        assert!(!paths.activity_file().exists());
        std::fs::remove_file(config_path).expect("remove config");
        std::fs::remove_dir(root).expect("remove test root");
    }

    #[test]
    fn every_view_and_safety_overlay_renders() {
        let root =
            std::env::temp_dir().join(format!("docker-maid-tui-render-{}", std::process::id()));
        let paths = StatePaths::new(root);
        let plan = Plan {
            decisions: vec![sample_decision()],
        };
        let survey = survey_inventory(
            &plan
                .decisions
                .iter()
                .map(|decision| decision.resource.clone())
                .collect::<Vec<_>>(),
        );
        let mut app = App {
            explicit_config: None,
            loaded: LoadedConfig {
                path: PathBuf::from("docker_maid.toml"),
                config: Config::default(),
                source: "# test config".to_owned(),
            },
            built_in_config: false,
            state_paths: paths.clone(),
            protection_store: ProtectionStore::new(paths),
            plan,
            plan_id: "test-plan".to_owned(),
            plan_created_at: 1,
            config_hash: "test-config".to_owned(),
            plan_validity: PlanValidity::Valid,
            history: Vec::new(),
            view: View::Dashboard,
            inventory_kind: ResourceKind::Container,
            selected: 0,
            filter: String::new(),
            editor: Editor::None,
            detail_focused: false,
            overlay: Overlay::None,
            confirm_scroll: 0,
            activity_scroll: 0,
            rules_scroll: 0,
            survey,
            configure_selected: BTreeSet::new(),
            configure_row: 0,
            configure_profile: PolicyProfile::Workstation,
            configure_policy: PolicyProfile::Workstation.settings(),
            policy_field: 0,
            config_proposal: None,
            prefix_input: String::new(),
            status: "ready".to_owned(),
        };
        let backend = TestBackend::new(140, 42);
        let mut terminal = Terminal::new(backend).expect("test terminal");

        for view in View::ALL {
            app.view = view;
            terminal
                .draw(|frame| render(frame, &mut app))
                .expect("render view");
            let rendered = rendered_text(&terminal);
            assert!(rendered.contains(view.title()), "missing {view:?}");
            assert!(rendered.contains("docker_maid"));
        }

        app.view = View::Plan;
        app.overlay = Overlay::Confirm;
        terminal
            .draw(|frame| render(frame, &mut app))
            .expect("render confirmation");
        assert!(rendered_text(&terminal).contains("Confirm plan application"));

        app.overlay = Overlay::Help;
        terminal
            .draw(|frame| render(frame, &mut app))
            .expect("render help");
        assert!(rendered_text(&terminal).contains("keyboard help"));
    }

    fn rendered_text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    fn sample_decision() -> Decision {
        Decision {
            resource: InventoryItem {
                kind: ResourceKind::Container,
                id: "abc".to_owned(),
                name: "agent-box".to_owned(),
                search_names: vec!["agent-box".to_owned()],
                parent_ids: Vec::new(),
                labels: BTreeMap::from([("ai-agent.owner".to_owned(), "test".to_owned())]),
                mounts: vec!["workspace → /workspace (volume, rw)".to_owned()],
                state: ResourceState::Stopped,
                created_at: Some(1),
                state_since: Some(2),
                size: Some(1024),
                referenced: false,
                dangling: false,
                system: false,
            },
            disposition: Disposition::Owned,
            matched_rule: Some("agents".to_owned()),
            action: Action::Remove,
            age_seconds: Some(90),
            reason: "matched agent label".to_owned(),
        }
    }
}
