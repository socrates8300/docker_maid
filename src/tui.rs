//! Interactive terminal frontend over the shared inventory, policy, state, and executor core.
//!
//! The interface answers one question a Docker GUI cannot: what would this
//! policy remove, and why. `Review` is that answer, `Keeping` is its complement
//! and the place protection is applied, and `Setup` is the guided configurator.
//!
//! Two rules hold everywhere in this module. Colour never carries meaning on
//! its own, so every coloured row also sits under a heading that says the same
//! thing in words. And the footer is generated from the same table the key
//! handler reads, so it cannot advertise a key that does nothing.

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
    add_name_prefix_candidate, candidate_display_indices, configuration_target_path, family_label,
    propose_configuration, refresh_candidate_warnings, survey_inventory, write_proposal,
    CandidateSelector, ConfigProposal, ConfiguratorError, ConfiguratorSurvey, PolicyProfile,
    PolicySettings, ProposalRequest, MANAGED_ID_PREFIX,
};
use docker_maid::executor::{execute_plan, ExecutionReport, TargetStatus};
use docker_maid::inventory::collect_inventory_for_configuration;
use docker_maid::observation::{ObservationState, ObservationStore};
use docker_maid::plan::{
    build_plan_with_context, Action, Decision, Disposition, InventoryItem, Plan, PlanContext,
    ResourceKind,
};
use docker_maid::state::{ProtectionKind, ProtectionState, ProtectionStore, StatePaths};
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState, Wrap,
};
use ratatui::{Frame, Terminal};
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::time::Duration;

type CrosstermTerminal = Terminal<CrosstermBackend<Stdout>>;

/// The narrowest terminal this interface will draw into.
const MIN_WIDTH: u16 = 60;
/// The shortest terminal this interface will draw into.
const MIN_HEIGHT: u16 = 20;

/// Would be removed. This colour means nothing else anywhere in the interface.
const REMOVE_COLOR: Color = Color::Red;
/// Protected. This colour means nothing else anywhere in the interface.
const PROTECT_COLOR: Color = Color::Green;
/// Needs a human decision: an unscoped authorization, or a warning.
const ATTENTION_COLOR: Color = Color::Yellow;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    Review,
    Keeping,
    Setup,
}

impl View {
    const ALL: [Self; 3] = [Self::Review, Self::Keeping, Self::Setup];

    const fn index(self) -> usize {
        match self {
            Self::Review => 0,
            Self::Keeping => 1,
            Self::Setup => 2,
        }
    }

    const fn title(self) -> &'static str {
        match self {
            Self::Review => "Review",
            Self::Keeping => "Keeping",
            Self::Setup => "Setup",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Overlay {
    None,
    Help,
    Activity,
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

/// What a key press means, decided before anything is executed.
///
/// Splitting the meaning from the effect is what lets one test prove that every
/// key the footer advertises resolves to something, without running the effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Intent {
    Quit,
    Help,
    Show(View),
    ActivityLog,
    Move(isize),
    ScrollPreview(isize),
    ToggleDetail,
    StartFilter,
    Protect,
    ProtectFamily,
    NamePrefix,
    Apply,
    Refresh,
    CycleProfile(isize),
    CyclePolicyField(isize),
    EditPolicy,
    ToggleCandidate,
    PreviewProposal,
    SaveProposal,
}

/// One entry in a view's key table.
///
/// The footer prints the key codes themselves, so there is no second string to
/// drift out of step with what the handler dispatches.
struct KeyHint {
    label: &'static str,
    codes: &'static [KeyCode],
}

/// What one key is called on screen.
fn key_symbol(code: KeyCode) -> String {
    match code {
        KeyCode::Enter => "enter".to_owned(),
        KeyCode::Esc => "esc".to_owned(),
        KeyCode::Up => "↑".to_owned(),
        KeyCode::Down => "↓".to_owned(),
        KeyCode::Left => "←".to_owned(),
        KeyCode::Right => "→".to_owned(),
        KeyCode::PageUp => "pgup".to_owned(),
        KeyCode::PageDown => "pgdn".to_owned(),
        KeyCode::Char(' ') => "space".to_owned(),
        KeyCode::Char(character) => character.to_string(),
        other => format!("{other:?}").to_lowercase(),
    }
}

/// The exact text the footer prints for one hint.
fn hint_text(hint: &KeyHint) -> String {
    let keys = hint
        .codes
        .iter()
        .map(|code| key_symbol(*code))
        .collect::<Vec<_>>()
        .join("/");
    format!("{keys} {}", hint.label)
}

const REVIEW_HINTS: &[KeyHint] = &[
    KeyHint {
        label: "protect",
        codes: &[KeyCode::Char(' ')],
    },
    KeyHint {
        label: "apply",
        codes: &[KeyCode::Char('a')],
    },
    KeyHint {
        label: "details",
        codes: &[KeyCode::Enter],
    },
    KeyHint {
        label: "filter",
        codes: &[KeyCode::Char('/')],
    },
    KeyHint {
        label: "keeping",
        codes: &[KeyCode::Char('2')],
    },
    KeyHint {
        label: "log",
        codes: &[KeyCode::Char('l')],
    },
    KeyHint {
        label: "refresh",
        codes: &[KeyCode::Char('r')],
    },
    KeyHint {
        label: "help",
        codes: &[KeyCode::Char('?')],
    },
    KeyHint {
        label: "quit",
        codes: &[KeyCode::Char('q')],
    },
];

const KEEPING_HINTS: &[KeyHint] = &[
    KeyHint {
        label: "protect",
        codes: &[KeyCode::Char(' ')],
    },
    KeyHint {
        label: "protect family",
        codes: &[KeyCode::Char('P')],
    },
    KeyHint {
        label: "details",
        codes: &[KeyCode::Enter],
    },
    KeyHint {
        label: "filter",
        codes: &[KeyCode::Char('/')],
    },
    KeyHint {
        label: "review",
        codes: &[KeyCode::Char('1')],
    },
    KeyHint {
        label: "log",
        codes: &[KeyCode::Char('l')],
    },
    KeyHint {
        label: "help",
        codes: &[KeyCode::Char('?')],
    },
    KeyHint {
        label: "quit",
        codes: &[KeyCode::Char('q')],
    },
];

const SETUP_HINTS: &[KeyHint] = &[
    KeyHint {
        label: "use this",
        codes: &[KeyCode::Char(' ')],
    },
    KeyHint {
        label: "profile",
        codes: &[KeyCode::Left, KeyCode::Right],
    },
    KeyHint {
        label: "field",
        codes: &[KeyCode::Char('['), KeyCode::Char(']')],
    },
    KeyHint {
        label: "edit",
        codes: &[KeyCode::Char('e')],
    },
    KeyHint {
        label: "preview",
        codes: &[KeyCode::Char('v')],
    },
    KeyHint {
        label: "save",
        codes: &[KeyCode::Char('s')],
    },
    KeyHint {
        label: "help",
        codes: &[KeyCode::Char('?')],
    },
    KeyHint {
        label: "quit",
        codes: &[KeyCode::Char('q')],
    },
];

const EDITOR_HINTS: &[KeyHint] = &[
    KeyHint {
        label: "save",
        codes: &[KeyCode::Enter],
    },
    KeyHint {
        label: "cancel",
        codes: &[KeyCode::Esc],
    },
];

const HELP_HINTS: &[KeyHint] = &[
    KeyHint {
        label: "scroll",
        codes: &[KeyCode::Up, KeyCode::Down],
    },
    KeyHint {
        label: "close",
        codes: &[KeyCode::Esc],
    },
];

const ACTIVITY_HINTS: &[KeyHint] = &[
    KeyHint {
        label: "scroll",
        codes: &[KeyCode::Up, KeyCode::Down],
    },
    KeyHint {
        label: "close",
        codes: &[KeyCode::Esc],
    },
];

const CONFIRM_HINTS: &[KeyHint] = &[
    KeyHint {
        label: "apply",
        codes: &[KeyCode::Enter],
    },
    KeyHint {
        label: "scroll",
        codes: &[KeyCode::Up, KeyCode::Down],
    },
    KeyHint {
        label: "cancel",
        codes: &[KeyCode::Esc],
    },
];

const YES_NO_HINTS: &[KeyHint] = &[
    KeyHint {
        label: "yes",
        codes: &[KeyCode::Char('y'), KeyCode::Enter],
    },
    KeyHint {
        label: "cancel",
        codes: &[KeyCode::Esc],
    },
];

/// The one key table both the footer and the key handler agree on.
const fn hints_for(view: View, editor: Editor, overlay: Overlay) -> &'static [KeyHint] {
    if !matches!(editor, Editor::None) {
        return EDITOR_HINTS;
    }
    match overlay {
        Overlay::Help => HELP_HINTS,
        Overlay::Activity => ACTIVITY_HINTS,
        Overlay::Confirm => CONFIRM_HINTS,
        Overlay::CacheConfirm | Overlay::ConfigSave => YES_NO_HINTS,
        Overlay::None => match view {
            View::Review => REVIEW_HINTS,
            View::Keeping => KEEPING_HINTS,
            View::Setup => SETUP_HINTS,
        },
    }
}

/// Resolve a key press against the current view, or report that this view has
/// no meaning for it.
fn intent_for(view: View, key: KeyEvent) -> Option<Intent> {
    if let Some(intent) = global_intent(key) {
        return Some(intent);
    }
    match view {
        View::Review => review_intent(key),
        View::Keeping => keeping_intent(key),
        View::Setup => setup_intent(key),
    }
}

fn global_intent(key: KeyEvent) -> Option<Intent> {
    Some(match key.code {
        KeyCode::Char('q') => Intent::Quit,
        KeyCode::Char('?') => Intent::Help,
        KeyCode::Char('1') => Intent::Show(View::Review),
        KeyCode::Char('2') => Intent::Show(View::Keeping),
        KeyCode::Char('3') => Intent::Show(View::Setup),
        KeyCode::Char('l') => Intent::ActivityLog,
        KeyCode::Char('r') => Intent::Refresh,
        KeyCode::Down | KeyCode::Char('j') => Intent::Move(1),
        KeyCode::Up | KeyCode::Char('k') => Intent::Move(-1),
        _ => return None,
    })
}

fn review_intent(key: KeyEvent) -> Option<Intent> {
    Some(match key.code {
        KeyCode::Char(' ') => Intent::Protect,
        KeyCode::Char('P') => Intent::ProtectFamily,
        KeyCode::Char('c') => Intent::NamePrefix,
        KeyCode::Enter => Intent::ToggleDetail,
        KeyCode::Char('/') => Intent::StartFilter,
        KeyCode::Char('a') => Intent::Apply,
        KeyCode::PageDown => Intent::Move(10),
        KeyCode::PageUp => Intent::Move(-10),
        _ => return None,
    })
}

fn keeping_intent(key: KeyEvent) -> Option<Intent> {
    Some(match key.code {
        KeyCode::Char(' ') => Intent::Protect,
        KeyCode::Char('P') => Intent::ProtectFamily,
        KeyCode::Char('c') => Intent::NamePrefix,
        KeyCode::Enter => Intent::ToggleDetail,
        KeyCode::Char('/') => Intent::StartFilter,
        KeyCode::PageDown => Intent::Move(10),
        KeyCode::PageUp => Intent::Move(-10),
        _ => return None,
    })
}

fn setup_intent(key: KeyEvent) -> Option<Intent> {
    Some(match key.code {
        // `enter` deliberately has no meaning here. It opens the details pane
        // in the other two views, and one key with two jobs is the defect this
        // interface exists to remove.
        KeyCode::Char(' ') => Intent::ToggleCandidate,
        KeyCode::Left => Intent::CycleProfile(-1),
        KeyCode::Right => Intent::CycleProfile(1),
        KeyCode::Char('[') => Intent::CyclePolicyField(-1),
        KeyCode::Char(']') => Intent::CyclePolicyField(1),
        KeyCode::Char('e') => Intent::EditPolicy,
        KeyCode::Char('v') => Intent::PreviewProposal,
        KeyCode::Char('s') => Intent::SaveProposal,
        KeyCode::PageDown => Intent::ScrollPreview(3),
        KeyCode::PageUp => Intent::ScrollPreview(-3),
        _ => return None,
    })
}

/// The interface word for a disposition.
///
/// The taxonomy stays exactly as it is in `Disposition`, its `Display`, and
/// every machine document. Only what a person reads on screen changes.
const fn plain_disposition(disposition: Disposition) -> &'static str {
    match disposition {
        Disposition::Protected => "you protected this",
        Disposition::Owned => "a rule covers this",
        Disposition::AuthorizedUnscoped => "you authorized this without a rule",
        Disposition::Unowned => "no rule covers this",
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
    selected: usize,
    filter: String,
    editor: Editor,
    detail_open: bool,
    overlay: Overlay,
    confirm_scroll: usize,
    overlay_scroll: u16,
    pane_scroll: u16,
    survey: ConfiguratorSurvey,
    protection: ProtectionState,
    observations: ObservationState,
    configure_selected: BTreeSet<String>,
    configure_row: usize,
    configure_profile: PolicyProfile,
    configure_policy: PolicySettings,
    policy_field: usize,
    config_proposal: Option<ConfigProposal>,
    prefix_input: String,
    prefix_kind: ResourceKind,
    status: String,
}

/// Collect the opening Docker snapshot, advancing the observed-unreferenced
/// clock, and degrade to an empty snapshot when Docker is unreachable.
async fn startup_snapshot(
    loaded: &LoadedConfig,
    state_paths: &StatePaths,
    protection: &ProtectionState,
    plan_created_at: i64,
) -> Result<(Plan, ConfiguratorSurvey, ObservationState, Option<String>), RunError> {
    let inventory = match collect_inventory_for_configuration().await {
        Ok(inventory) => inventory,
        Err(error) => {
            return Ok((
                Plan {
                    decisions: Vec::new(),
                },
                survey_inventory(&[]),
                ObservationState::default(),
                Some(error.to_string()),
            ))
        }
    };
    let observations =
        ObservationStore::new(state_paths.clone()).record(&inventory, plan_created_at)?;
    let plan = build_plan_with_context(
        &loaded.config,
        inventory.clone(),
        plan_created_at,
        &PlanContext {
            protection,
            observations: &observations,
        },
    )
    .map_err(|error| RunError::Internal(format!("cannot build TUI snapshot: {error}")))?;
    Ok((plan, survey_inventory(&inventory), observations, None))
}

impl App {
    async fn load(explicit_config: Option<&Path>) -> Result<Self, RunError> {
        let (loaded, built_in_config) = load_tui_config(explicit_config)?;
        let state_paths = StatePaths::from_env()?;
        let protection_store = ProtectionStore::new(state_paths.clone());
        let protection = protection_store.snapshot()?;
        let plan_created_at = epoch_seconds()?;
        let config_hash = stable_config_hash(&loaded.source);
        let (plan, mut survey, observations, docker_error) =
            startup_snapshot(&loaded, &state_paths, &protection, plan_created_at).await?;
        let plan_id = tui_plan_id(&config_hash, plan_created_at, &plan);
        let history = ActivityJournal::new(state_paths.clone()).completed_passes()?;
        let plan_validity = if docker_error.is_none() {
            PlanValidity::Valid
        } else {
            PlanValidity::Stale
        };
        let configure_policy = PolicyProfile::Workstation.settings();
        let startup_inventory = plan
            .decisions
            .iter()
            .map(|decision| decision.resource.clone())
            .collect::<Vec<_>>();
        refresh_candidate_warnings(
            &mut survey,
            &configure_policy,
            &startup_inventory,
            plan_created_at,
            &PlanContext {
                protection: &protection,
                observations: &observations,
            },
        );
        let configure_row = first_configure_row(&survey);
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
            protection,
            plan,
            plan_id,
            plan_created_at,
            config_hash,
            plan_validity,
            history,
            view: if built_in_config {
                View::Setup
            } else {
                View::Review
            },
            selected: 0,
            filter: String::new(),
            editor: Editor::None,
            detail_open: false,
            overlay: Overlay::None,
            confirm_scroll: 0,
            overlay_scroll: 0,
            pane_scroll: 0,
            survey,
            observations,
            configure_selected,
            configure_row,
            configure_profile: PolicyProfile::Workstation,
            configure_policy,
            policy_field: 0,
            config_proposal: None,
            prefix_input: String::new(),
            prefix_kind: ResourceKind::Container,
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
        let selected_candidate_id = self
            .selected_candidate()
            .map(|candidate| candidate.id.clone());
        let (loaded, built_in_config) = load_tui_config(self.explicit_config.as_deref())?;
        let protection = self.protection_store.snapshot()?;
        let inventory = collect_inventory_for_configuration().await?;
        let plan_created_at = epoch_seconds()?;
        let config_hash = stable_config_hash(&loaded.source);
        let observations =
            ObservationStore::new(self.state_paths.clone()).record(&inventory, plan_created_at)?;
        let plan = build_plan_with_context(
            &loaded.config,
            inventory.clone(),
            plan_created_at,
            &PlanContext {
                protection: &protection,
                observations: &observations,
            },
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
        self.protection = protection;
        self.observations = observations;
        self.survey = survey_inventory(&inventory);
        refresh_candidate_warnings(
            &mut self.survey,
            &self.configure_policy,
            &inventory,
            plan_created_at,
            &PlanContext {
                protection: &self.protection,
                observations: &self.observations,
            },
        );
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
        self.configure_row = selected_candidate_id
            .as_deref()
            .and_then(|id| {
                self.survey
                    .candidates
                    .iter()
                    .position(|candidate| candidate.id == id)
            })
            .unwrap_or_else(|| first_configure_row(&self.survey));
        self.clamp_selection();
        self.status = format!(
            "Refreshed: {} would be removed, {} kept",
            self.plan.pending_count(),
            self.plan.decisions.len() - self.plan.pending_count()
        );
        if self.loaded.config.rules.build_cache.is_some() {
            self.status
                .push_str(" • build cache is authorized without ownership evidence");
        }
        Ok(())
    }

    fn refresh_activity(&mut self) -> Result<(), RunError> {
        let history = ActivityJournal::new(self.state_paths.clone()).completed_passes()?;
        if history != self.history {
            self.history = history;
            replace_status(&mut self.status, "Activity log updated");
        }
        Ok(())
    }

    /// Every removal in the plan, in plan order and never filtered.
    ///
    /// This is what the confirmation modal and the executor act on. A filter is
    /// a reading aid and must never narrow the set a person confirms.
    fn plan_targets(&self) -> Vec<&Decision> {
        self.plan
            .decisions
            .iter()
            .filter(|decision| decision.action == Action::Remove)
            .collect()
    }

    /// The rows `Review` lists: removals, narrowed by the active filter.
    fn review_rows(&self) -> Vec<&Decision> {
        self.plan
            .decisions
            .iter()
            .filter(|decision| decision.action == Action::Remove)
            .filter(|decision| self.matches_filter(decision))
            .collect()
    }

    /// The rows `Keeping` lists: everything the plan keeps, narrowed by the
    /// active filter.
    fn keeping_rows(&self) -> Vec<&Decision> {
        self.plan
            .decisions
            .iter()
            .filter(|decision| decision.action == Action::Keep)
            .filter(|decision| self.matches_filter(decision))
            .collect()
    }

    fn matches_filter(&self, decision: &Decision) -> bool {
        self.filter.is_empty()
            || fuzzy_match(&decision.resource.name, &self.filter)
            || fuzzy_match(&decision.resource.id, &self.filter)
            || decision
                .matched_rule
                .as_deref()
                .is_some_and(|rule| fuzzy_match(rule, &self.filter))
    }

    fn current_rows(&self) -> Vec<&Decision> {
        match self.view {
            View::Review => self.review_rows(),
            View::Keeping => self.keeping_rows(),
            View::Setup => Vec::new(),
        }
    }

    fn selected_decision(&self) -> Option<&Decision> {
        self.current_rows().get(self.selected).copied()
    }

    fn clamp_selection(&mut self) {
        let length = match self.view {
            View::Review => self.review_rows().len(),
            View::Keeping => self.keeping_rows().len(),
            View::Setup => self.survey.candidates.len(),
        };
        self.selected = self.selected.min(length.saturating_sub(1));
    }

    fn move_selection(&mut self, delta: isize) {
        match self.view {
            View::Review | View::Keeping => {
                let length = self.current_rows().len();
                if length == 0 {
                    self.selected = 0;
                } else {
                    self.selected = self.selected.saturating_add_signed(delta).min(length - 1);
                }
            }
            View::Setup => {
                self.configure_row = move_configure_row(&self.survey, self.configure_row, delta);
            }
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
        self.refresh_survey_warnings();
        self.config_proposal = None;
        self.status = format!("Policy profile: {}", self.configure_profile.title());
    }

    /// Recompute Compose warnings from the current policy against the same
    /// inventory snapshot and clock the proposal preview uses.
    fn refresh_survey_warnings(&mut self) {
        let inventory = self.inventory_snapshot();
        refresh_candidate_warnings(
            &mut self.survey,
            &self.configure_policy,
            &inventory,
            self.plan_created_at,
            &PlanContext {
                protection: &self.protection,
                observations: &self.observations,
            },
        );
    }

    /// The durable state every policy and preview surface in this session
    /// shares, so a preview never contradicts the plan beside it.
    fn plan_context(&self) -> PlanContext<'_> {
        PlanContext {
            protection: &self.protection,
            observations: &self.observations,
        }
    }

    fn inventory_snapshot(&self) -> Vec<InventoryItem> {
        self.plan
            .decisions
            .iter()
            .map(|decision| decision.resource.clone())
            .collect()
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
        let mut next_policy = self.configure_policy.clone();
        set_policy_field(field, &mut next_policy, self.prefix_input.trim())?;
        next_policy.validate()?;
        self.configure_policy = next_policy;
        self.refresh_survey_warnings();
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
        let inventory = self.inventory_snapshot();
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
            context: self.plan_context(),
        })?;
        self.status = format!(
            "Proposal {}: {} would be removed after save; press s to review",
            proposal.proposal_id, proposal.preview.after_pending
        );
        self.config_proposal = Some(proposal);
        Ok(())
    }

    fn start_prefix_editor(&mut self) {
        let Some(decision) = self.selected_decision() else {
            replace_status(&mut self.status, "Nothing is selected");
            return;
        };
        if decision.resource.kind == ResourceKind::BuildCache {
            replace_status(&mut self.status, "Build cache has no name to own");
            return;
        }
        let kind = decision.resource.kind;
        let name = decision.resource.name.clone();
        self.prefix_kind = kind;
        self.prefix_input = name;
        self.editor = Editor::Prefix;
        replace_status(
            &mut self.status,
            "Edit the exact prefix; Enter adds it, Esc cancels",
        );
    }

    fn accept_prefix(&mut self) -> Result<(), RunError> {
        let kind = self.prefix_kind;
        let inventory = self.inventory_snapshot();
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
        self.view = View::Setup;
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
        self.view = View::Review;
        self.status = format!(
            "Saved {} • review refreshed • apply remains a separate confirmation",
            result.path.display()
        );
        Ok(())
    }

    async fn toggle_protection(&mut self) -> Result<(), RunError> {
        let Some(decision) = self.selected_decision().cloned() else {
            replace_status(&mut self.status, "Nothing is selected");
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
            self.status = format!("Removed protection: {kind} {value}");
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

    /// Protect or release every resource sharing the selected object's
    /// ownership family through one typed runtime entry.
    async fn toggle_family_protection(&mut self) -> Result<(), RunError> {
        let Some(decision) = self.selected_decision().cloned() else {
            replace_status(&mut self.status, "Nothing is selected");
            return Ok(());
        };
        let Some((key, value)) = family_label(&decision.resource) else {
            replace_status(
                &mut self.status,
                &format!(
                    "{} carries no Compose or agent family label; press space to protect it alone",
                    decision.resource.name
                ),
            );
            return Ok(());
        };
        let (pair, members) = family_protection_target(&self.plan, key, value);
        let held = self
            .protection_store
            .snapshot()?
            .entries
            .iter()
            .any(|entry| entry.kind == ProtectionKind::Label && entry.value == pair);
        if held {
            self.protection_store
                .remove(ProtectionKind::Label, std::slice::from_ref(&pair))?;
            self.status = format!("Removed protection: label {pair} ({members} objects)");
        } else {
            self.protection_store
                .add(ProtectionKind::Label, std::slice::from_ref(&pair))?;
            self.status = format!("Protected family: label {pair} ({members} objects)");
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
            replace_status(&mut self.status, "Nothing would be removed");
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
            &self.observations,
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
            _ = refresh.tick(), if matches!(app.overlay, Overlay::None | Overlay::Help | Overlay::Activity) => {
                if let Err(error) = app.refresh().await {
                    app.plan_validity = PlanValidity::Stale;
                    app.overlay = Overlay::None;
                    app.status = format!("Refresh failed: {}", super::run_error_message(&error));
                }
                refresh = tokio::time::interval(app.refresh_interval());
                refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                refresh.tick().await;
            }
            _ = activity.tick(), if matches!(app.overlay, Overlay::None | Overlay::Help | Overlay::Activity) => {
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
    if is_global_quit(key) {
        return Ok(true);
    }
    if app.editor == Editor::Prefix {
        handle_prefix_key(app, key)?;
        return Ok(false);
    }
    if app.editor == Editor::Policy {
        handle_policy_key(app, key);
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
    let Some(intent) = intent_for(app.view, key) else {
        return Ok(false);
    };
    match intent {
        Intent::Quit => return Ok(true),
        Intent::Help => {
            app.overlay_scroll = 0;
            app.overlay = Overlay::Help;
        }
        Intent::Show(view) => switch_view(app, view),
        Intent::ActivityLog => {
            app.overlay_scroll = 0;
            app.overlay = Overlay::Activity;
        }
        Intent::Move(delta) => app.move_selection(delta),
        Intent::ScrollPreview(delta) => app.pane_scroll = move_scroll(app.pane_scroll, delta),
        Intent::ToggleDetail => {
            app.detail_open = !app.detail_open;
            app.pane_scroll = 0;
        }
        Intent::StartFilter => {
            app.editor = Editor::Filter;
            replace_status(&mut app.status, "Type to narrow; Enter accepts, Esc clears");
        }
        Intent::Protect => app.toggle_protection().await?,
        Intent::ProtectFamily => app.toggle_family_protection().await?,
        Intent::NamePrefix => app.start_prefix_editor(),
        Intent::Apply => open_confirmation(app),
        Intent::Refresh => {
            if let Err(error) = app.refresh().await {
                app.plan_validity = PlanValidity::Stale;
                app.status = format!("Refresh failed: {}", super::run_error_message(&error));
            }
        }
        Intent::CycleProfile(delta) => app.cycle_profile(delta),
        Intent::CyclePolicyField(delta) => app.cycle_policy_field(delta),
        Intent::EditPolicy => app.start_policy_editor(),
        Intent::ToggleCandidate => app.toggle_selected_candidate(),
        Intent::PreviewProposal => {
            if let Err(error) = app.preview_configuration() {
                app.status = format!("Proposal blocked: {}", super::run_error_message(&error));
            }
        }
        Intent::SaveProposal => {
            if app.config_proposal.is_some() {
                app.overlay = Overlay::ConfigSave;
            } else {
                replace_status(&mut app.status, "Preview the proposal with v before saving");
            }
        }
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
        Overlay::Help | Overlay::Activity => handle_scrolling_overlay_key(app, key),
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
                    "Enabled a build-cache proposal with no ownership evidence; preview before save",
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

/// How many lines the open overlay can scroll through.
fn overlay_line_count(app: &App) -> usize {
    match app.overlay {
        Overlay::Activity => activity_lines(&app.history).len(),
        Overlay::Help => HELP_ENTRIES.len() + 4,
        _ => 0,
    }
}

/// Scroll whichever overlay is open, clamped to the last line it actually has.
///
/// Without the clamp the pane scrolls off its own content and shows an empty
/// box, which reads as "nothing here" rather than "you scrolled too far".
fn handle_scrolling_overlay_key(app: &mut App, key: KeyEvent) {
    let last = overlay_line_count(app)
        .saturating_sub(1)
        .try_into()
        .unwrap_or(u16::MAX);
    match key.code {
        KeyCode::Esc | KeyCode::Char('l' | 'q' | '?') => {
            app.overlay = Overlay::None;
            app.overlay_scroll = 0;
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.overlay_scroll = move_scroll(app.overlay_scroll, 1).min(last);
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.overlay_scroll = move_scroll(app.overlay_scroll, -1);
        }
        KeyCode::PageDown => {
            app.overlay_scroll = move_scroll(app.overlay_scroll, 10).min(last);
        }
        KeyCode::PageUp => {
            app.overlay_scroll = move_scroll(app.overlay_scroll, -10);
        }
        _ => {}
    }
}

async fn handle_plan_confirmation(
    terminal: &mut CrosstermTerminal,
    app: &mut App,
    key: KeyEvent,
) -> Result<(), RunError> {
    match key.code {
        KeyCode::Esc | KeyCode::Char('n') => {
            app.overlay = Overlay::None;
            replace_status(&mut app.status, "Apply cancelled");
        }
        KeyCode::Enter => {
            app.overlay = Overlay::None;
            replace_status(&mut app.status, "Applying the confirmed plan…");
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
            app.status = format!("Showing names matching {:?}", app.filter);
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

fn handle_policy_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            app.editor = Editor::None;
            app.prefix_input.clear();
            replace_status(&mut app.status, "Policy edit cancelled");
        }
        KeyCode::Enter => {
            if let Err(error) = app.accept_policy_value() {
                app.status = format!(
                    "Invalid value: {} • correct it or press Esc",
                    super::run_error_message(&error)
                );
            }
        }
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
}

fn is_global_quit(key: KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c' | 'C'))
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
        replace_status(&mut app.status, "Nothing would be removed");
    }
}

fn switch_view(app: &mut App, view: View) {
    app.view = view;
    app.selected = 0;
    app.detail_open = false;
    // One offset serves the details pane and the Setup preview. Carrying it
    // across a view switch leaves the next pane scrolled with no key to scroll
    // it back, which is how the old dead scroll offset looked from the outside.
    app.pane_scroll = 0;
    app.clamp_selection();
}

fn draw(terminal: &mut CrosstermTerminal, app: &mut App) -> Result<(), RunError> {
    terminal
        .draw(|frame| render(frame, app))
        .map(|_| ())
        .map_err(|error| RunError::Internal(format!("cannot draw terminal: {error}")))
}

fn render(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        render_too_small(frame, area);
        return;
    }
    let hints = hint_lines(
        hints_for(app.view, app.editor, app.overlay),
        area.width.saturating_sub(2),
    );
    let footer_height = u16::try_from(hints.len()).unwrap_or(1).saturating_add(1);
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(footer_height),
        ])
        .split(area);
    render_header(frame, app, areas[0]);
    render_view_bar(frame, app, areas[1]);

    match app.view {
        View::Review => render_review(frame, app, areas[2]),
        View::Keeping => render_keeping(frame, app, areas[2]),
        View::Setup => render_setup(frame, app, areas[2]),
    }
    render_footer(frame, app, &hints, areas[3]);

    let body = areas[2];
    if app.editor != Editor::None {
        render_editor(frame, app, body);
        return;
    }

    match app.overlay {
        Overlay::None => {}
        Overlay::Help => render_help(frame, app, body),
        Overlay::Activity => render_activity(frame, app, body),
        Overlay::Confirm => render_confirmation(frame, app, body),
        Overlay::CacheConfirm => render_cache_confirmation(frame, app, body),
        Overlay::ConfigSave => render_config_save(frame, app, body),
    }
}

/// Say the terminal is too small and by how much, rather than drawing a layout
/// that cannot hold its own content.
fn render_too_small(frame: &mut Frame<'_>, area: Rect) {
    let text = Text::from(vec![
        Line::from("docker_maid"),
        Line::from(""),
        Line::from("This terminal is too small."),
        Line::from(format!("Now:    {} x {}", area.width, area.height)),
        Line::from(format!("Needed: {MIN_WIDTH} x {MIN_HEIGHT}")),
        Line::from(""),
        Line::from("Make the window bigger, or press q to quit."),
    ]);
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), area);
}

fn render_header(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let removals = app.plan.pending_count();
    let reclaim = app
        .plan
        .decisions
        .iter()
        .filter(|decision| decision.action == Action::Remove)
        .filter_map(|decision| decision.resource.size)
        .fold(0u64, u64::saturating_add);
    let mut spans = vec![
        Span::styled("docker_maid", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(format!(
            " · {removals} to remove · {} reclaimable",
            format_bytes(reclaim)
        )),
    ];
    if app.plan_validity == PlanValidity::Stale {
        spans.push(Span::styled(
            " · plan is stale, press r",
            Style::default()
                .fg(ATTENTION_COLOR)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if app.loaded.config.rules.build_cache.is_some() {
        spans.push(Span::styled(
            " · build cache runs without ownership evidence",
            Style::default().fg(ATTENTION_COLOR),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_view_bar(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let mut spans = Vec::new();
    for view in View::ALL {
        let label = format!(" [{}] {} ", view.index() + 1, view.title());
        let style = if view == app.view {
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::default()
        };
        spans.push(Span::styled(label, style));
        spans.push(Span::raw(" "));
    }
    if let Some(last) = app.history.last() {
        spans.push(Span::raw(format!(
            "· last pass removed {}, freed {}",
            last.removed_count,
            format_bytes(last.reclaimed_bytes)
        )));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_review(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let rows = app.review_rows();
    let hidden = app.plan_targets().len().saturating_sub(rows.len());
    let (list_area, detail_area) = split_for_detail(app, area);

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(list_area);

    let mut heading = vec![Span::styled(
        format!("WOULD REMOVE  ({})", rows.len()),
        Style::default()
            .fg(REMOVE_COLOR)
            .add_modifier(Modifier::BOLD),
    )];
    if hidden != 0 {
        heading.push(Span::styled(
            format!("   {hidden} more hidden by the filter"),
            Style::default().fg(ATTENTION_COLOR),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(heading)), sections[0]);

    let items = if rows.is_empty() {
        vec![ListItem::new(Text::from(vec![
            Line::from(""),
            Line::from("  Nothing would be removed."),
            Line::from("  Every resource is kept. Press 2 to see why."),
        ]))]
    } else {
        rows.iter()
            .map(|decision| ListItem::new(removal_entry(decision)))
            .collect::<Vec<_>>()
    };
    // An empty list has a message in it, not a row, so nothing is selected.
    let mut state = ListState::default().with_selected((!rows.is_empty()).then_some(app.selected));
    frame.render_stateful_widget(
        List::new(items)
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
            .highlight_symbol("▶ "),
        sections[1],
        &mut state,
    );

    let kept = app
        .plan
        .decisions
        .len()
        .saturating_sub(app.plan_targets().len());
    frame.render_widget(
        Paragraph::new(Line::from(format!(
            "KEEPING  ({kept})                    press 2 to see"
        ))),
        sections[2],
    );

    render_detail_pane(frame, app, detail_area);
}

/// One removal, rendered as the three lines a person reads in order: what it
/// is, how big and how old, and why the policy claimed it.
fn removal_entry(decision: &Decision) -> Text<'static> {
    let mut lines = vec![Line::from(vec![
        Span::styled(
            format!("{:<13}", decision.resource.kind.to_string()),
            Style::default().fg(REMOVE_COLOR),
        ),
        Span::styled(
            decision.resource.name.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ])];
    lines.push(Line::from(format!(
        "     {} old · {}",
        format_age(decision.age_seconds),
        decision
            .resource
            .size
            .map_or_else(|| "size unknown".to_owned(), format_bytes)
    )));
    for (index, clause) in reason_clauses(&decision.reason).into_iter().enumerate() {
        let label = if index == 0 {
            "     why:  "
        } else {
            "           "
        };
        lines.push(Line::from(format!("{label}{clause}")));
    }
    if decision.disposition == Disposition::AuthorizedUnscoped {
        lines.push(Line::from(Span::styled(
            format!("     {}", plain_disposition(decision.disposition)),
            Style::default()
                .fg(ATTENTION_COLOR)
                .add_modifier(Modifier::BOLD),
        )));
    }
    lines.push(Line::from(""));
    Text::from(lines)
}

fn render_keeping(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let rows = app.keeping_rows();
    let (list_area, detail_area) = split_for_detail(app, area);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(3)])
        .split(list_area);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!("KEEPING  ({})", rows.len()),
            Style::default().add_modifier(Modifier::BOLD),
        ))),
        sections[0],
    );

    let items = if rows.is_empty() {
        vec![ListItem::new("  Nothing is being kept.")]
    } else {
        rows.iter()
            .enumerate()
            .map(|(index, decision)| ListItem::new(keeping_entry(decision, index == app.selected)))
            .collect::<Vec<_>>()
    };
    let mut state = ListState::default().with_selected((!rows.is_empty()).then_some(app.selected));
    frame.render_stateful_widget(
        List::new(items)
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
            .highlight_symbol("▶ "),
        sections[1],
        &mut state,
    );

    render_detail_pane(frame, app, detail_area);
}

/// One kept resource. This list routinely holds sixty rows, so it stays one
/// line per item and only the selected row opens up.
fn keeping_entry(decision: &Decision, expanded: bool) -> Text<'static> {
    let style = match decision.disposition {
        Disposition::Protected => Style::default().fg(PROTECT_COLOR),
        Disposition::AuthorizedUnscoped => Style::default().fg(ATTENTION_COLOR),
        Disposition::Owned | Disposition::Unowned => Style::default(),
    };
    let mut lines = vec![Line::from(vec![
        Span::styled(format!("{:<13}", decision.resource.kind.to_string()), style),
        Span::raw(format!("{:<28}", truncate(&decision.resource.name, 27))),
        Span::styled(plain_disposition(decision.disposition), style),
    ])];
    if expanded {
        lines.push(Line::from(format!(
            "     {} · {}",
            decision.resource.state,
            decision
                .resource
                .size
                .map_or_else(|| "size unknown".to_owned(), format_bytes)
        )));
        for (index, clause) in reason_clauses(&decision.reason).into_iter().enumerate() {
            let label = if index == 0 {
                "     why:  "
            } else {
                "           "
            };
            lines.push(Line::from(format!("{label}{clause}")));
        }
    }
    Text::from(lines)
}

/// Split the body into a list and an optional detail pane.
fn split_for_detail(app: &App, area: Rect) -> (Rect, Option<Rect>) {
    if !app.detail_open || area.width < 100 {
        return (area, None);
    }
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
        .split(area);
    (columns[0], Some(columns[1]))
}

fn render_detail_pane(frame: &mut Frame<'_>, app: &App, area: Option<Rect>) {
    let Some(area) = area else {
        return;
    };
    let detail = app
        .selected_decision()
        .map_or_else(|| "Nothing is selected".to_owned(), resource_detail);
    frame.render_widget(
        Paragraph::new(detail)
            .block(Block::default().borders(Borders::ALL).title("Details"))
            .scroll((app.pane_scroll, 0))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_setup(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(61), Constraint::Percentage(39)])
        .split(area);
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(4), Constraint::Min(5)])
        .split(columns[0]);
    render_setup_header(frame, app, left[0]);
    render_setup_candidates(frame, app, left[1]);
    frame.render_widget(
        Paragraph::new(setup_detail(app))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Policy and before/after preview"),
            )
            .scroll((app.pane_scroll, 0))
            .wrap(Wrap { trim: false }),
        columns[1],
    );
}

fn render_setup_header(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let current_stage = if app.config_proposal.is_some() {
        "Preview ready • s saves"
    } else if app.configure_selected.is_empty() {
        "Choose what this policy owns"
    } else {
        "Press v to build the real preview"
    };
    frame.render_widget(
        Paragraph::new(format!(
            "Profile: {} [←→] • keep for c={} i={} v={} • cache={}/{}\nUsing: {} of {} • {current_stage} • [{}] field, e edits",
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
            "Survey {} • {} with no rule",
            app.survey.snapshot_id, app.survey.summary.unowned_resources
        ))),
        area,
    );
}

fn render_setup_candidates(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let display_indices = candidate_display_indices(&app.survey.candidates);
    let rows = display_indices
        .iter()
        .map(|index| {
            let candidate = &app.survey.candidates[*index];
            let selected = if app.configure_selected.contains(&candidate.id) {
                "[x]"
            } else {
                "[ ]"
            };
            let style = if candidate.warning.is_some() {
                Style::default().fg(ATTENTION_COLOR)
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
            .title("Candidates • space uses one"),
    )
    .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
    .highlight_symbol("▶ ");
    let selected_display_row = display_indices
        .iter()
        .position(|index| *index == app.configure_row)
        .unwrap_or(0);
    let mut table_state = TableState::default().with_selected(Some(selected_display_row));
    frame.render_stateful_widget(table, area, &mut table_state);
}

fn setup_detail(app: &App) -> String {
    if let Some(proposal) = &app.config_proposal {
        let warnings = if proposal.warnings.is_empty() {
            "none".to_owned()
        } else {
            proposal.warnings.join("\n")
        };
        return format!(
            "Proposal {}\n\nTarget: {}\nProfile: {}\nCandidates: {}\nSelected objects: {}\n\nWould remove before: {}\nWould remove after: {}\nNewly claimed: {}\nEstimated reclaim: {}\n\nWarnings:\n{}\n\nThe save changes configuration only. It never deletes Docker objects. After save, Review refreshes and apply remains a separate confirmation.",
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
        );
    }
    let Some(candidate) = app.selected_candidate() else {
        return "No exact agent or Compose ownership evidence was found.\n\nUnlabeled objects stay unowned. In Review or Keeping, select an object and press c to approve a name prefix."
            .to_owned();
    };
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
        if matches!(candidate.selector, CandidateSelector::BuildCache) {
            detail.push_str("Enable only when daemon-wide cache cleanup is intentional.\n");
        }
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
}

/// Pack a key table into lines that fit, so the footer never advertises a key
/// off the right edge of the terminal.
fn hint_lines(hints: &[KeyHint], width: u16) -> Vec<String> {
    let width = usize::from(width.max(1));
    let mut lines = Vec::new();
    let mut current = String::new();
    for hint in hints {
        let piece = hint_text(hint);
        if current.is_empty() {
            current = piece;
        } else if current.chars().count() + 3 + piece.chars().count() <= width {
            let _ = write!(current, " · {piece}");
        } else {
            lines.push(std::mem::take(&mut current));
            current = piece;
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

fn render_footer(frame: &mut Frame<'_>, app: &App, hints: &[String], area: Rect) {
    let status = match app.editor {
        Editor::Prefix => format!("prefix={}▌", app.prefix_input),
        Editor::Policy => format!(
            "{}={}▌",
            PolicyField::ALL[app.policy_field].title(),
            app.prefix_input
        ),
        Editor::Filter => format!("/{}▌", app.filter),
        Editor::None => app.status.clone(),
    };
    let mut lines = hints
        .iter()
        .map(|line| {
            Line::from(Span::styled(
                format!(" {line} "),
                Style::default().add_modifier(Modifier::REVERSED),
            ))
        })
        .collect::<Vec<_>>();
    lines.push(Line::from(status));
    frame.render_widget(Paragraph::new(lines), area);
}

fn render_editor(frame: &mut Frame<'_>, app: &App, body: Rect) {
    let (title, label, guidance, value) = match app.editor {
        Editor::Policy => (
            "Edit policy value",
            PolicyField::ALL[app.policy_field].title(),
            "Durations: 15m, 2h, 7d • cache bytes: 10GiB",
            app.prefix_input.as_str(),
        ),
        Editor::Prefix => (
            "Add explicit name prefix",
            "prefix",
            "Only matching names become owned by this rule",
            app.prefix_input.as_str(),
        ),
        // The filter writes to its own buffer. Reading `prefix_input` here
        // showed whatever the last prefix or policy edit left behind.
        Editor::Filter => (
            "Narrow this list",
            "filter",
            "Matching is case-insensitive and fuzzy",
            app.filter.as_str(),
        ),
        Editor::None => return,
    };
    let area = overlay_area(body, 2, body.height / 4);
    let input_width = usize::from(area.width.saturating_sub(8)).max(1);
    let input = visible_input_tail(value, input_width);
    let message = if app.status.starts_with("Invalid value:") {
        app.status.as_str()
    } else {
        guidance
    };
    let text = Text::from(vec![
        Line::from(format!("{label}:")),
        Line::from(vec![
            Span::raw("> "),
            Span::styled(input, Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("▌"),
        ]),
        Line::from(""),
        Line::from(message),
        Line::from("Enter save • Esc cancel • Ctrl-C quit"),
    ]);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(text)
            .block(Block::default().borders(Borders::ALL).title(title))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn visible_input_tail(value: &str, max_chars: usize) -> String {
    let count = value.chars().count();
    if count <= max_chars {
        return value.to_owned();
    }
    let keep = max_chars.saturating_sub(1);
    let tail = value
        .chars()
        .skip(count.saturating_sub(keep))
        .collect::<String>();
    format!("…{tail}")
}

/// Every key this interface binds, and what it does.
///
/// The left column is the list of key symbols, separated by spaces, so a test
/// can prove that no key is bound without being written down somewhere a person
/// can find it.
const HELP_ENTRIES: &[(&str, &str)] = &[
    ("1 2 3", "Review, Keeping, Setup"),
    ("↑ ↓ j k", "Move the selection"),
    ("pgup pgdn", "Review, Keeping: jump ten rows"),
    ("pgup pgdn", "Setup: scroll the preview pane"),
    ("space", "Review, Keeping: protect or release this object"),
    ("space", "Setup: use or drop this ownership candidate"),
    ("P", "Protect or release this object's whole label family"),
    ("enter", "Open or close the details pane"),
    ("/", "Narrow the list by name, id, or rule"),
    ("c", "Approve a name prefix as ownership evidence"),
    ("a", "Review the exact removal set, then apply it"),
    ("l", "Open the activity log"),
    ("r", "Re-read configuration, state, and Docker"),
    ("← →", "Setup: change the policy profile"),
    ("[ ]", "Setup: choose a policy value; e edits it"),
    ("e", "Setup: edit the chosen policy value"),
    ("v", "Setup: preview the proposed configuration"),
    ("s", "Setup: save a previewed proposal"),
    ("?", "Open this help; esc closes it"),
    ("q", "Quit"),
];

fn render_help(frame: &mut Frame<'_>, app: &App, body: Rect) {
    let area = overlay_area(body, 0, 0);
    frame.render_widget(Clear, area);
    let mut lines = vec![Line::from("docker_maid keyboard help"), Line::from("")];
    for (keys, description) in HELP_ENTRIES {
        lines.push(Line::from(format!("{keys:<10}  {description}")));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(
        "No key deletes one object. Apply can only execute the confirmed set.",
    ));
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(Block::default().borders(Borders::ALL).title("Help"))
            .scroll((app.overlay_scroll, 0))
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// Every line the activity log can show, so the scroll position can be clamped
/// against a real length rather than against nothing.
fn activity_lines(history: &[CompletedPass]) -> Vec<String> {
    let mut lines = Vec::new();
    for pass in history.iter().rev() {
        lines.push(format!(
            "{} · {} pass · removed {} · skipped {} · failed {} · freed {}",
            format_timestamp(pass.completed_at),
            pass.source,
            pass.removed_count,
            pass.skipped_count,
            pass.failure_count,
            format_bytes(pass.reclaimed_bytes)
        ));
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
                lines.push(format!(
                    "  {action:8} {resource_kind:12} {resource_name} · {matched_rule} · {}",
                    format_bytes(*freed_bytes)
                ));
            }
        }
    }
    if lines.is_empty() {
        lines.push("No completed cleanup passes recorded.".to_owned());
    }
    lines
}

fn render_activity(frame: &mut Frame<'_>, app: &App, body: Rect) {
    let area = overlay_area(body, 0, 0);
    frame.render_widget(Clear, area);
    let lines = activity_lines(&app.history);
    let items = lines
        .iter()
        .skip(usize::from(app.overlay_scroll))
        .map(|line| ListItem::new(line.clone()))
        .collect::<Vec<_>>();
    frame.render_widget(
        List::new(items).block(Block::default().borders(Borders::ALL).title(
            match app.history.len() {
                1 => "Activity log • 1 completed pass".to_owned(),
                count => format!("Activity log • {count} completed passes"),
            },
        )),
        area,
    );
}

fn render_cache_confirmation(frame: &mut Frame<'_>, app: &App, body: Rect) {
    let area = overlay_area(body, 3, 1);
    frame.render_widget(Clear, area);
    let count = app
        .selected_candidate()
        .map_or(0, |candidate| candidate.resources.len());
    let text = format!(
        "Build cache carries no owner, project, label, or name.\n\nThis authorizes a rule over {count} current cache records with no ownership evidence. The {} profile suggests an age floor and a byte budget.\n\nNothing is deleted here. The next stage shows the exact plan.\n\ny: enable    esc: cancel",
        app.configure_profile.title()
    );
    frame.render_widget(
        Paragraph::new(text)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(ATTENTION_COLOR))
                    .title("Careful • daemon-wide build cache"),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_config_save(frame: &mut Frame<'_>, app: &App, body: Rect) {
    let area = overlay_area(body, 2, 0);
    frame.render_widget(Clear, area);
    let text = app.config_proposal.as_ref().map_or_else(
        || "No proposal is available.".to_owned(),
        |proposal| {
            format!(
                "Write reviewed configuration?\n\nPath: {}\nProposal: {}\nProfile: {}\nSelected objects: {}\nWould remove after save: {}\nNewly claimed: {}\nEstimated reclaim: {}\n\nThe writer checks the source hash and Docker inventory again. Existing config is backed up. Manual rules and comments stay outside the managed region.\n\nThis does not delete Docker objects.\n\ny: save    esc: cancel",
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

fn render_confirmation(frame: &mut Frame<'_>, app: &App, body: Rect) {
    let area = overlay_area(body, 0, 0);
    frame.render_widget(Clear, area);
    let targets = app.plan_targets();
    // At least one target is always listed. A modal that offers to apply while
    // showing nothing is the thing this floor exists to prevent.
    let inner_height = usize::from(area.height.saturating_sub(9)).max(1);
    let mut lines = vec![
        Line::from(Span::styled(
            match targets.len() {
                1 => "Remove this 1 object?".to_owned(),
                count => format!("Remove these {count} objects?"),
            },
            Style::default()
                .fg(REMOVE_COLOR)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(format!("Plan: {}", app.plan_id)),
        Line::from(""),
    ];
    let unscoped = targets
        .iter()
        .filter(|decision| decision.disposition == Disposition::AuthorizedUnscoped)
        .count();
    if unscoped != 0 {
        lines.push(Line::from(Span::styled(
            format!("{unscoped} of these have no ownership evidence; you authorized them"),
            Style::default()
                .fg(ATTENTION_COLOR)
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
            "Showing {}–{} of {} • ↑↓ or PgUp/PgDn scroll",
            app.confirm_scroll.saturating_add(1),
            shown_end,
            targets.len()
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(
        "Enter confirms this exact set. Esc cancels. No target can be added.",
    ));
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(REMOVE_COLOR))
                    .title("Confirm removal"),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// An overlay rectangle inset from the body by an exact number of cells.
///
/// Exact arithmetic rather than percentages: a percentage layout can round an
/// overlay one cell wider than the region it sits in, and a box that overruns
/// its frame by one column wraps and corrupts the line below it.
fn overlay_area(body: Rect, margin_x: u16, margin_y: u16) -> Rect {
    let margin_x = margin_x.min(body.width / 2);
    let margin_y = margin_y.min(body.height / 2);
    Rect {
        x: body.x.saturating_add(margin_x),
        y: body.y.saturating_add(margin_y),
        width: body.width.saturating_sub(margin_x.saturating_mul(2)).max(1),
        height: body
            .height
            .saturating_sub(margin_y.saturating_mul(2))
            .max(1),
    }
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
            "No config found • Setup opened • proposed writes target {}",
            loaded.path.display()
        )
    } else if loaded.config.rules.build_cache.is_some() {
        format!(
            "Loaded {} • build cache is authorized without ownership evidence",
            loaded.path.display()
        )
    } else {
        format!(
            "Loaded {} • {} would be removed",
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
    format!("Done: removed {removed}, skipped {skipped}, failed {failed}")
}

/// Build the exact protection value for one family and count its members in
/// the current plan, so the action writes typed state and reports a true count.
fn family_protection_target(plan: &Plan, key: &str, value: &str) -> (String, usize) {
    let members = plan
        .decisions
        .iter()
        .filter(|decision| family_label(&decision.resource) == Some((key, value)))
        .count();
    (format!("{key}={value}"), members)
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

fn first_configure_row(survey: &ConfiguratorSurvey) -> usize {
    candidate_display_indices(&survey.candidates)
        .first()
        .copied()
        .unwrap_or(0)
}

fn move_configure_row(
    survey: &ConfiguratorSurvey,
    current_canonical_row: usize,
    delta: isize,
) -> usize {
    let display_indices = candidate_display_indices(&survey.candidates);
    if display_indices.is_empty() {
        return 0;
    }
    let current_display_row = display_indices
        .iter()
        .position(|index| *index == current_canonical_row)
        .unwrap_or(0);
    let next_display_row = current_display_row
        .saturating_add_signed(delta)
        .min(display_indices.len() - 1);
    display_indices[next_display_row]
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

/// Split a policy reason into the clauses the planner already separated, so
/// each one gets its own line instead of arriving as one long sentence.
fn reason_clauses(reason: &str) -> Vec<String> {
    let clauses = reason
        .split(';')
        .map(str::trim)
        .filter(|clause| !clause.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if clauses.is_empty() {
        vec![reason.to_owned()]
    } else {
        clauses
    }
}

fn truncate(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    let kept = value
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    format!("{kept}…")
}

/// A person reads yes and no. `true` and `false` are for the machine document.
const fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

fn resource_detail(decision: &Decision) -> String {
    let mut detail = format!(
        "{}\n\nType: {}\nID: {}\nState: {}\nAge: {}\nSize: {}\nStatus: {}\nRule: {}\nAction: {}\nIn use: {}\nDangling: {}\nDocker's own: {}\n\nWhy:\n{}",
        decision.resource.name,
        decision.resource.kind,
        decision.resource.id,
        decision.resource.state,
        format_age(decision.age_seconds),
        decision
            .resource
            .size
            .map_or_else(|| "unknown".to_owned(), format_bytes),
        plain_disposition(decision.disposition),
        decision.matched_rule.as_deref().unwrap_or("-"),
        decision.action,
        yes_no(decision.resource.referenced),
        yes_no(decision.resource.dangling),
        yes_no(decision.resource.system),
        reason_clauses(&decision.reason).join("\n"),
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

/// An epoch second rendered as a timestamp rather than a raw integer.
fn format_timestamp(epoch_seconds: i64) -> String {
    u64::try_from(epoch_seconds).map_or_else(
        |_| "unknown time".to_owned(),
        |seconds| {
            humantime::format_rfc3339_seconds(std::time::UNIX_EPOCH + Duration::from_secs(seconds))
                .to_string()
        },
    )
}

/// A compact age a person reads at a glance: `3d`, `2h`, `5m`, `30s`.
fn format_age(seconds: Option<u64>) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;

    let Some(value) = seconds else {
        return "unknown".to_owned();
    };
    if value >= DAY {
        format!("{}d", value / DAY)
    } else if value >= HOUR {
        format!("{}h", value / HOUR)
    } else if value >= MINUTE {
        format!("{}m", value / MINUTE)
    } else {
        format!("{value}s")
    }
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
    use std::collections::BTreeMap;

    /// One `App`, so a new field does not break every test that builds one.
    fn test_app(paths: &StatePaths, plan: Plan, view: View) -> App {
        let survey = survey_inventory(
            &plan
                .decisions
                .iter()
                .map(|decision| decision.resource.clone())
                .collect::<Vec<_>>(),
        );
        App {
            explicit_config: None,
            loaded: LoadedConfig {
                path: PathBuf::from("docker_maid.toml"),
                config: Config::default(),
                source: "# test config".to_owned(),
            },
            built_in_config: false,
            state_paths: paths.clone(),
            protection_store: ProtectionStore::new(paths.clone()),
            protection: ProtectionState::default(),
            plan_id: "test-plan".to_owned(),
            plan,
            plan_created_at: 1,
            observations: ObservationState::default(),
            config_hash: "test-config".to_owned(),
            plan_validity: PlanValidity::Valid,
            history: Vec::new(),
            view,
            selected: 0,
            filter: String::new(),
            editor: Editor::None,
            detail_open: false,
            overlay: Overlay::None,
            confirm_scroll: 0,
            overlay_scroll: 0,
            pane_scroll: 0,
            survey,
            configure_selected: BTreeSet::new(),
            configure_row: 0,
            configure_profile: PolicyProfile::Workstation,
            configure_policy: PolicyProfile::Workstation.settings(),
            policy_field: 0,
            config_proposal: None,
            prefix_input: String::new(),
            prefix_kind: ResourceKind::Container,
            status: "ready".to_owned(),
        }
    }

    fn temp_paths(label: &str) -> (PathBuf, StatePaths) {
        let root = std::env::temp_dir().join(format!(
            "docker-maid-tui-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ));
        let paths = StatePaths::new(root.join("state"));
        (root, paths)
    }

    /// Everything a key press could visibly move.
    fn fingerprint(app: &App) -> String {
        format!(
            "{:?}|{:?}|{:?}|{}|{}|{}|{}|{}|{}|{}|{}|{:?}|{}",
            app.view,
            app.overlay,
            app.editor,
            app.selected,
            app.filter,
            app.detail_open,
            app.confirm_scroll,
            app.overlay_scroll,
            app.pane_scroll,
            app.configure_row,
            app.policy_field,
            app.configure_profile,
            app.status
        )
    }

    #[test]
    fn fuzzy_filter_matches_subsequences_case_insensitively() {
        assert!(fuzzy_match("Agent-Sandbox-123", "as13"));
        assert!(fuzzy_match("postgres", "PG"));
        assert!(!fuzzy_match("volume", "network"));
    }

    /// The direct fix for the footer that advertised eleven keys, six of which
    /// did nothing in most views. An advertised key that resolves to no intent
    /// fails the build.
    #[test]
    fn every_advertised_key_resolves_to_an_intent_in_its_own_view() {
        for view in View::ALL {
            for hint in hints_for(view, Editor::None, Overlay::None) {
                for code in hint.codes {
                    let key = KeyEvent::new(*code, KeyModifiers::NONE);
                    assert!(
                        intent_for(view, key).is_some(),
                        "{view:?} advertises {:?} ({}) but nothing handles it",
                        code,
                        hint.label
                    );
                }
            }
        }
    }

    /// The complement: a key that changes nothing is as much of a lie as a key
    /// with no binding, so each advertised key is dispatched for real.
    #[tokio::test]
    async fn every_advertised_key_moves_the_application_when_it_is_pressed() {
        let (root, paths) = temp_paths("keys");
        for view in View::ALL {
            for hint in hints_for(view, Editor::None, Overlay::None) {
                for code in hint.codes {
                    // Refresh reaches the Docker daemon, and quit ends the loop
                    // rather than moving state. Both are covered elsewhere.
                    if matches!(*code, KeyCode::Char('r' | 'q')) {
                        continue;
                    }
                    let mut app = test_app(&paths, fixture_plan(), view);
                    app.selected = 0;
                    let before = fingerprint(&app);
                    let key = KeyEvent::new(*code, KeyModifiers::NONE);
                    handle_main_key(&mut app, key)
                        .await
                        .expect("advertised key is handled");
                    assert_ne!(
                        before,
                        fingerprint(&app),
                        "{view:?} advertises {:?} ({}) but pressing it changed nothing",
                        code,
                        hint.label
                    );
                }
            }
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn quit_is_advertised_and_really_quits() {
        for view in View::ALL {
            assert_eq!(
                intent_for(view, KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
                Some(Intent::Quit)
            );
            assert_eq!(
                intent_for(view, KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE)),
                Some(Intent::Refresh)
            );
        }
        assert!(is_global_quit(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )));
    }

    /// Every key code this interface binds anywhere.
    const PROBE_KEYS: &[KeyCode] = &[
        KeyCode::Char('q'),
        KeyCode::Char('?'),
        KeyCode::Char('1'),
        KeyCode::Char('2'),
        KeyCode::Char('3'),
        KeyCode::Char('l'),
        KeyCode::Char('r'),
        KeyCode::Char('j'),
        KeyCode::Char('k'),
        KeyCode::Char(' '),
        KeyCode::Char('P'),
        KeyCode::Char('c'),
        KeyCode::Char('/'),
        KeyCode::Char('a'),
        KeyCode::Char('e'),
        KeyCode::Char('v'),
        KeyCode::Char('s'),
        KeyCode::Char('['),
        KeyCode::Char(']'),
        KeyCode::Up,
        KeyCode::Down,
        KeyCode::Left,
        KeyCode::Right,
        KeyCode::Enter,
        KeyCode::PageUp,
        KeyCode::PageDown,
    ];

    /// The other half of an honest key surface. The old footer advertised keys
    /// that did nothing; this proves the reverse cannot happen either, so a key
    /// that works is always written down where a person can find it.
    #[test]
    fn every_bound_key_is_written_down_in_the_footer_or_in_help() {
        let documented = HELP_ENTRIES
            .iter()
            .flat_map(|(keys, _)| keys.split_whitespace())
            .collect::<BTreeSet<_>>();
        for view in View::ALL {
            let footer = hints_for(view, Editor::None, Overlay::None)
                .iter()
                .flat_map(|hint| hint.codes.iter().map(|code| key_symbol(*code)))
                .collect::<BTreeSet<_>>();
            for code in PROBE_KEYS {
                let key = KeyEvent::new(*code, KeyModifiers::NONE);
                if intent_for(view, key).is_none() {
                    continue;
                }
                let symbol = key_symbol(*code);
                assert!(
                    footer.contains(&symbol) || documented.contains(symbol.as_str()),
                    "{view:?} binds {symbol:?} but neither its footer nor help mentions it"
                );
            }
        }
    }

    /// A key may mean different things in different views only when the help
    /// says so once per meaning. `space` protects a resource in Review and
    /// Keeping and uses a candidate in Setup, and help carries a line for each.
    /// The old `h/l` pair carried two meanings and one description, which is
    /// why it could not be learned.
    #[test]
    fn a_key_with_more_than_one_meaning_is_described_once_per_meaning() {
        for code in PROBE_KEYS {
            let key = KeyEvent::new(*code, KeyModifiers::NONE);
            let mut meanings = Vec::new();
            for view in View::ALL {
                if let Some(intent) = intent_for(view, key) {
                    if !meanings.contains(&intent) {
                        meanings.push(intent);
                    }
                }
            }
            if meanings.len() <= 1 {
                continue;
            }
            let symbol = key_symbol(*code);
            let described = HELP_ENTRIES
                .iter()
                .filter(|(keys, _)| keys.split_whitespace().any(|token| token == symbol))
                .count();
            assert!(
                described >= meanings.len(),
                "{symbol:?} has {} meanings ({meanings:?}) but only {described} help lines",
                meanings.len()
            );
        }
    }

    /// Every view's key table has to fit the narrowest terminal this interface
    /// agrees to draw into, or the footer scrolls a key off the edge.
    #[test]
    fn every_key_table_fits_two_lines_at_the_minimum_width() {
        for view in View::ALL {
            let lines = hint_lines(hints_for(view, Editor::None, Overlay::None), MIN_WIDTH - 2);
            assert!(
                lines.len() <= 2,
                "{view:?} needs {} footer lines at {MIN_WIDTH} columns",
                lines.len()
            );
            for line in &lines {
                assert!(line.chars().count() <= usize::from(MIN_WIDTH - 2));
            }
        }
    }

    /// A box that overruns its region by one cell wraps in a real terminal and
    /// corrupts the line below it, which a fixed-size test buffer hides by
    /// clipping. So the geometry itself is what gets checked.
    #[test]
    fn an_overlay_never_reaches_outside_the_region_that_holds_it() {
        for width in [60u16, 80, 100, 140, 200] {
            for height in [20u16, 24, 30, 42, 50] {
                let body = Rect {
                    x: 0,
                    y: 2,
                    width,
                    height: height - 5,
                };
                for (margin_x, margin_y) in [(0, 0), (1, 0), (2, 0), (3, 1), (2, body.height / 4)] {
                    let area = overlay_area(body, margin_x, margin_y);
                    assert!(area.x >= body.x, "{width}x{height} left of body");
                    assert!(area.y >= body.y, "{width}x{height} above body");
                    assert!(
                        area.right() <= body.right(),
                        "{width}x{height} overruns the right edge by {}",
                        area.right() - body.right()
                    );
                    assert!(
                        area.bottom() <= body.bottom(),
                        "{width}x{height} overruns the bottom edge by {}",
                        area.bottom() - body.bottom()
                    );
                    assert!(area.width >= 1 && area.height >= 1);
                }
            }
        }
    }

    #[test]
    fn plain_words_replace_the_taxonomy_without_touching_the_machine_strings() {
        assert_eq!(
            plain_disposition(Disposition::Protected),
            "you protected this"
        );
        assert_eq!(plain_disposition(Disposition::Owned), "a rule covers this");
        assert_eq!(
            plain_disposition(Disposition::AuthorizedUnscoped),
            "you authorized this without a rule"
        );
        assert_eq!(
            plain_disposition(Disposition::Unowned),
            "no rule covers this"
        );

        // The frozen machine contract is untouched.
        assert_eq!(Disposition::Protected.to_string(), "protected");
        assert_eq!(Disposition::Owned.to_string(), "owned");
        assert_eq!(
            Disposition::AuthorizedUnscoped.to_string(),
            "authorized-unscoped"
        );
        assert_eq!(Disposition::Unowned.to_string(), "unowned");
    }

    #[test]
    fn reasons_are_split_into_the_clauses_the_planner_already_separated() {
        let clauses = reason_clauses("matched agent label agents; state age 3d meets 2h");
        assert_eq!(
            clauses,
            vec![
                "matched agent label agents".to_owned(),
                "state age 3d meets 2h".to_owned()
            ]
        );
        assert_eq!(reason_clauses("no rule matched"), vec!["no rule matched"]);
    }

    #[test]
    fn every_view_renders_at_every_supported_terminal_size() {
        let (root, paths) = temp_paths("sizes");
        let mut app = test_app(&paths, mixed_plan(), View::Review);
        for (width, height) in [(60, 20), (80, 24), (140, 42), (200, 50)] {
            let mut terminal =
                Terminal::new(TestBackend::new(width, height)).expect("test terminal");
            for view in View::ALL {
                app.view = view;
                app.selected = 0;
                terminal
                    .draw(|frame| render(frame, &mut app))
                    .expect("render view");
                let rendered = rendered_text(&terminal);
                assert!(
                    rendered.contains("docker_maid"),
                    "{view:?} at {width}x{height}"
                );
                assert!(
                    rendered.contains(view.title()),
                    "{view:?} title missing at {width}x{height}"
                );
            }
        }
        let _ = std::fs::remove_dir_all(root);
    }

    /// Colour is never the only carrier. Both lists print the plain words for
    /// the dispositions they hold.
    #[test]
    fn no_row_depends_on_colour_alone() {
        let (root, paths) = temp_paths("words");
        let mut app = test_app(&paths, mixed_plan(), View::Review);
        let mut terminal = Terminal::new(TestBackend::new(140, 42)).expect("test terminal");

        terminal
            .draw(|frame| render(frame, &mut app))
            .expect("render review");
        let review = rendered_text(&terminal);
        assert!(review.contains("WOULD REMOVE"));
        assert!(review.contains(plain_disposition(Disposition::AuthorizedUnscoped)));

        app.view = View::Keeping;
        app.selected = 0;
        terminal
            .draw(|frame| render(frame, &mut app))
            .expect("render keeping");
        let keeping = rendered_text(&terminal);
        assert!(keeping.contains("KEEPING"));
        assert!(keeping.contains(plain_disposition(Disposition::Protected)));
        assert!(keeping.contains(plain_disposition(Disposition::Unowned)));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_terminal_below_the_minimum_says_so_instead_of_drawing_a_broken_layout() {
        let (root, paths) = temp_paths("small");
        let mut app = test_app(&paths, mixed_plan(), View::Review);
        for (width, height) in [(59, 20), (60, 19), (40, 10)] {
            let mut terminal =
                Terminal::new(TestBackend::new(width, height)).expect("test terminal");
            terminal
                .draw(|frame| render(frame, &mut app))
                .expect("render small");
            let rendered = rendered_text(&terminal);
            assert!(rendered.contains("too small"), "{width}x{height}");
            assert!(rendered.contains("60"), "{width}x{height}");
            assert!(!rendered.contains("WOULD REMOVE"), "{width}x{height}");
        }
        let _ = std::fs::remove_dir_all(root);
    }

    /// A filter narrows what is read, never what is confirmed. The header says
    /// so, and the modal still lists the whole set.
    #[test]
    fn a_filter_never_narrows_the_set_a_person_confirms() {
        let (root, paths) = temp_paths("filter");
        let mut app = test_app(&paths, mixed_plan(), View::Review);
        let all_targets = app.plan_targets().len();
        app.filter = "zzz-matches-nothing".to_owned();
        assert!(app.review_rows().is_empty());
        assert_eq!(app.plan_targets().len(), all_targets);

        let mut terminal = Terminal::new(TestBackend::new(140, 42)).expect("test terminal");
        terminal
            .draw(|frame| render(frame, &mut app))
            .expect("render filtered review");
        assert!(rendered_text(&terminal).contains("hidden by the filter"));

        app.overlay = Overlay::Confirm;
        terminal
            .draw(|frame| render(frame, &mut app))
            .expect("render confirmation");
        let confirmation = rendered_text(&terminal);
        assert!(confirmation.contains("Confirm removal"));
        assert!(confirmation.contains(&format!("Remove these {all_targets} objects?")));
        assert_eq!(all_targets, 2, "this fixture must exercise the plural form");
        let _ = std::fs::remove_dir_all(root);
    }

    /// The confirmation used to compute a zero-row list on a short terminal and
    /// still offer to apply.
    #[test]
    fn the_confirmation_always_lists_at_least_one_target_it_would_remove() {
        let (root, paths) = temp_paths("confirm");
        let mut app = test_app(&paths, mixed_plan(), View::Review);
        app.overlay = Overlay::Confirm;
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).expect("test terminal");
        terminal
            .draw(|frame| render(frame, &mut app))
            .expect("render confirmation");
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("Confirm removal"));
        assert!(rendered.contains("agent-box"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn every_overlay_renders_and_the_activity_log_is_reachable() {
        let (root, paths) = temp_paths("overlays");
        let mut app = test_app(&paths, mixed_plan(), View::Review);
        let mut terminal = Terminal::new(TestBackend::new(140, 42)).expect("test terminal");
        for (overlay, expected) in [
            (Overlay::Help, "keyboard help"),
            (Overlay::Activity, "Activity log"),
            (Overlay::Confirm, "Confirm removal"),
            (Overlay::CacheConfirm, "daemon-wide build cache"),
            (Overlay::ConfigSave, "Confirm configuration write"),
        ] {
            app.overlay = overlay;
            terminal
                .draw(|frame| render(frame, &mut app))
                .expect("render overlay");
            assert!(
                rendered_text(&terminal).contains(expected),
                "{overlay:?} did not render {expected:?}"
            );
        }
        let _ = std::fs::remove_dir_all(root);
    }

    /// The log pane used to scroll past its own last line and show an empty box.
    #[test]
    fn the_activity_log_cannot_scroll_past_its_last_line() {
        let (root, paths) = temp_paths("scroll");
        let mut app = test_app(&paths, mixed_plan(), View::Review);
        app.overlay = Overlay::Activity;
        let last = overlay_line_count(&app).saturating_sub(1);
        for _ in 0..50 {
            handle_scrolling_overlay_key(
                &mut app,
                KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            );
        }
        assert_eq!(usize::from(app.overlay_scroll), last);

        handle_scrolling_overlay_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.overlay, Overlay::None);
        let _ = std::fs::remove_dir_all(root);
    }

    /// One offset serves two panes, so it has to be put back when the pane
    /// changes. Otherwise the details pane opens already scrolled, with no key
    /// in that view able to scroll it back.
    #[tokio::test]
    async fn the_pane_scroll_resets_when_the_pane_changes() {
        let (root, paths) = temp_paths("pane");
        let mut app = test_app(&paths, mixed_plan(), View::Setup);
        handle_main_key(
            &mut app,
            KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
        )
        .await
        .expect("scroll the preview");
        assert_ne!(app.pane_scroll, 0);

        handle_main_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE),
        )
        .await
        .expect("switch view");
        assert_eq!(
            app.pane_scroll, 0,
            "a view switch left the next pane scrolled"
        );

        app.pane_scroll = 7;
        handle_main_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("open details");
        assert_eq!(
            app.pane_scroll, 0,
            "the details pane opened already scrolled"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// The long generated text in Setup had a scroll offset wired into the
    /// widget that no key path could move.
    #[test]
    fn the_setup_preview_pane_can_be_scrolled() {
        let key = KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE);
        assert_eq!(setup_intent(key), Some(Intent::ScrollPreview(3)));
        assert_eq!(
            setup_intent(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)),
            Some(Intent::ScrollPreview(-3))
        );
    }

    /// The filter modal rendered the prefix buffer, so it showed whatever the
    /// last unrelated edit left behind.
    #[test]
    fn the_filter_editor_shows_the_filter_and_not_a_stale_buffer() {
        let (root, paths) = temp_paths("editor");
        let mut app = test_app(&paths, mixed_plan(), View::Keeping);
        app.prefix_input = "left-over-prefix".to_owned();
        app.filter = "agent".to_owned();
        app.editor = Editor::Filter;
        let mut terminal = Terminal::new(TestBackend::new(140, 42)).expect("test terminal");
        terminal
            .draw(|frame| render(frame, &mut app))
            .expect("render filter editor");
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("Narrow this list"));
        assert!(rendered.contains("agent"));
        assert!(!rendered.contains("left-over-prefix"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn policy_editor_is_visible_in_an_eighty_column_terminal() {
        let (root, paths) = temp_paths("policy");
        let mut app = test_app(&paths, mixed_plan(), View::Setup);
        app.built_in_config = true;
        app.start_policy_editor();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("test terminal");

        terminal
            .draw(|frame| render(frame, &mut app))
            .expect("render editor");
        let rendered = rendered_text(&terminal);

        assert!(rendered.contains("Edit policy value"));
        assert!(rendered.contains("2h"));
        assert!(rendered.contains("Enter save"));
        assert!(rendered.contains("Esc cancel"));

        app.prefix_input = "not-a-duration".to_owned();
        handle_policy_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.editor, Editor::Policy);
        assert_eq!(app.configure_policy.stopped_container_ttl, "2h");
        assert!(app.status.starts_with("Invalid value:"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn the_family_action_writes_one_typed_label_entry_and_toggles_it() {
        let (root, paths) = temp_paths("family");
        let mut app = test_app(&paths, fixture_plan(), View::Review);

        // The action writes typed state and then refreshes. The refresh needs
        // Docker, whose availability this unit test must not depend on, so the
        // durable write is what is asserted.
        let _ = app.toggle_family_protection().await;
        let entries = ProtectionStore::new(paths.clone())
            .snapshot()
            .expect("read protection")
            .entries;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, ProtectionKind::Label);
        assert_eq!(entries[0].value, "ai-agent.owner=test");

        // Pressing it again releases the same family. A successful refresh
        // replaces the synthetic plan with the live daemon's, so restore the
        // fixture the action reads from.
        app.protection = ProtectionState::default();
        app.plan = fixture_plan();
        app.selected = 0;
        let _ = app.toggle_family_protection().await;
        assert!(ProtectionStore::new(paths)
            .snapshot()
            .expect("read protection")
            .entries
            .is_empty());

        // A resource with no family label is refused with a pointer to `space`.
        // The previous leg refreshed too, so restore the fixture here as well.
        // Without this the assertion quietly depends on the ambient daemon
        // holding at least one removal: against an empty daemon the live plan
        // carries only kept built-in networks, no row survives the Review
        // filter, and the action reports "Nothing is selected" instead of the
        // message this leg is about.
        app.plan = fixture_plan();
        app.selected = 0;
        app.plan.decisions[0].resource.labels.clear();
        app.toggle_family_protection()
            .await
            .expect("an unlabelled object is a status message, not an error");
        assert!(app
            .status
            .contains("carries no Compose or agent family label"));

        std::fs::remove_dir_all(root).expect("remove test directory");
    }

    /// One container and one volume in the same `ai-agent.owner=test` family.
    fn fixture_plan() -> Plan {
        let mut volume = sample_decision();
        volume.resource.kind = ResourceKind::Volume;
        volume.resource.id = "vol".to_owned();
        Plan {
            decisions: vec![sample_decision(), volume],
        }
    }

    /// One removal, one protected keep, one unowned keep, and one removal with
    /// no ownership evidence, so both lists have something of every colour.
    fn mixed_plan() -> Plan {
        let mut protected = sample_decision();
        protected.resource.kind = ResourceKind::Volume;
        protected.resource.id = "vol".to_owned();
        protected.resource.name = "keep-me".to_owned();
        protected.disposition = Disposition::Protected;
        protected.action = Action::Keep;
        protected.matched_rule = None;
        protected.reason = "protected by runtime state".to_owned();

        let mut unowned = sample_decision();
        unowned.resource.kind = ResourceKind::Network;
        unowned.resource.id = "net".to_owned();
        unowned.resource.name = "bridge".to_owned();
        unowned.disposition = Disposition::Unowned;
        unowned.action = Action::Keep;
        unowned.matched_rule = None;
        unowned.reason = "no rule matched".to_owned();

        let mut unscoped = sample_decision();
        unscoped.resource.kind = ResourceKind::BuildCache;
        unscoped.resource.id = "cache".to_owned();
        unscoped.resource.name = "build-cache".to_owned();
        unscoped.disposition = Disposition::AuthorizedUnscoped;
        unscoped.reason = "matched build cache; older than 30d".to_owned();

        Plan {
            decisions: vec![sample_decision(), protected, unowned, unscoped],
        }
    }

    #[test]
    fn family_protection_targets_the_whole_family_and_never_build_cache() {
        let mut volume = sample_decision();
        volume.resource.kind = ResourceKind::Volume;
        volume.resource.id = "vol".to_owned();
        let mut other = sample_decision();
        other.resource.kind = ResourceKind::Network;
        other.resource.id = "net".to_owned();
        other
            .resource
            .labels
            .insert("ai-agent.owner".to_owned(), "different".to_owned());
        let mut cache = sample_decision();
        cache.resource.kind = ResourceKind::BuildCache;
        cache.resource.id = "cache".to_owned();
        cache.resource.labels.clear();

        let plan = Plan {
            decisions: vec![sample_decision(), volume, other, cache],
        };
        let (key, value) = family_label(&plan.decisions[0].resource).expect("family label");
        let (pair, members) = family_protection_target(&plan, key, value);
        assert_eq!(pair, "ai-agent.owner=test");
        // The container and the volume, not the other owner and not the cache.
        assert_eq!(members, 2);
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
    fn configure_navigation_uses_display_order_but_stores_canonical_indexes() {
        let agent = sample_decision().resource;
        let mut compose = agent.clone();
        compose.id = "compose".to_owned();
        compose.name = "project-web".to_owned();
        compose.search_names = vec![compose.name.clone()];
        compose.labels = BTreeMap::from([(
            "com.docker.compose.project".to_owned(),
            "project".to_owned(),
        )]);
        let mut cache = agent.clone();
        cache.kind = ResourceKind::BuildCache;
        cache.id = "cache".to_owned();
        cache.name = "cache".to_owned();
        cache.search_names = vec![cache.name.clone()];
        cache.labels.clear();
        cache.state = ResourceState::Available;
        let survey = survey_inventory(&[compose, agent, cache]);

        assert!(survey.candidates[0].id.starts_with("agent-label/"));
        assert_eq!(survey.candidates[1].id, "build-cache");
        assert!(survey.candidates[2].id.starts_with("compose/"));
        assert_eq!(first_configure_row(&survey), 0);
        assert_eq!(move_configure_row(&survey, 0, 1), 2);
        assert_eq!(move_configure_row(&survey, 2, 1), 1);
        assert_eq!(move_configure_row(&survey, 1, -1), 2);
    }

    #[test]
    fn resource_detail_exposes_policy_reason_and_labels() {
        let decision = sample_decision();
        let detail = resource_detail(&decision);
        assert!(detail.contains("matched agent label"));
        assert!(detail.contains("ai-agent.owner=test"));
        assert!(detail.contains("agents"));
        assert!(detail.contains("workspace → /workspace"));
        assert!(detail.contains("a rule covers this"));
    }

    /// Detail panes used to print epoch integers and Rust booleans straight
    /// out of the struct.
    #[test]
    fn no_pane_shows_a_person_a_raw_epoch_or_a_rust_boolean() {
        assert_eq!(format_timestamp(1_787_013_961), "2026-08-18T00:46:01Z");
        assert_eq!(format_timestamp(-1), "unknown time");
        assert_eq!(yes_no(true), "yes");
        assert_eq!(yes_no(false), "no");

        let detail = resource_detail(&sample_decision());
        assert!(detail.contains("In use: no"));
        assert!(detail.contains("Dangling: no"));
        assert!(
            !detail.contains("false"),
            "a Rust boolean reached the screen"
        );
        assert!(
            !detail.contains("true"),
            "a Rust boolean reached the screen"
        );
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
        let mut app = test_app(&paths, plan, View::Review);
        app.explicit_config = Some(config_path.clone());
        app.loaded = LoadedConfig {
            path: config_path.clone(),
            config,
            source: source.to_owned(),
        };
        std::fs::write(&config_path, format!("{source}# changed\n")).expect("change config");

        app.apply_confirmed_plan()
            .await
            .expect("stale plan is handled in the TUI");

        assert_eq!(app.plan_validity, PlanValidity::Stale);
        assert!(app.status.contains("Configuration changed"));
        assert!(!paths.activity_file().exists());
        std::fs::remove_file(config_path).expect("remove config");
        std::fs::remove_dir_all(root).expect("remove test root");
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
