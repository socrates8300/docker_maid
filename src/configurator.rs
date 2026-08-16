//! Deterministic, evidence-backed configuration proposals and durable writes.

use crate::activity::stable_config_hash;
use crate::config::{
    BuildCacheRule, CommonRule, Config, ConfigError, ContainerRule, ImageRule, NetworkRule,
    RuleScope, Rules, Selectors, VolumeRule,
};
use crate::plan::{build_plan, Action, InventoryItem, ResourceKind, ResourceState};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub const CONFIGURATOR_SCHEMA_VERSION: u32 = 1;
pub const MANAGED_ID_PREFIX: &str = "docker-maid.configure/";
const MANAGED_START: &str = "# >>> docker_maid managed rules; edit with `docker_maid tui` >>>";
const MANAGED_END: &str = "# <<< docker_maid managed rules <<<";
static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub enum ConfiguratorError {
    Invalid(String),
    Stale(String),
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Config(ConfigError),
}

impl fmt::Display for ConfiguratorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) | Self::Stale(message) => formatter.write_str(message),
            Self::Io { path, source } => write!(formatter, "{}: {source}", path.display()),
            Self::Config(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ConfiguratorError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Config(error) => Some(error),
            Self::Invalid(_) | Self::Stale(_) => None,
        }
    }
}

impl From<ConfigError> for ConfiguratorError {
    fn from(error: ConfigError) -> Self {
        Self::Config(error)
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PolicyProfile {
    SharedHost,
    Workstation,
    EphemeralCi,
}

impl PolicyProfile {
    pub const ALL: [Self; 3] = [Self::SharedHost, Self::Workstation, Self::EphemeralCi];

    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::SharedHost => "Shared Host",
            Self::Workstation => "Workstation",
            Self::EphemeralCi => "Ephemeral CI",
        }
    }

    const fn container_ttl(self) -> &'static str {
        match self {
            Self::SharedHost => "24h",
            Self::Workstation => "2h",
            Self::EphemeralCi => "15m",
        }
    }

    const fn image_ttl(self) -> &'static str {
        match self {
            Self::SharedHost => "7d",
            Self::Workstation => "24h",
            Self::EphemeralCi => "1h",
        }
    }

    const fn volume_ttl(self) -> &'static str {
        match self {
            Self::SharedHost => "14d",
            Self::Workstation => "48h",
            Self::EphemeralCi => "6h",
        }
    }

    const fn cache_ttl(self) -> &'static str {
        match self {
            Self::SharedHost => "30d",
            Self::Workstation => "7d",
            Self::EphemeralCi => "24h",
        }
    }

    const fn cache_bytes(self) -> u64 {
        match self {
            Self::SharedHost => 20 * 1024 * 1024 * 1024,
            Self::Workstation => 10 * 1024 * 1024 * 1024,
            Self::EphemeralCi => 5 * 1024 * 1024 * 1024,
        }
    }

    #[must_use]
    pub fn settings(self) -> PolicySettings {
        PolicySettings {
            stopped_container_ttl: self.container_ttl().to_owned(),
            image_ttl: self.image_ttl().to_owned(),
            volume_ttl: self.volume_ttl().to_owned(),
            build_cache_ttl: self.cache_ttl().to_owned(),
            build_cache_max_bytes: self.cache_bytes(),
        }
    }
}

impl fmt::Display for PolicyProfile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::SharedHost => "shared-host",
            Self::Workstation => "workstation",
            Self::EphemeralCi => "ephemeral-ci",
        })
    }
}

impl FromStr for PolicyProfile {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "shared-host" => Ok(Self::SharedHost),
            "workstation" => Ok(Self::Workstation),
            "ephemeral-ci" => Ok(Self::EphemeralCi),
            _ => Err(format!(
                "unknown profile {value:?}; expected shared-host, workstation, or ephemeral-ci"
            )),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PolicySettings {
    pub stopped_container_ttl: String,
    pub image_ttl: String,
    pub volume_ttl: String,
    pub build_cache_ttl: String,
    pub build_cache_max_bytes: u64,
}

impl PolicySettings {
    /// Validate every editable policy value.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero or invalid duration, or a zero cache budget.
    pub fn validate(&self) -> Result<(), ConfiguratorError> {
        for (field, value) in [
            ("stopped container TTL", &self.stopped_container_ttl),
            ("image TTL", &self.image_ttl),
            ("volume TTL", &self.volume_ttl),
            ("build-cache TTL", &self.build_cache_ttl),
        ] {
            let duration = humantime::parse_duration(value).map_err(|error| {
                ConfiguratorError::Invalid(format!("{field} is invalid: {error}"))
            })?;
            if duration.is_zero() {
                return Err(ConfiguratorError::Invalid(format!(
                    "{field} must be greater than zero"
                )));
            }
        }
        if self.build_cache_max_bytes == 0 {
            return Err(ConfiguratorError::Invalid(
                "build-cache byte budget must be greater than zero".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CandidateSelector {
    ExactLabel {
        key: String,
        value: String,
    },
    NamePrefix {
        resource_kind: String,
        prefix: String,
    },
    BuildCache,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CandidateResource {
    pub resource_kind: String,
    pub id: String,
    pub name: String,
    pub size: Option<u64>,
    pub referenced: bool,
    pub running: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CandidateFamily {
    pub id: String,
    pub title: String,
    pub evidence: String,
    pub selector: CandidateSelector,
    pub resources: Vec<CandidateResource>,
    pub known_bytes: u64,
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SurveySummary {
    pub total_resources: usize,
    pub candidate_resources: usize,
    pub unowned_resources: usize,
    pub known_bytes: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConfiguratorSurvey {
    pub schema_version: u32,
    pub snapshot_id: String,
    pub candidates: Vec<CandidateFamily>,
    pub summary: SurveySummary,
}

/// Return canonical candidate indexes in their human-facing display order.
///
/// The survey vector is part of the deterministic machine document. Callers
/// must render through this index map instead of reordering that vector.
#[must_use]
pub fn candidate_display_indices(candidates: &[CandidateFamily]) -> Vec<usize> {
    let mut indices = (0..candidates.len()).collect::<Vec<_>>();
    indices.sort_by_key(|index| (candidate_display_rank(&candidates[*index]), *index));
    indices
}

/// Recompute policy-specific warnings without changing candidate identity or
/// canonical order.
///
/// The current inventory and clock decide whether a family truthfully claims a
/// zero-removal preview or must state its current pending count.
pub fn refresh_candidate_warnings(
    survey: &mut ConfiguratorSurvey,
    policy: &PolicySettings,
    inventory: &[InventoryItem],
    now_epoch_seconds: i64,
) {
    for candidate in &mut survey.candidates {
        if is_compose_candidate(candidate) {
            candidate.warning = Some(compose_future_cleanup_warning(
                candidate,
                policy,
                inventory,
                now_epoch_seconds,
            ));
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProposalPreview {
    pub before_pending: usize,
    pub after_pending: usize,
    pub newly_pending: usize,
    pub selected_resources: usize,
    pub estimated_reclaim_bytes: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConfigProposal {
    pub schema_version: u32,
    pub proposal_id: String,
    pub snapshot_id: String,
    pub inventory_signature: String,
    pub target_path: PathBuf,
    pub source_existed: bool,
    pub base_source_hash: String,
    pub result_source_hash: String,
    pub profile: PolicyProfile,
    pub policy: PolicySettings,
    pub candidate_ids: Vec<String>,
    pub generated_rule_ids: Vec<String>,
    pub preview: ProposalPreview,
    pub warnings: Vec<String>,
    pub resulting_source: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConfigWriteResult {
    pub schema_version: u32,
    pub proposal_id: String,
    pub path: PathBuf,
    pub backup_path: Option<PathBuf>,
    pub source_hash: String,
}

#[derive(Clone, Copy)]
pub struct ProposalRequest<'a> {
    pub base_source: &'a str,
    pub source_existed: bool,
    pub target_path: &'a Path,
    pub survey: &'a ConfiguratorSurvey,
    pub inventory: &'a [InventoryItem],
    pub profile: PolicyProfile,
    pub policy: Option<&'a PolicySettings>,
    pub candidate_ids: &'a [String],
    pub now_epoch_seconds: i64,
}

#[derive(Default)]
struct CandidateAccumulator {
    title: String,
    evidence: String,
    selector: Option<CandidateSelector>,
    resources: BTreeMap<(String, String), CandidateResource>,
    warning: Option<String>,
}

/// Derive selectable ownership families from stable Docker evidence.
#[must_use]
pub fn survey_inventory(inventory: &[InventoryItem]) -> ConfiguratorSurvey {
    let mut groups = BTreeMap::<String, CandidateAccumulator>::new();
    for item in inventory {
        for (key, value) in &item.labels {
            if key == "com.docker.compose.project" && !value.trim().is_empty() {
                let identity = format!("{key}={value}");
                add_candidate_resource(
                    &mut groups,
                    &format!("compose/{}-{}", slug(value), short_hash(&identity)),
                    format!("Compose project {value}"),
                    format!("exact Docker label {identity}"),
                    CandidateSelector::ExactLabel {
                        key: key.clone(),
                        value: value.clone(),
                    },
                    item,
                    false,
                );
            }
            if is_agent_label(key) {
                let identity = format!("{key}={value}");
                add_candidate_resource(
                    &mut groups,
                    &format!("agent-label/{}-{}", slug(key), short_hash(&identity)),
                    format!("Agent label {identity}"),
                    format!("exact known agent label {identity}"),
                    CandidateSelector::ExactLabel {
                        key: key.clone(),
                        value: value.clone(),
                    },
                    item,
                    false,
                );
            }
        }
    }

    let cache_items = inventory
        .iter()
        .filter(|item| item.kind == ResourceKind::BuildCache)
        .collect::<Vec<_>>();
    if !cache_items.is_empty() {
        for item in cache_items {
            add_candidate_resource(
                &mut groups,
                "build-cache",
                "Docker build cache".to_owned(),
                "Docker exposes no ownership labels or names".to_owned(),
                CandidateSelector::BuildCache,
                item,
                true,
            );
        }
    }

    finish_survey(inventory, groups)
}

/// Add an operator-chosen name prefix to an existing survey.
///
/// # Errors
///
/// Returns an error for a blank prefix, build cache, or a prefix that matches
/// no current resource.
pub fn add_name_prefix_candidate(
    survey: &mut ConfiguratorSurvey,
    inventory: &[InventoryItem],
    kind: ResourceKind,
    prefix: &str,
) -> Result<String, ConfiguratorError> {
    if prefix.trim().is_empty() {
        return Err(ConfiguratorError::Invalid(
            "name prefix must not be blank".to_owned(),
        ));
    }
    if kind == ResourceKind::BuildCache {
        return Err(ConfiguratorError::Invalid(
            "build cache has no name-prefix ownership surface".to_owned(),
        ));
    }
    let id = format!(
        "name-prefix/{}/{}-{}",
        kind,
        slug(prefix),
        short_hash(&format!("{kind}:{prefix}"))
    );
    let resources = inventory
        .iter()
        .filter(|item| {
            item.kind == kind
                && item
                    .search_names
                    .iter()
                    .any(|name| name.starts_with(prefix))
        })
        .map(candidate_resource)
        .collect::<Vec<_>>();
    if resources.is_empty() {
        return Err(ConfiguratorError::Invalid(format!(
            "prefix {prefix:?} matches no current {kind} resource"
        )));
    }
    let known_bytes = resources
        .iter()
        .filter_map(|resource| resource.size)
        .fold(0u64, u64::saturating_add);
    survey.candidates.retain(|candidate| candidate.id != id);
    survey.candidates.push(CandidateFamily {
        id: id.clone(),
        title: format!("{kind} names beginning {prefix:?}"),
        evidence: "operator-selected prefix; no automatic name generalization".to_owned(),
        selector: CandidateSelector::NamePrefix {
            resource_kind: kind.to_string(),
            prefix: prefix.to_owned(),
        },
        resources,
        known_bytes,
        warning: None,
    });
    survey
        .candidates
        .sort_by(|left, right| left.id.cmp(&right.id));
    let candidate_resources = survey
        .candidates
        .iter()
        .flat_map(|candidate| &candidate.resources)
        .map(|resource| (resource.resource_kind.clone(), resource.id.clone()))
        .collect::<BTreeSet<_>>()
        .len();
    survey.summary.candidate_resources = candidate_resources;
    survey.summary.unowned_resources = survey
        .summary
        .total_resources
        .saturating_sub(candidate_resources);
    survey.snapshot_id = survey_signature(&survey.candidates);
    Ok(id)
}

/// Create a reviewed config proposal without writing the filesystem.
///
/// # Errors
///
/// Returns an error when candidates overlap, a selected candidate is absent,
/// a manual rule would be overwritten, or the resulting config is invalid.
pub fn propose_configuration(
    request: &ProposalRequest<'_>,
) -> Result<ConfigProposal, ConfiguratorError> {
    let ProposalRequest {
        base_source,
        source_existed,
        target_path,
        survey,
        inventory,
        profile,
        policy,
        candidate_ids,
        now_epoch_seconds,
    } = *request;
    let policy = policy.cloned().unwrap_or_else(|| profile.settings());
    policy.validate()?;
    if candidate_ids.is_empty() {
        return Err(ConfiguratorError::Invalid(
            "select at least one discovered candidate".to_owned(),
        ));
    }
    let manual_source = strip_managed_region(base_source)?;
    let manual_config = parse_config_source(&manual_source, target_path)?;
    reject_managed_ids_outside_region(&manual_config)?;

    let selected_ids = candidate_ids.iter().cloned().collect::<BTreeSet<_>>();
    if selected_ids.len() != candidate_ids.len() {
        return Err(ConfiguratorError::Invalid(
            "candidate IDs must not be repeated".to_owned(),
        ));
    }
    let candidates_by_id = survey
        .candidates
        .iter()
        .map(|candidate| (candidate.id.as_str(), candidate))
        .collect::<BTreeMap<_, _>>();
    let selected = selected_ids
        .iter()
        .map(|id| {
            candidates_by_id.get(id.as_str()).copied().ok_or_else(|| {
                ConfiguratorError::Invalid(format!(
                    "candidate {id:?} is not present in snapshot {}",
                    survey.snapshot_id
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    reject_overlaps(&selected)?;

    let generated_rules = generated_rules(&manual_config, &selected, &policy)?;
    let generated_rule_ids = rule_ids(&generated_rules);
    let resulting_source = render_managed_source(&manual_source, &generated_rules)?;
    let result_config = Config::parse(&resulting_source, target_path)?;
    result_config.validate()?;

    let preview = proposal_preview(
        &manual_config,
        &result_config,
        inventory,
        now_epoch_seconds,
        &selected,
        &generated_rule_ids,
    )?;
    let warnings = selected_candidate_warnings(&selected, &policy, inventory, now_epoch_seconds);
    let inventory_signature = inventory_signature(inventory);
    let mut sorted_candidate_ids = selected_ids.into_iter().collect::<Vec<_>>();
    sorted_candidate_ids.sort();
    let result_source_hash = stable_config_hash(&resulting_source);
    let proposal_seed = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        survey.snapshot_id,
        inventory_signature,
        target_path.display(),
        profile,
        sorted_candidate_ids.join("\n"),
        result_source_hash
    );
    Ok(ConfigProposal {
        schema_version: CONFIGURATOR_SCHEMA_VERSION,
        proposal_id: format!("proposal-{}", stable_config_hash(&proposal_seed)),
        snapshot_id: survey.snapshot_id.clone(),
        inventory_signature,
        target_path: target_path.to_path_buf(),
        source_existed,
        base_source_hash: stable_config_hash(base_source),
        result_source_hash,
        profile,
        policy,
        candidate_ids: sorted_candidate_ids,
        generated_rule_ids,
        preview,
        warnings,
        resulting_source,
    })
}

/// Compare-and-swap a validated proposal into place using a sibling lock.
///
/// # Errors
///
/// Returns an error when the source or inventory changed, the proposal is
/// malformed, or the durable write fails.
pub fn write_proposal(
    proposal: &ConfigProposal,
    current_inventory: &[InventoryItem],
) -> Result<ConfigWriteResult, ConfiguratorError> {
    if proposal.schema_version != CONFIGURATOR_SCHEMA_VERSION {
        return Err(ConfiguratorError::Invalid(format!(
            "unsupported proposal schema_version {}; expected {CONFIGURATOR_SCHEMA_VERSION}",
            proposal.schema_version
        )));
    }
    if inventory_signature(current_inventory) != proposal.inventory_signature {
        return Err(ConfiguratorError::Stale(
            "Docker inventory changed after proposal creation; refresh and review again".to_owned(),
        ));
    }
    let current_source = match fs::read_to_string(&proposal.target_path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(source) => return Err(io_error(&proposal.target_path, source)),
    };
    if stable_config_hash(&current_source) != proposal.base_source_hash {
        return Err(ConfiguratorError::Stale(format!(
            "configuration changed after proposal creation: {}",
            proposal.target_path.display()
        )));
    }
    if stable_config_hash(&proposal.resulting_source) != proposal.result_source_hash {
        return Err(ConfiguratorError::Invalid(
            "proposal payload does not match its result hash".to_owned(),
        ));
    }
    let parsed = Config::parse(&proposal.resulting_source, &proposal.target_path)?;
    parsed.validate()?;
    durable_write(proposal, &current_source)
}

#[must_use]
pub fn inventory_signature(inventory: &[InventoryItem]) -> String {
    let mut rows = inventory
        .iter()
        .map(|item| {
            let labels = item
                .labels
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join(",");
            format!(
                "{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
                item.kind,
                item.id,
                item.name,
                item.state,
                item.created_at
                    .map_or_else(|| "-".to_owned(), |value| value.to_string()),
                item.state_since
                    .map_or_else(|| "-".to_owned(), |value| value.to_string()),
                item.size
                    .map_or_else(|| "-".to_owned(), |value| value.to_string()),
                item.referenced,
                item.system,
                labels
            )
        })
        .collect::<Vec<_>>();
    rows.sort();
    stable_config_hash(&rows.join("\n"))
}

/// Select the config file that a configurator write will own.
///
/// Explicit and already-loaded paths win. A new configuration is written to
/// `XDG_CONFIG_HOME`, then `HOME/.config`.
///
/// # Errors
///
/// Returns an error when no explicit or loaded path exists and neither
/// environment root is available.
pub fn configuration_target_path(
    explicit: Option<&Path>,
    loaded: Option<&Path>,
    xdg_config_home: Option<&Path>,
    home: Option<&Path>,
) -> Result<PathBuf, ConfiguratorError> {
    if let Some(path) = explicit.or(loaded) {
        return Ok(path.to_path_buf());
    }
    if let Some(root) = xdg_config_home {
        return Ok(root.join("docker_maid/config.toml"));
    }
    home.map(|root| root.join(".config/docker_maid/config.toml"))
        .ok_or_else(|| {
            ConfiguratorError::Invalid(
                "cannot select a configuration path: set XDG_CONFIG_HOME or HOME".to_owned(),
            )
        })
}

fn proposal_preview(
    manual_config: &Config,
    result_config: &Config,
    inventory: &[InventoryItem],
    now_epoch_seconds: i64,
    selected: &[&CandidateFamily],
    generated_rule_ids: &[String],
) -> Result<ProposalPreview, ConfiguratorError> {
    let before =
        build_plan(manual_config, inventory.to_vec(), now_epoch_seconds).map_err(|error| {
            ConfiguratorError::Invalid(format!("cannot preview current policy: {error}"))
        })?;
    let after =
        build_plan(result_config, inventory.to_vec(), now_epoch_seconds).map_err(|error| {
            ConfiguratorError::Invalid(format!("cannot preview proposed policy: {error}"))
        })?;
    reject_shadowed_rules(selected, generated_rule_ids, &after)?;
    let before_ids = pending_ids(&before);
    let after_targets = after
        .decisions
        .iter()
        .filter(|decision| decision.action == Action::Remove)
        .collect::<Vec<_>>();
    let newly_pending = after_targets
        .iter()
        .filter(|decision| {
            !before_ids.contains(&(decision.resource.kind, decision.resource.id.clone()))
        })
        .count();
    let estimated_reclaim_bytes = after_targets
        .iter()
        .filter_map(|decision| decision.resource.size)
        .fold(0u64, u64::saturating_add);
    let selected_resources = selected
        .iter()
        .flat_map(|candidate| &candidate.resources)
        .map(|resource| (&resource.resource_kind, &resource.id))
        .collect::<BTreeSet<_>>()
        .len();
    Ok(ProposalPreview {
        before_pending: before.pending_count(),
        after_pending: after.pending_count(),
        newly_pending,
        selected_resources,
        estimated_reclaim_bytes,
    })
}

fn finish_survey(
    inventory: &[InventoryItem],
    groups: BTreeMap<String, CandidateAccumulator>,
) -> ConfiguratorSurvey {
    let candidates = groups
        .into_iter()
        .filter_map(|(id, group)| {
            let selector = group.selector?;
            let resources = group.resources.into_values().collect::<Vec<_>>();
            let known_bytes = resources
                .iter()
                .filter_map(|resource| resource.size)
                .fold(0u64, u64::saturating_add);
            Some(CandidateFamily {
                id,
                title: group.title,
                evidence: group.evidence,
                selector,
                resources,
                known_bytes,
                warning: group.warning,
            })
        })
        .collect::<Vec<_>>();
    let candidate_resource_keys = candidates
        .iter()
        .flat_map(|candidate| &candidate.resources)
        .map(|resource| (resource.resource_kind.clone(), resource.id.clone()))
        .collect::<BTreeSet<_>>();
    ConfiguratorSurvey {
        schema_version: CONFIGURATOR_SCHEMA_VERSION,
        snapshot_id: survey_signature(&candidates),
        candidates,
        summary: SurveySummary {
            total_resources: inventory.len(),
            candidate_resources: candidate_resource_keys.len(),
            unowned_resources: inventory
                .len()
                .saturating_sub(candidate_resource_keys.len()),
            known_bytes: inventory
                .iter()
                .filter_map(|item| item.size)
                .fold(0u64, u64::saturating_add),
        },
    }
}

fn survey_signature(candidates: &[CandidateFamily]) -> String {
    let mut rows = candidates
        .iter()
        .flat_map(|candidate| {
            candidate.resources.iter().map(move |resource| {
                format!(
                    "{}|{}|{}|{}|{:?}",
                    candidate.id,
                    resource.resource_kind,
                    resource.id,
                    resource.name,
                    candidate.selector
                )
            })
        })
        .collect::<Vec<_>>();
    rows.sort();
    stable_config_hash(&rows.join("\n"))
}

fn add_candidate_resource(
    groups: &mut BTreeMap<String, CandidateAccumulator>,
    id: &str,
    title: String,
    evidence: String,
    selector: CandidateSelector,
    item: &InventoryItem,
    warning: bool,
) {
    if matches!(selector, CandidateSelector::ExactLabel { ref key, .. } if key == "com.docker.compose.project")
        && item.kind == ResourceKind::Image
    {
        return;
    }
    let group = groups.entry(id.to_owned()).or_default();
    group.title = title;
    group.evidence = evidence;
    group.selector = Some(selector);
    if warning {
        group.warning =
            Some("WARNING: build cache is unscoped; Docker exposes no owner metadata".to_owned());
    }
    let resource = candidate_resource(item);
    group.resources.insert(
        (resource.resource_kind.clone(), resource.id.clone()),
        resource,
    );
}

fn candidate_resource(item: &InventoryItem) -> CandidateResource {
    CandidateResource {
        resource_kind: item.kind.to_string(),
        id: item.id.clone(),
        name: item.name.clone(),
        size: item.size,
        referenced: item.referenced,
        running: item.state == ResourceState::Running,
    }
}

fn is_agent_label(key: &str) -> bool {
    key.starts_with("ai-agent.")
        || key.starts_with("devcontainer.")
        || key == "dev.docker-maid.managed"
}

fn candidate_display_rank(candidate: &CandidateFamily) -> u8 {
    match &candidate.selector {
        CandidateSelector::ExactLabel { key, .. } if key == "com.docker.compose.project" => 1,
        CandidateSelector::ExactLabel { .. } => 0,
        CandidateSelector::NamePrefix { .. } => 2,
        CandidateSelector::BuildCache => 3,
    }
}

fn is_compose_candidate(candidate: &CandidateFamily) -> bool {
    matches!(
        &candidate.selector,
        CandidateSelector::ExactLabel { key, .. } if key == "com.docker.compose.project"
    )
}

fn selected_candidate_warnings(
    selected: &[&CandidateFamily],
    policy: &PolicySettings,
    inventory: &[InventoryItem],
    now_epoch_seconds: i64,
) -> Vec<String> {
    selected
        .iter()
        .filter_map(|candidate| {
            if is_compose_candidate(candidate) {
                Some(compose_future_cleanup_warning(
                    candidate,
                    policy,
                    inventory,
                    now_epoch_seconds,
                ))
            } else {
                candidate.warning.clone()
            }
        })
        .collect()
}

fn compose_future_cleanup_warning(
    candidate: &CandidateFamily,
    policy: &PolicySettings,
    inventory: &[InventoryItem],
    now_epoch_seconds: i64,
) -> String {
    let mut rules = Rules::default();
    if append_candidate_rules(&mut rules, candidate, policy).is_err() {
        return "WARNING: This Compose family has an invalid generated rule shape; do not write it"
            .to_owned();
    }
    sort_rules(&mut rules);
    let family_config = Config {
        rules: rules.clone(),
        ..Config::default()
    };
    let Ok(family_plan) = build_plan(&family_config, inventory.to_vec(), now_epoch_seconds) else {
        return "WARNING: This Compose family has an invalid generated rule shape; do not write it"
            .to_owned();
    };
    let pending_now = family_plan
        .decisions
        .iter()
        .filter(|decision| decision.action == Action::Remove)
        .count();

    let mut effects = Vec::new();
    for rule in &rules.containers {
        if let Some(ttl) = &rule.stopped_ttl {
            effects.push(format!("stopped containers become eligible after {ttl}"));
        }
    }
    for rule in &rules.images {
        if let Some(ttl) = &rule.unused_for {
            effects.push(format!(
                "unreferenced images become eligible when their resource age exceeds {ttl}"
            ));
        }
    }
    for rule in &rules.volumes {
        if let Some(ttl) = &rule.orphan_for {
            effects.push(format!(
                "detached volumes become eligible immediately when their resource age already exceeds {ttl}"
            ));
        }
    }
    for rule in &rules.networks {
        if rule.orphan {
            effects.push("empty networks become eligible immediately".to_owned());
        }
    }

    let effect_text = if effects.is_empty() {
        "no cleanup rule is generated for the currently observed resource kinds".to_owned()
    } else {
        effects.join("; ")
    };
    if pending_now == 0 {
        format!(
            "WARNING: Running and referenced Compose resources stay, so this stack can preview zero removals now. After `docker compose down` or another detach, the generated rules apply: {effect_text}. Preview the plan again before applying."
        )
    } else {
        format!(
            "WARNING: This stack currently previews {pending_now} pending removal(s) under the generated rules. After `docker compose down` or another detach, the generated rules apply: {effect_text}. Preview the plan again before applying."
        )
    }
}

fn generated_rules(
    manual: &Config,
    candidates: &[&CandidateFamily],
    policy: &PolicySettings,
) -> Result<Rules, ConfiguratorError> {
    let mut rules = Rules::default();
    for candidate in candidates {
        match &candidate.selector {
            CandidateSelector::BuildCache => {
                if manual.rules.build_cache.is_some() {
                    return Err(ConfiguratorError::Invalid(
                        "a manual build-cache policy already exists; the configurator will not overwrite it"
                            .to_owned(),
                    ));
                }
                rules.build_cache = Some(BuildCacheRule {
                    id: Some(format!("{MANAGED_ID_PREFIX}build-cache")),
                    older_than: Some(policy.build_cache_ttl.clone()),
                    max_bytes: Some(policy.build_cache_max_bytes),
                    allow_unscoped: true,
                });
            }
            CandidateSelector::ExactLabel { .. } | CandidateSelector::NamePrefix { .. } => {
                append_candidate_rules(&mut rules, candidate, policy)?;
            }
        }
    }
    sort_rules(&mut rules);
    Ok(rules)
}

fn append_candidate_rules(
    rules: &mut Rules,
    candidate: &CandidateFamily,
    policy: &PolicySettings,
) -> Result<(), ConfiguratorError> {
    match &candidate.selector {
        CandidateSelector::ExactLabel { key, value } => {
            let selector = Selectors {
                labels: vec![escape_glob(&format!("{key}={value}"))],
                ..Selectors::default()
            };
            let kinds = candidate
                .resources
                .iter()
                .filter_map(|resource| parse_resource_kind(&resource.resource_kind))
                .collect::<BTreeSet<_>>();
            for kind in kinds {
                push_managed_rule(rules, candidate, kind, selector.clone(), policy);
            }
            Ok(())
        }
        CandidateSelector::NamePrefix {
            resource_kind,
            prefix,
        } => {
            let kind = parse_resource_kind(resource_kind).ok_or_else(|| {
                ConfiguratorError::Invalid(format!(
                    "candidate {} has unknown resource kind {resource_kind:?}",
                    candidate.id
                ))
            })?;
            let selector = Selectors {
                names: vec![format!("^{}", regex::escape(prefix))],
                ..Selectors::default()
            };
            push_managed_rule(rules, candidate, kind, selector, policy);
            Ok(())
        }
        CandidateSelector::BuildCache => Ok(()),
    }
}

fn push_managed_rule(
    rules: &mut Rules,
    candidate: &CandidateFamily,
    kind: ResourceKind,
    selector: Selectors,
    policy: &PolicySettings,
) {
    let id = format!("{MANAGED_ID_PREFIX}{}/{}", candidate.id, kind);
    let common = CommonRule {
        id: Some(id),
        name: format!("configure-{}-{kind}", short_hash(&candidate.id)),
        description: Some(format!("Managed from {}", candidate.evidence)),
        select: selector,
        scope: RuleScope::Owned,
        allow_unscoped: false,
    };
    match kind {
        ResourceKind::Container => rules.containers.push(ContainerRule {
            common,
            stopped_ttl: Some(policy.stopped_container_ttl.clone()),
            running_ttl: None,
            adopt: false,
        }),
        ResourceKind::Image => {
            let image_tag_patterns = match &candidate.selector {
                CandidateSelector::NamePrefix { prefix, .. } => {
                    vec![format!("{}*", escape_glob(prefix))]
                }
                CandidateSelector::ExactLabel { .. } => vec!["*".to_owned()],
                CandidateSelector::BuildCache => Vec::new(),
            };
            rules.images.push(ImageRule {
                common,
                dangling: true,
                unused_for: Some(policy.image_ttl.clone()),
                image_tag_patterns,
            });
        }
        ResourceKind::Volume => rules.volumes.push(VolumeRule {
            common,
            orphan_for: Some(policy.volume_ttl.clone()),
        }),
        ResourceKind::Network => rules.networks.push(NetworkRule {
            common,
            orphan: true,
        }),
        ResourceKind::BuildCache => {}
    }
}

fn sort_rules(rules: &mut Rules) {
    rules
        .containers
        .sort_by(|left, right| left.common.id.cmp(&right.common.id));
    rules
        .images
        .sort_by(|left, right| left.common.id.cmp(&right.common.id));
    rules
        .volumes
        .sort_by(|left, right| left.common.id.cmp(&right.common.id));
    rules
        .networks
        .sort_by(|left, right| left.common.id.cmp(&right.common.id));
}

fn rule_ids(rules: &Rules) -> Vec<String> {
    let mut ids = rules
        .containers
        .iter()
        .map(|rule| &rule.common.id)
        .chain(rules.images.iter().map(|rule| &rule.common.id))
        .chain(rules.volumes.iter().map(|rule| &rule.common.id))
        .chain(rules.networks.iter().map(|rule| &rule.common.id))
        .filter_map(Clone::clone)
        .collect::<Vec<_>>();
    if let Some(id) = rules.build_cache.as_ref().and_then(|rule| rule.id.clone()) {
        ids.push(id);
    }
    ids.sort();
    ids
}

fn reject_overlaps(candidates: &[&CandidateFamily]) -> Result<(), ConfiguratorError> {
    let mut owners = BTreeMap::<(String, String), String>::new();
    for candidate in candidates {
        for resource in &candidate.resources {
            let key = (resource.resource_kind.clone(), resource.id.clone());
            if let Some(existing) = owners.insert(key, candidate.id.clone()) {
                return Err(ConfiguratorError::Invalid(format!(
                    "candidates {existing:?} and {:?} overlap; select one ownership explanation",
                    candidate.id
                )));
            }
        }
    }
    Ok(())
}

fn reject_shadowed_rules(
    candidates: &[&CandidateFamily],
    generated_ids: &[String],
    plan: &crate::plan::Plan,
) -> Result<(), ConfiguratorError> {
    let generated_names = plan
        .decisions
        .iter()
        .filter(|decision| {
            generated_ids.iter().any(|id| {
                id.ends_with(&format!("/{}", decision.resource.kind))
                    && decision
                        .matched_rule
                        .as_deref()
                        .is_some_and(|name| name.starts_with("configure-"))
            })
        })
        .map(|decision| {
            (
                decision.resource.kind.to_string(),
                decision.resource.id.clone(),
            )
        })
        .collect::<BTreeSet<_>>();
    for candidate in candidates {
        let relevant = candidate
            .resources
            .iter()
            .filter(|resource| resource.resource_kind != "build-cache")
            .collect::<Vec<_>>();
        if relevant.iter().any(|resource| {
            !generated_names.contains(&(resource.resource_kind.clone(), resource.id.clone()))
        }) {
            return Err(ConfiguratorError::Invalid(format!(
                "candidate {:?} is partly or fully shadowed by an earlier manual rule",
                candidate.id
            )));
        }
    }
    Ok(())
}

fn render_managed_source(
    manual_source: &str,
    generated_rules: &Rules,
) -> Result<String, ConfiguratorError> {
    let generated = Config {
        rules: generated_rules.clone(),
        ..Config::default()
    }
    .to_normalized_toml()?;
    let mut source = manual_source.trim_end().to_owned();
    if !source.is_empty() {
        source.push_str("\n\n");
    }
    source.push_str(MANAGED_START);
    source.push('\n');
    source.push_str(
        "# Stable IDs below are owned by the configurator. Manual rules above are untouched.\n",
    );
    source.push_str(generated.trim());
    source.push('\n');
    source.push_str(MANAGED_END);
    source.push('\n');
    let _: toml_edit::DocumentMut = source.parse().map_err(|error| {
        ConfiguratorError::Invalid(format!("generated TOML cannot be edited safely: {error}"))
    })?;
    Ok(source)
}

fn strip_managed_region(source: &str) -> Result<String, ConfiguratorError> {
    let start = source.find(MANAGED_START);
    let end = source.find(MANAGED_END);
    match (start, end) {
        (None, None) => Ok(source.to_owned()),
        (Some(start), Some(end)) if start < end => {
            let suffix = end + MANAGED_END.len();
            let mut result = source[..start].trim_end().to_owned();
            let remainder = source[suffix..].trim();
            if !remainder.is_empty() {
                result.push_str("\n\n");
                result.push_str(remainder);
            }
            if !result.is_empty() {
                result.push('\n');
            }
            Ok(result)
        }
        _ => Err(ConfiguratorError::Invalid(
            "configuration contains an incomplete docker_maid managed-rule region".to_owned(),
        )),
    }
}

fn parse_config_source(source: &str, path: &Path) -> Result<Config, ConfiguratorError> {
    if source.trim().is_empty() {
        return Ok(Config::default());
    }
    let config = Config::parse(source, path)?;
    config.validate()?;
    Ok(config)
}

fn reject_managed_ids_outside_region(config: &Config) -> Result<(), ConfiguratorError> {
    let ids = rule_ids(&config.rules);
    if let Some(id) = ids.iter().find(|id| id.starts_with(MANAGED_ID_PREFIX)) {
        return Err(ConfiguratorError::Invalid(format!(
            "managed rule {id:?} is outside the configurator region; move it back or change its id"
        )));
    }
    Ok(())
}

fn durable_write(
    proposal: &ConfigProposal,
    current_source: &str,
) -> Result<ConfigWriteResult, ConfiguratorError> {
    let parent = proposal.target_path.parent().ok_or_else(|| {
        ConfiguratorError::Invalid(format!(
            "configuration path has no parent: {}",
            proposal.target_path.display()
        ))
    })?;
    fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
    set_directory_permissions(parent)?;
    let filename = proposal
        .target_path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| {
            ConfiguratorError::Invalid("configuration filename is not UTF-8".to_owned())
        })?;
    let lock_path = parent.join(format!(".{filename}.lock"));
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|source| io_error(&lock_path, source))?;
    set_file_permissions(&lock_path)?;
    lock.lock_exclusive()
        .map_err(|source| io_error(&lock_path, source))?;

    let locked_source = match fs::read_to_string(&proposal.target_path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(source) => return Err(io_error(&proposal.target_path, source)),
    };
    if locked_source != current_source {
        return Err(ConfiguratorError::Stale(
            "configuration changed while waiting for its write lock".to_owned(),
        ));
    }

    let backup_path = if current_source.is_empty() {
        None
    } else {
        let backup = parent.join(format!("{filename}.bak"));
        fs::copy(&proposal.target_path, &backup).map_err(|source| io_error(&backup, source))?;
        set_file_permissions(&backup)?;
        File::open(&backup)
            .and_then(|file| file.sync_all())
            .map_err(|source| io_error(&backup, source))?;
        Some(backup)
    };

    let nonce = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = parent.join(format!(
        ".{filename}.tmp-{}-{timestamp}-{nonce}",
        std::process::id()
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .map_err(|source| io_error(&temporary, source))?;
        set_file_permissions(&temporary)?;
        file.write_all(proposal.resulting_source.as_bytes())
            .map_err(|source| io_error(&temporary, source))?;
        file.sync_all()
            .map_err(|source| io_error(&temporary, source))?;
        fs::rename(&temporary, &proposal.target_path)
            .map_err(|source| io_error(&proposal.target_path, source))?;
        set_file_permissions(&proposal.target_path)?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|source| io_error(parent, source))?;
        Ok(ConfigWriteResult {
            schema_version: CONFIGURATOR_SCHEMA_VERSION,
            proposal_id: proposal.proposal_id.clone(),
            path: proposal.target_path.clone(),
            backup_path,
            source_hash: proposal.result_source_hash.clone(),
        })
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn pending_ids(plan: &crate::plan::Plan) -> BTreeSet<(ResourceKind, String)> {
    plan.decisions
        .iter()
        .filter(|decision| decision.action == Action::Remove)
        .map(|decision| (decision.resource.kind, decision.resource.id.clone()))
        .collect()
}

fn parse_resource_kind(value: &str) -> Option<ResourceKind> {
    match value {
        "container" => Some(ResourceKind::Container),
        "image" => Some(ResourceKind::Image),
        "volume" => Some(ResourceKind::Volume),
        "network" => Some(ResourceKind::Network),
        "build-cache" => Some(ResourceKind::BuildCache),
        _ => None,
    }
}

fn slug(value: &str) -> String {
    let mut result = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    while result.contains("--") {
        result = result.replace("--", "-");
    }
    let result = result.trim_matches('-');
    if result.is_empty() {
        "candidate".to_owned()
    } else {
        result.chars().take(40).collect()
    }
}

fn escape_glob(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(character, '*' | '?' | '[' | ']' | '{' | '}' | '\\') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

fn short_hash(value: &str) -> String {
    stable_config_hash(value).chars().take(8).collect()
}

fn io_error(path: impl AsRef<Path>, source: std::io::Error) -> ConfiguratorError {
    ConfiguratorError::Io {
        path: path.as_ref().to_path_buf(),
        source,
    }
}

#[cfg(unix)]
fn set_directory_permissions(path: &Path) -> Result<(), ConfiguratorError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|source| io_error(path, source))
}

#[cfg(not(unix))]
fn set_directory_permissions(_path: &Path) -> Result<(), ConfiguratorError> {
    Ok(())
}

#[cfg(unix)]
fn set_file_permissions(path: &Path) -> Result<(), ConfiguratorError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|source| io_error(path, source))
}

#[cfg(not(unix))]
fn set_file_permissions(_path: &Path) -> Result<(), ConfiguratorError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(kind: ResourceKind, id: &str, name: &str, labels: &[(&str, &str)]) -> InventoryItem {
        InventoryItem {
            kind,
            id: id.to_owned(),
            name: name.to_owned(),
            search_names: vec![name.to_owned()],
            parent_ids: Vec::new(),
            labels: labels
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect(),
            mounts: Vec::new(),
            state: if kind == ResourceKind::Container {
                ResourceState::Stopped
            } else {
                ResourceState::Available
            },
            created_at: Some(1),
            state_since: Some(1),
            size: Some(10),
            referenced: false,
            dangling: false,
            system: false,
        }
    }

    #[test]
    fn survey_uses_exact_compose_evidence_and_keeps_unlabeled_unowned() {
        let inventory = vec![
            item(
                ResourceKind::Container,
                "c1",
                "project-web",
                &[("com.docker.compose.project", "project")],
            ),
            item(ResourceKind::Volume, "v1", "unowned", &[]),
        ];
        let mut survey = survey_inventory(&inventory);
        assert_eq!(survey.candidates.len(), 1);
        assert_eq!(survey.summary.candidate_resources, 1);
        assert_eq!(survey.summary.unowned_resources, 1);
        assert!(matches!(
            survey.candidates[0].selector,
            CandidateSelector::ExactLabel { .. }
        ));
        refresh_candidate_warnings(
            &mut survey,
            &PolicyProfile::Workstation.settings(),
            &inventory,
            10_000,
        );
        let warning = survey.candidates[0]
            .warning
            .as_deref()
            .expect("Compose warning");
        assert!(warning.contains("docker compose down"));
        assert!(warning.contains("stopped containers become eligible after 2h"));
    }

    #[test]
    fn display_order_keeps_the_canonical_candidate_vector_unchanged() {
        let inventory = vec![
            item(
                ResourceKind::Container,
                "compose",
                "project-web",
                &[("com.docker.compose.project", "project")],
            ),
            item(
                ResourceKind::Container,
                "agent",
                "agent-web",
                &[("devcontainer.local_folder", "/workspace")],
            ),
            item(ResourceKind::BuildCache, "cache", "cache", &[]),
        ];
        let survey = survey_inventory(&inventory);
        let canonical = survey
            .candidates
            .iter()
            .map(|candidate| candidate.id.clone())
            .collect::<Vec<_>>();
        assert!(canonical[0].starts_with("agent-label/"));
        assert_eq!(canonical[1], "build-cache");
        assert!(canonical[2].starts_with("compose/"));

        let display = candidate_display_indices(&survey.candidates)
            .into_iter()
            .map(|index| survey.candidates[index].id.clone())
            .collect::<Vec<_>>();
        assert!(display[0].starts_with("agent-label/"));
        assert!(display[1].starts_with("compose/"));
        assert_eq!(display[2], "build-cache");
        assert_eq!(
            canonical,
            survey
                .candidates
                .iter()
                .map(|candidate| candidate.id.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn compose_warning_tracks_generated_policy_and_zero_pending_running_preview() {
        let mut container = item(
            ResourceKind::Container,
            "container",
            "project-web",
            &[("com.docker.compose.project", "project")],
        );
        container.state = ResourceState::Running;
        let mut volume = item(
            ResourceKind::Volume,
            "volume",
            "project-data",
            &[("com.docker.compose.project", "project")],
        );
        volume.referenced = true;
        let mut network = item(
            ResourceKind::Network,
            "network",
            "project_default",
            &[("com.docker.compose.project", "project")],
        );
        network.referenced = true;
        let inventory = vec![container, volume, network];
        let mut survey = survey_inventory(&inventory);
        let mut policy = PolicyProfile::Workstation.settings();
        policy.stopped_container_ttl = "3h".to_owned();
        policy.volume_ttl = "5d".to_owned();
        refresh_candidate_warnings(&mut survey, &policy, &inventory, 10_000);

        let candidate = &survey.candidates[0];
        let warning = candidate.warning.as_deref().expect("Compose warning");
        assert!(warning.contains("preview zero removals now"));
        assert!(warning.contains("stopped containers become eligible after 3h"));
        assert!(warning.contains(
            "detached volumes become eligible immediately when their resource age already exceeds 5d"
        ));
        assert!(warning.contains("empty networks become eligible immediately"));

        let proposal = propose_configuration(&ProposalRequest {
            base_source: "",
            source_existed: false,
            target_path: Path::new("config.toml"),
            survey: &survey,
            inventory: &inventory,
            profile: PolicyProfile::Workstation,
            policy: Some(&policy),
            candidate_ids: std::slice::from_ref(&candidate.id),
            now_epoch_seconds: 10_000,
        })
        .expect("proposal");
        assert_eq!(proposal.preview.after_pending, 0);
        assert_eq!(proposal.warnings, vec![warning.to_owned()]);
    }

    #[test]
    fn compose_warning_states_pending_count_instead_of_zero_claim_when_past_ttl() {
        let container = item(
            ResourceKind::Container,
            "c1",
            "project-web",
            &[("com.docker.compose.project", "project")],
        );
        let volume = item(
            ResourceKind::Volume,
            "v1",
            "project-data",
            &[("com.docker.compose.project", "project")],
        );
        let inventory = vec![container, volume];
        let mut survey = survey_inventory(&inventory);
        let policy = PolicyProfile::Workstation.settings();
        refresh_candidate_warnings(&mut survey, &policy, &inventory, 1_000_000);

        let candidate = &survey.candidates[0];
        let warning = candidate.warning.as_deref().expect("Compose warning");
        assert!(!warning.contains("zero removals now"), "{warning}");
        assert!(
            warning.contains("currently previews 2 pending removal(s)"),
            "{warning}"
        );

        let proposal = propose_configuration(&ProposalRequest {
            base_source: "",
            source_existed: false,
            target_path: Path::new("config.toml"),
            survey: &survey,
            inventory: &inventory,
            profile: PolicyProfile::Workstation,
            policy: Some(&policy),
            candidate_ids: std::slice::from_ref(&candidate.id),
            now_epoch_seconds: 1_000_000,
        })
        .expect("proposal");
        assert_eq!(proposal.preview.after_pending, 2);
        assert_eq!(proposal.warnings, vec![warning.to_owned()]);
    }

    #[test]
    fn survey_and_proposal_warnings_are_byte_identical_for_a_profile() {
        let inventory = vec![item(
            ResourceKind::Network,
            "n1",
            "project_default",
            &[("com.docker.compose.project", "project")],
        )];
        let mut survey = survey_inventory(&inventory);
        refresh_candidate_warnings(
            &mut survey,
            &PolicyProfile::SharedHost.settings(),
            &inventory,
            10_000,
        );
        let survey_warning = survey.candidates[0]
            .warning
            .clone()
            .expect("Compose warning");

        let proposal = propose_configuration(&ProposalRequest {
            base_source: "",
            source_existed: false,
            target_path: Path::new("config.toml"),
            survey: &survey,
            inventory: &inventory,
            profile: PolicyProfile::SharedHost,
            policy: None,
            candidate_ids: &[survey.candidates[0].id.clone()],
            now_epoch_seconds: 10_000,
        })
        .expect("proposal");
        assert_eq!(proposal.warnings, vec![survey_warning]);
    }

    #[test]
    fn proposal_is_deterministic_and_preserves_manual_source() {
        let source = "# keep this comment\n\n[[rules.networks]]\nname = \"manual\"\norphan = true\nselect.names = [\"^manual$\"]\n";
        let inventory = vec![item(
            ResourceKind::Volume,
            "v1",
            "project-data",
            &[("com.docker.compose.project", "project")],
        )];
        let survey = survey_inventory(&inventory);
        let ids = vec![survey.candidates[0].id.clone()];
        let first = propose_configuration(&ProposalRequest {
            base_source: source,
            source_existed: true,
            target_path: Path::new("config.toml"),
            survey: &survey,
            inventory: &inventory,
            profile: PolicyProfile::Workstation,
            policy: None,
            candidate_ids: &ids,
            now_epoch_seconds: 10_000,
        })
        .expect("proposal");
        let second = propose_configuration(&ProposalRequest {
            base_source: source,
            source_existed: true,
            target_path: Path::new("config.toml"),
            survey: &survey,
            inventory: &inventory,
            profile: PolicyProfile::Workstation,
            policy: None,
            candidate_ids: &ids,
            now_epoch_seconds: 10_000,
        })
        .expect("proposal");
        assert_eq!(first, second);
        assert!(first.resulting_source.starts_with("# keep this comment"));
        assert!(first.resulting_source.contains(MANAGED_START));
        assert!(first.resulting_source.contains("orphan_for = \"48h\""));
    }

    #[test]
    fn overlapping_candidates_are_refused() {
        let inventory = vec![item(
            ResourceKind::Container,
            "c1",
            "project-web",
            &[
                ("com.docker.compose.project", "project"),
                ("devcontainer.local_folder", "/workspace"),
            ],
        )];
        let survey = survey_inventory(&inventory);
        let ids = survey
            .candidates
            .iter()
            .map(|candidate| candidate.id.clone())
            .collect::<Vec<_>>();
        let error = propose_configuration(&ProposalRequest {
            base_source: "",
            source_existed: false,
            target_path: Path::new("config.toml"),
            survey: &survey,
            inventory: &inventory,
            profile: PolicyProfile::SharedHost,
            policy: None,
            candidate_ids: &ids,
            now_epoch_seconds: 10_000,
        })
        .expect_err("overlap");
        assert!(error.to_string().contains("overlap"));
    }

    #[test]
    fn exact_label_evidence_escapes_glob_metacharacters() {
        let inventory = vec![item(
            ResourceKind::Container,
            "c1",
            "agent",
            &[("devcontainer.local_folder", "/work/[agent]*")],
        )];
        let survey = survey_inventory(&inventory);
        let candidate_ids = [survey.candidates[0].id.clone()];
        let proposal = propose_configuration(&ProposalRequest {
            base_source: "",
            source_existed: false,
            target_path: Path::new("config.toml"),
            survey: &survey,
            inventory: &inventory,
            profile: PolicyProfile::Workstation,
            policy: None,
            candidate_ids: &candidate_ids,
            now_epoch_seconds: 10_000,
        })
        .expect("proposal");
        assert!(
            proposal
                .resulting_source
                .contains(r"devcontainer.local_folder=/work/\[agent\]\*"),
            "{}",
            proposal.resulting_source
        );
        assert_eq!(proposal.preview.after_pending, 1);
    }

    #[test]
    fn writer_rejects_source_and_inventory_drift() {
        let root = std::env::temp_dir().join(format!(
            "docker-maid-configurator-{}-{}",
            std::process::id(),
            NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).expect("root");
        let target = root.join("config.toml");
        let inventory = vec![item(
            ResourceKind::Network,
            "n1",
            "project_default",
            &[("com.docker.compose.project", "project")],
        )];
        let survey = survey_inventory(&inventory);
        let candidate_ids = [survey.candidates[0].id.clone()];
        let proposal = propose_configuration(&ProposalRequest {
            base_source: "",
            source_existed: false,
            target_path: &target,
            survey: &survey,
            inventory: &inventory,
            profile: PolicyProfile::SharedHost,
            policy: None,
            candidate_ids: &candidate_ids,
            now_epoch_seconds: 10_000,
        })
        .expect("proposal");
        let mut changed_inventory = inventory.clone();
        changed_inventory[0].name.push_str("-changed");
        assert!(write_proposal(&proposal, &changed_inventory)
            .expect_err("inventory drift")
            .to_string()
            .contains("inventory changed"));
        fs::write(&target, "# changed\n").expect("change source");
        assert!(write_proposal(&proposal, &inventory)
            .expect_err("source drift")
            .to_string()
            .contains("configuration changed"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn writer_creates_private_file_then_replaces_only_managed_region() {
        let root = std::env::temp_dir().join(format!(
            "docker-maid-configurator-write-{}-{}",
            std::process::id(),
            NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).expect("root");
        let target = root.join("config.toml");
        let inventory = vec![item(
            ResourceKind::Volume,
            "v1",
            "project-data",
            &[("com.docker.compose.project", "project")],
        )];
        let survey = survey_inventory(&inventory);
        let candidate_ids = [survey.candidates[0].id.clone()];
        let first = propose_configuration(&ProposalRequest {
            base_source: "",
            source_existed: false,
            target_path: &target,
            survey: &survey,
            inventory: &inventory,
            profile: PolicyProfile::SharedHost,
            policy: None,
            candidate_ids: &candidate_ids,
            now_epoch_seconds: 10_000,
        })
        .expect("first proposal");
        let first_write = write_proposal(&first, &inventory).expect("first write");
        assert!(first_write.backup_path.is_none());
        let first_source = fs::read_to_string(&target).expect("first source");
        let manual_source = format!("# manual comment\n\n{first_source}");
        fs::write(&target, &manual_source).expect("add manual comment");

        let second = propose_configuration(&ProposalRequest {
            base_source: &manual_source,
            source_existed: true,
            target_path: &target,
            survey: &survey,
            inventory: &inventory,
            profile: PolicyProfile::EphemeralCi,
            policy: None,
            candidate_ids: &candidate_ids,
            now_epoch_seconds: 10_000,
        })
        .expect("second proposal");
        let second_write = write_proposal(&second, &inventory).expect("second write");
        assert!(second_write.backup_path.is_some());
        let second_source = fs::read_to_string(&target).expect("second source");
        assert_eq!(second_source.matches(MANAGED_START).count(), 1);
        assert_eq!(second_source.matches("# manual comment").count(), 1);
        assert!(second_source.contains("orphan_for = \"6h\""));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&target)
                    .expect("metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(&root).expect("metadata").permissions().mode() & 0o777,
                0o700
            );
        }
        fs::remove_dir_all(root).expect("cleanup");
    }
}
