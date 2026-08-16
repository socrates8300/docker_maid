//! Pure policy evaluation and deterministic dry-run plan rendering.

use crate::config::{BuildCacheRule, CommonRule, Config, RuleScope, Selectors};
use crate::observation::ObservationState;
use crate::state::ProtectionState;
use globset::{Glob, GlobMatcher};
use regex::Regex;
use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ResourceKind {
    Container,
    Image,
    Volume,
    Network,
    BuildCache,
}

impl fmt::Display for ResourceKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Container => "container",
            Self::Image => "image",
            Self::Volume => "volume",
            Self::Network => "network",
            Self::BuildCache => "build-cache",
        };
        formatter.write_str(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceState {
    Running,
    Stopped,
    Available,
    Other(String),
}

impl fmt::Display for ResourceState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Running => formatter.write_str("running"),
            Self::Stopped => formatter.write_str("stopped"),
            Self::Available => formatter.write_str("available"),
            Self::Other(value) => formatter.write_str(value),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InventoryItem {
    pub kind: ResourceKind,
    pub id: String,
    pub name: String,
    pub search_names: Vec<String>,
    pub parent_ids: Vec<String>,
    pub labels: BTreeMap<String, String>,
    pub mounts: Vec<String>,
    pub state: ResourceState,
    pub created_at: Option<i64>,
    pub state_since: Option<i64>,
    pub size: Option<u64>,
    pub referenced: bool,
    pub dangling: bool,
    pub system: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    Protected,
    Owned,
    AuthorizedUnscoped,
    Unowned,
}

impl fmt::Display for Disposition {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Protected => "protected",
            Self::Owned => "owned",
            Self::AuthorizedUnscoped => "authorized-unscoped",
            Self::Unowned => "unowned",
        };
        formatter.write_str(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Keep,
    Remove,
}

impl fmt::Display for Action {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Keep => formatter.write_str("keep"),
            Self::Remove => formatter.write_str("remove"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub resource: InventoryItem,
    pub disposition: Disposition,
    pub matched_rule: Option<String>,
    pub action: Action,
    pub age_seconds: Option<u64>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub decisions: Vec<Decision>,
}

impl Plan {
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.decisions
            .iter()
            .filter(|decision| decision.action == Action::Remove)
            .count()
    }

    #[must_use]
    pub fn has_pending_removals(&self) -> bool {
        self.pending_count() != 0
    }

    #[must_use]
    pub fn render_table(&self) -> String {
        let removals = self
            .decisions
            .iter()
            .filter(|decision| decision.action == Action::Remove)
            .collect::<Vec<_>>();
        let counts = ResourceCounts::from_decisions(&self.decisions);

        if removals.is_empty() {
            return format!("No removals pending.\n{}\n", counts.render("Scanned"));
        }

        let authorized_unscoped = removals
            .iter()
            .filter(|decision| decision.disposition == Disposition::AuthorizedUnscoped)
            .count();

        let mut rows = vec![vec![
            "TYPE".to_owned(),
            "NAME".to_owned(),
            "STATE".to_owned(),
            "AGE".to_owned(),
            "DISPOSITION".to_owned(),
            "RULE".to_owned(),
            "ACTION".to_owned(),
            "REASON".to_owned(),
        ]];

        for decision in &removals {
            rows.push(vec![
                decision.resource.kind.to_string(),
                decision.resource.name.clone(),
                decision.resource.state.to_string(),
                decision
                    .age_seconds
                    .map_or_else(|| "unknown".to_owned(), format_duration),
                decision.disposition.to_string(),
                decision
                    .matched_rule
                    .clone()
                    .unwrap_or_else(|| "-".to_owned()),
                decision.action.to_string(),
                decision.reason.clone(),
            ]);
        }

        let widths = (0..rows[0].len())
            .map(|column| rows.iter().map(|row| row[column].len()).max().unwrap_or(0))
            .collect::<Vec<_>>();
        let mut output = String::new();
        if authorized_unscoped != 0 {
            output.push_str("WARNING: ");
            output.push_str(&authorized_unscoped.to_string());
            output.push_str(" pending removal(s) use authorized-unscoped policy.\n\n");
        }
        for row in rows {
            for (column, value) in row.iter().enumerate() {
                if column + 1 == row.len() {
                    output.push_str(value);
                } else {
                    output.push_str(value);
                    output.extend(std::iter::repeat_n(
                        ' ',
                        widths[column].saturating_sub(value.len()) + 2,
                    ));
                }
            }
            output.push('\n');
        }
        output.push('\n');
        output.push_str("Pending removals: ");
        output.push_str(&removals.len().to_string());
        output.push('\n');
        output.push_str(&counts.render("Scanned"));
        output.push('\n');
        output
    }
}

#[derive(Debug)]
pub struct PlanError {
    field: String,
    message: String,
}

impl fmt::Display for PlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.field, self.message)
    }
}

impl std::error::Error for PlanError {}

/// Runtime state that policy evaluation consults besides the configuration.
///
/// Both members default to empty, and both defaults are conservative: no
/// runtime protection, and no observed-unreferenced history, which keeps every
/// resource whose policy depends on that clock.
#[derive(Debug, Clone, Copy)]
pub struct PlanContext<'a> {
    pub protection: &'a ProtectionState,
    pub observations: &'a ObservationState,
}

static NO_PROTECTION: ProtectionState = ProtectionState::empty();
static NO_OBSERVATIONS: ObservationState = ObservationState::empty();

impl Default for PlanContext<'_> {
    fn default() -> Self {
        Self {
            protection: &NO_PROTECTION,
            observations: &NO_OBSERVATIONS,
        }
    }
}

/// Evaluate a validated configuration against an immutable inventory snapshot.
///
/// # Errors
///
/// Returns an error if a selector or duration cannot be compiled. Normal CLI
/// use prevents this by validating the configuration before inventory starts.
pub fn build_plan(
    config: &Config,
    inventory: Vec<InventoryItem>,
    now_epoch_seconds: i64,
) -> Result<Plan, PlanError> {
    build_plan_with_context(
        config,
        inventory,
        now_epoch_seconds,
        &PlanContext::default(),
    )
}

/// Evaluate configuration and machine-managed protection against an inventory.
///
/// # Errors
///
/// Returns an error if a selector or duration cannot be compiled.
pub fn build_plan_with_protection(
    config: &Config,
    inventory: Vec<InventoryItem>,
    now_epoch_seconds: i64,
    runtime_protection: &ProtectionState,
) -> Result<Plan, PlanError> {
    build_plan_with_context(
        config,
        inventory,
        now_epoch_seconds,
        &PlanContext {
            protection: runtime_protection,
            ..PlanContext::default()
        },
    )
}

/// Evaluate configuration against an inventory plus all durable runtime state.
///
/// # Errors
///
/// Returns an error if a selector or duration cannot be compiled.
pub fn build_plan_with_context(
    config: &Config,
    inventory: Vec<InventoryItem>,
    now_epoch_seconds: i64,
    context: &PlanContext<'_>,
) -> Result<Plan, PlanError> {
    let policy = CompiledPolicy::compile(config)?;
    let build_cache_removals = policy.build_cache_removals(&inventory, now_epoch_seconds);
    let mut decisions = inventory
        .into_iter()
        .map(|item| policy.decide(item, now_epoch_seconds, &build_cache_removals, context))
        .collect::<Vec<_>>();
    decisions.sort_by(|left, right| {
        (left.resource.kind, &left.resource.name, &left.resource.id).cmp(&(
            right.resource.kind,
            &right.resource.name,
            &right.resource.id,
        ))
    });
    Ok(Plan { decisions })
}

struct CompiledPolicy {
    protection: SelectorMatcher,
    containers: Vec<CompiledContainerRule>,
    images: Vec<CompiledImageRule>,
    volumes: Vec<CompiledVolumeRule>,
    networks: Vec<CompiledNetworkRule>,
    build_cache: Option<CompiledBuildCacheRule>,
}

impl CompiledPolicy {
    fn compile(config: &Config) -> Result<Self, PlanError> {
        let protection = SelectorMatcher::compile(
            "protect",
            &Selectors {
                labels: config.protect.labels.clone(),
                names: config.protect.names.clone(),
                name_parts: Vec::new(),
            },
        )?;

        let containers = config
            .rules
            .containers
            .iter()
            .enumerate()
            .map(|(index, rule)| {
                let field = format!("rules.containers[{index}]");
                Ok(CompiledContainerRule {
                    common: CompiledCommonRule::compile(&field, &rule.common)?,
                    stopped_ttl: parse_optional_duration(
                        &format!("{field}.stopped_ttl"),
                        rule.stopped_ttl.as_deref(),
                    )?,
                    running_ttl: parse_optional_duration(
                        &format!("{field}.running_ttl"),
                        rule.running_ttl.as_deref(),
                    )?,
                })
            })
            .collect::<Result<Vec<_>, PlanError>>()?;

        let images = config
            .rules
            .images
            .iter()
            .enumerate()
            .map(|(index, rule)| {
                let field = format!("rules.images[{index}]");
                Ok(CompiledImageRule {
                    common: CompiledCommonRule::compile(&field, &rule.common)?,
                    dangling: rule.dangling,
                    unused_for: parse_optional_duration(
                        &format!("{field}.unused_for"),
                        rule.unused_for.as_deref(),
                    )?,
                    tag_patterns: compile_globs(
                        &format!("{field}.image_tag_patterns"),
                        &rule.image_tag_patterns,
                    )?,
                })
            })
            .collect::<Result<Vec<_>, PlanError>>()?;

        let volumes = config
            .rules
            .volumes
            .iter()
            .enumerate()
            .map(|(index, rule)| {
                let field = format!("rules.volumes[{index}]");
                Ok(CompiledVolumeRule {
                    common: CompiledCommonRule::compile(&field, &rule.common)?,
                    orphan_for: parse_optional_duration(
                        &format!("{field}.orphan_for"),
                        rule.orphan_for.as_deref(),
                    )?,
                })
            })
            .collect::<Result<Vec<_>, PlanError>>()?;

        let networks = config
            .rules
            .networks
            .iter()
            .enumerate()
            .map(|(index, rule)| {
                let field = format!("rules.networks[{index}]");
                Ok(CompiledNetworkRule {
                    common: CompiledCommonRule::compile(&field, &rule.common)?,
                    orphan: rule.orphan,
                    orphan_for: parse_optional_duration(
                        &format!("{field}.orphan_for"),
                        rule.orphan_for.as_deref(),
                    )?,
                })
            })
            .collect::<Result<Vec<_>, PlanError>>()?;

        let build_cache = config
            .rules
            .build_cache
            .as_ref()
            .map(CompiledBuildCacheRule::compile)
            .transpose()?;

        Ok(Self {
            protection,
            containers,
            images,
            volumes,
            networks,
            build_cache,
        })
    }

    fn decide(
        &self,
        item: InventoryItem,
        now: i64,
        build_cache_removals: &BTreeMap<String, String>,
        context: &PlanContext<'_>,
    ) -> Decision {
        if let Some(reason) = context.protection.match_reason(&item) {
            return keep(item, Disposition::Protected, None, reason);
        }
        if let Some(reason) = self.protection.match_reason(&item) {
            return keep(item, Disposition::Protected, None, reason);
        }

        let unreferenced_age = context.observations.unreferenced_age(&item, now);
        match item.kind {
            ResourceKind::Container => self.decide_container(item, now),
            ResourceKind::Image => self.decide_image(item, unreferenced_age),
            ResourceKind::Volume => self.decide_volume(item, unreferenced_age),
            ResourceKind::Network => self.decide_network(item, unreferenced_age),
            ResourceKind::BuildCache => self.decide_build_cache(item, now, build_cache_removals),
        }
    }

    fn decide_container(&self, item: InventoryItem, now: i64) -> Decision {
        for rule in &self.containers {
            let Some(selector) = rule.common.selector.match_reason(&item) else {
                continue;
            };
            let disposition = rule.common.disposition();
            let threshold = match item.state {
                ResourceState::Running => rule.running_ttl,
                ResourceState::Stopped => rule.stopped_ttl,
                ResourceState::Available | ResourceState::Other(_) => None,
            };
            let Some(threshold) = threshold else {
                return keep(
                    item,
                    disposition,
                    Some(rule.common.name.clone()),
                    format!("matched {selector}; no policy applies to its current state"),
                );
            };
            let age = age_seconds(now, item.state_since);
            if age.is_some_and(|seconds| seconds >= threshold.as_secs()) {
                let reason = format!(
                    "matched {selector}; state age {} meets {}",
                    format_duration(age.unwrap_or_default()),
                    format_duration(threshold.as_secs())
                );
                return remove(item, disposition, &rule.common.name, age, reason);
            }
            return keep(
                item,
                disposition,
                Some(rule.common.name.clone()),
                format!(
                    "matched {selector}; state age is below {} or unavailable",
                    format_duration(threshold.as_secs())
                ),
            );
        }
        unowned(item)
    }

    fn decide_image(&self, item: InventoryItem, unreferenced_age: Option<u64>) -> Decision {
        for rule in &self.images {
            let Some(selector) = rule.common.selector.match_reason(&item) else {
                continue;
            };
            let disposition = rule.common.disposition();
            if item.referenced {
                return keep(
                    item,
                    disposition,
                    Some(rule.common.name.clone()),
                    format!("matched {selector}; image is referenced by a container"),
                );
            }
            let tag_match = rule.tag_patterns.iter().any(|pattern| {
                item.search_names
                    .iter()
                    .any(|name| pattern.matcher.is_match(name))
            });
            if !(tag_match || rule.dangling && item.dangling) {
                return keep(
                    item,
                    disposition,
                    Some(rule.common.name.clone()),
                    format!("matched {selector}; no image removal predicate matched"),
                );
            }
            let Some(threshold) = rule.unused_for else {
                return keep(
                    item,
                    disposition,
                    Some(rule.common.name.clone()),
                    format!("matched {selector}; no unused age policy is set"),
                );
            };
            if unreferenced_age.is_none_or(|value| value < threshold.as_secs()) {
                return keep(
                    item,
                    disposition,
                    Some(rule.common.name.clone()),
                    observed_wait_reason(&selector, "unreferenced", unreferenced_age, threshold),
                );
            }
            let predicate = if rule.dangling && item.dangling {
                "dangling image"
            } else {
                "image tag pattern"
            };
            let reason = format!(
                "matched {selector}; unreferenced {predicate}; observed unreferenced for {} which meets {}",
                unreferenced_age.map_or_else(|| "unknown".to_owned(), format_duration),
                format_duration(threshold.as_secs())
            );
            return remove(
                item,
                disposition,
                &rule.common.name,
                unreferenced_age,
                reason,
            );
        }
        unowned(item)
    }

    fn decide_volume(&self, item: InventoryItem, unreferenced_age: Option<u64>) -> Decision {
        for rule in &self.volumes {
            let Some(selector) = rule.common.selector.match_reason(&item) else {
                continue;
            };
            let disposition = rule.common.disposition();
            if item.referenced {
                return keep(
                    item,
                    disposition,
                    Some(rule.common.name.clone()),
                    format!("matched {selector}; volume is attached to a container"),
                );
            }
            let Some(threshold) = rule.orphan_for else {
                return keep(
                    item,
                    disposition,
                    Some(rule.common.name.clone()),
                    format!("matched {selector}; no orphan age policy is set"),
                );
            };
            if unreferenced_age.is_some_and(|seconds| seconds >= threshold.as_secs()) {
                return remove(
                    item,
                    disposition,
                    &rule.common.name,
                    unreferenced_age,
                    format!(
                        "matched {selector}; observed unattached for {} which meets {}",
                        unreferenced_age.map_or_else(|| "unknown".to_owned(), format_duration),
                        format_duration(threshold.as_secs())
                    ),
                );
            }
            return keep(
                item,
                disposition,
                Some(rule.common.name.clone()),
                observed_wait_reason(&selector, "unattached", unreferenced_age, threshold),
            );
        }
        unowned(item)
    }

    fn decide_network(&self, item: InventoryItem, unreferenced_age: Option<u64>) -> Decision {
        if item.system {
            return keep(
                item,
                Disposition::Protected,
                None,
                "Docker system network".to_owned(),
            );
        }
        for rule in &self.networks {
            let Some(selector) = rule.common.selector.match_reason(&item) else {
                continue;
            };
            let disposition = rule.common.disposition();
            if !rule.orphan || item.referenced {
                return keep(
                    item,
                    disposition,
                    Some(rule.common.name.clone()),
                    format!("matched {selector}; network is in use or orphan cleanup is disabled"),
                );
            }
            let Some(threshold) = rule.orphan_for else {
                return keep(
                    item,
                    disposition,
                    Some(rule.common.name.clone()),
                    format!("matched {selector}; no orphan age policy is set"),
                );
            };
            if unreferenced_age.is_some_and(|seconds| seconds >= threshold.as_secs()) {
                return remove(
                    item,
                    disposition,
                    &rule.common.name,
                    unreferenced_age,
                    format!(
                        "matched {selector}; observed empty for {} which meets {}",
                        unreferenced_age.map_or_else(|| "unknown".to_owned(), format_duration),
                        format_duration(threshold.as_secs())
                    ),
                );
            }
            return keep(
                item,
                disposition,
                Some(rule.common.name.clone()),
                observed_wait_reason(&selector, "empty", unreferenced_age, threshold),
            );
        }
        unowned(item)
    }

    fn build_cache_removals(
        &self,
        inventory: &[InventoryItem],
        now: i64,
    ) -> BTreeMap<String, String> {
        let Some(rule) = &self.build_cache else {
            return BTreeMap::new();
        };
        let cache = inventory
            .iter()
            .filter(|item| item.kind == ResourceKind::BuildCache)
            .collect::<Vec<_>>();
        let total_bytes = cache
            .iter()
            .filter_map(|item| item.size)
            .fold(0_u64, u64::saturating_add);
        let mut removals = BTreeMap::new();

        if let Some(threshold) = rule.older_than {
            for item in &cache {
                if item.referenced {
                    continue;
                }
                let age = age_seconds(now, item.state_since.or(item.created_at));
                if age.is_some_and(|seconds| seconds >= threshold.as_secs()) {
                    removals.insert(
                        item.id.clone(),
                        format!(
                            "authorized-unscoped build cache; last-use age {} meets {}",
                            format_duration(age.unwrap_or_default()),
                            format_duration(threshold.as_secs())
                        ),
                    );
                }
            }
        }

        let selected_bytes = cache
            .iter()
            .filter(|item| removals.contains_key(&item.id))
            .filter_map(|item| item.size)
            .fold(0_u64, u64::saturating_add);
        let mut retained_bytes = total_bytes.saturating_sub(selected_bytes);
        if let Some(max_bytes) = rule.max_bytes {
            let mut candidates = cache
                .iter()
                .filter(|item| !item.referenced && !removals.contains_key(&item.id))
                .filter(|item| item.size.is_some())
                .filter_map(|item| {
                    item.state_since
                        .or(item.created_at)
                        .map(|last_used| (*item, last_used))
                })
                .collect::<Vec<_>>();
            candidates.sort_by(|(left, left_used), (right, right_used)| {
                (left_used, &left.id).cmp(&(right_used, &right.id))
            });
            for (item, _) in candidates {
                if retained_bytes <= max_bytes {
                    break;
                }
                removals.insert(
                    item.id.clone(),
                    format!(
                        "authorized-unscoped build cache; oldest-first removal reduces cache from {total_bytes} bytes toward {max_bytes} bytes"
                    ),
                );
                retained_bytes = retained_bytes.saturating_sub(item.size.unwrap_or(0));
            }
        }
        removals
    }

    fn decide_build_cache(
        &self,
        item: InventoryItem,
        now: i64,
        removals: &BTreeMap<String, String>,
    ) -> Decision {
        if self.build_cache.is_none() {
            return unowned(item);
        }
        let age = age_seconds(now, item.state_since.or(item.created_at));
        if item.referenced {
            return keep(
                item,
                Disposition::AuthorizedUnscoped,
                Some("build-cache".to_owned()),
                "authorized-unscoped build cache is in use or shared with an image".to_owned(),
            );
        }
        if let Some(reason) = removals.get(&item.id) {
            return remove(
                item,
                Disposition::AuthorizedUnscoped,
                "build-cache",
                age,
                reason.clone(),
            );
        }
        let reason = if age.is_none() {
            "authorized-unscoped build cache has no usable age; kept conservatively"
        } else {
            "authorized-unscoped build cache does not meet the configured age or budget policy"
        };
        keep(
            item,
            Disposition::AuthorizedUnscoped,
            Some("build-cache".to_owned()),
            reason.to_owned(),
        )
    }
}

struct CompiledCommonRule {
    name: String,
    scope: RuleScope,
    selector: SelectorMatcher,
}

impl CompiledCommonRule {
    fn compile(field: &str, rule: &CommonRule) -> Result<Self, PlanError> {
        Ok(Self {
            name: rule.name.clone(),
            scope: rule.scope.clone(),
            selector: SelectorMatcher::compile(&format!("{field}.select"), &rule.select)?,
        })
    }

    fn disposition(&self) -> Disposition {
        match self.scope {
            RuleScope::Owned => Disposition::Owned,
            RuleScope::All => Disposition::AuthorizedUnscoped,
        }
    }
}

struct CompiledContainerRule {
    common: CompiledCommonRule,
    stopped_ttl: Option<Duration>,
    running_ttl: Option<Duration>,
}

struct CompiledImageRule {
    common: CompiledCommonRule,
    dangling: bool,
    unused_for: Option<Duration>,
    tag_patterns: Vec<CompiledGlob>,
}

struct CompiledVolumeRule {
    common: CompiledCommonRule,
    orphan_for: Option<Duration>,
}

struct CompiledNetworkRule {
    common: CompiledCommonRule,
    orphan: bool,
    orphan_for: Option<Duration>,
}

struct CompiledBuildCacheRule {
    older_than: Option<Duration>,
    max_bytes: Option<u64>,
}

impl CompiledBuildCacheRule {
    fn compile(rule: &BuildCacheRule) -> Result<Self, PlanError> {
        Ok(Self {
            older_than: parse_optional_duration(
                "rules.build_cache.older_than",
                rule.older_than.as_deref(),
            )?,
            max_bytes: rule.max_bytes,
        })
    }
}

struct SelectorMatcher {
    labels: Vec<CompiledGlob>,
    names: Vec<CompiledRegex>,
    name_parts: Vec<String>,
}

impl SelectorMatcher {
    fn compile(field: &str, selectors: &Selectors) -> Result<Self, PlanError> {
        let labels = compile_globs(&format!("{field}.labels"), &selectors.labels)?;
        let names = selectors
            .names
            .iter()
            .enumerate()
            .map(|(index, value)| {
                Regex::new(value)
                    .map(|matcher| CompiledRegex {
                        source: value.clone(),
                        matcher,
                    })
                    .map_err(|error| PlanError {
                        field: format!("{field}.names[{index}]"),
                        message: error.to_string(),
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            labels,
            names,
            name_parts: selectors.name_parts.clone(),
        })
    }

    fn match_reason(&self, item: &InventoryItem) -> Option<String> {
        for pattern in &self.labels {
            for (key, value) in &item.labels {
                let pair = format!("{key}={value}");
                if pattern.matcher.is_match(key) || pattern.matcher.is_match(&pair) {
                    return Some(format!("label {} ({pair})", pattern.source));
                }
            }
        }
        for pattern in &self.names {
            if item
                .search_names
                .iter()
                .any(|name| pattern.matcher.is_match(name))
            {
                return Some(format!("name regex {}", pattern.source));
            }
        }
        for part in &self.name_parts {
            if item.search_names.iter().any(|name| name.contains(part)) {
                return Some(format!("name part {part}"));
            }
        }
        None
    }
}

struct CompiledGlob {
    source: String,
    matcher: GlobMatcher,
}

struct CompiledRegex {
    source: String,
    matcher: Regex,
}

fn compile_globs(field: &str, values: &[String]) -> Result<Vec<CompiledGlob>, PlanError> {
    values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            Glob::new(value)
                .map(|glob| CompiledGlob {
                    source: value.clone(),
                    matcher: glob.compile_matcher(),
                })
                .map_err(|error| PlanError {
                    field: format!("{field}[{index}]"),
                    message: error.to_string(),
                })
        })
        .collect()
}

fn parse_optional_duration(
    field: &str,
    value: Option<&str>,
) -> Result<Option<Duration>, PlanError> {
    value
        .map(|value| {
            humantime::parse_duration(value).map_err(|error| PlanError {
                field: field.to_owned(),
                message: error.to_string(),
            })
        })
        .transpose()
}

fn age_seconds(now: i64, timestamp: Option<i64>) -> Option<u64> {
    let timestamp = timestamp?;
    now.checked_sub(timestamp)?.try_into().ok()
}

fn remove(
    resource: InventoryItem,
    disposition: Disposition,
    rule: &str,
    age_seconds: Option<u64>,
    reason: String,
) -> Decision {
    Decision {
        resource,
        disposition,
        matched_rule: Some(rule.to_owned()),
        action: Action::Remove,
        age_seconds,
        reason,
    }
}

fn keep(
    resource: InventoryItem,
    disposition: Disposition,
    matched_rule: Option<String>,
    reason: String,
) -> Decision {
    Decision {
        resource,
        disposition,
        matched_rule,
        action: Action::Keep,
        age_seconds: None,
        reason,
    }
}

/// Explain a keep whose policy waits on the observed-unreferenced clock.
///
/// A resource with no measurement is reported as newly observed rather than as
/// an unknown age: the first pass that sees it starts its clock at zero.
fn observed_wait_reason(
    selector: &str,
    condition: &str,
    unreferenced_age: Option<u64>,
    threshold: Duration,
) -> String {
    unreferenced_age.map_or_else(
        || {
            format!(
                "matched {selector}; first observed {condition} now, so its {} clock starts at zero",
                format_duration(threshold.as_secs())
            )
        },
        |seconds| {
            format!(
                "matched {selector}; observed {condition} for {} which is below {}",
                format_duration(seconds),
                format_duration(threshold.as_secs())
            )
        },
    )
}

fn unowned(resource: InventoryItem) -> Decision {
    keep(
        resource,
        Disposition::Unowned,
        None,
        "no rule matched".to_owned(),
    )
}

fn format_duration(seconds: u64) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;

    if seconds >= DAY {
        format!("{}d", seconds / DAY)
    } else if seconds >= HOUR {
        format!("{}h", seconds / HOUR)
    } else if seconds >= MINUTE {
        format!("{}m", seconds / MINUTE)
    } else {
        format!("{seconds}s")
    }
}

#[derive(Default)]
struct ResourceCounts {
    containers: usize,
    images: usize,
    volumes: usize,
    networks: usize,
    build_cache: usize,
}

impl ResourceCounts {
    fn from_decisions(decisions: &[Decision]) -> Self {
        let mut counts = Self::default();
        for decision in decisions {
            match decision.resource.kind {
                ResourceKind::Container => counts.containers += 1,
                ResourceKind::Image => counts.images += 1,
                ResourceKind::Volume => counts.volumes += 1,
                ResourceKind::Network => counts.networks += 1,
                ResourceKind::BuildCache => counts.build_cache += 1,
            }
        }
        counts
    }

    fn render(&self, label: &str) -> String {
        format!(
            "{label}: containers={} images={} volumes={} networks={} build_cache={}",
            self.containers, self.images, self.volumes, self.networks, self.build_cache
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const NOW: i64 = 2_000_000;

    fn config(source: &str) -> Config {
        let config = Config::parse(source, Path::new("test.toml")).expect("parse config");
        config.validate().expect("validate config");
        config
    }

    fn item(kind: ResourceKind, name: &str) -> InventoryItem {
        InventoryItem {
            kind,
            id: format!("id-{name}"),
            name: name.to_owned(),
            search_names: vec![name.to_owned()],
            parent_ids: Vec::new(),
            labels: BTreeMap::new(),
            mounts: Vec::new(),
            state: ResourceState::Available,
            created_at: Some(NOW - 86_400),
            state_since: None,
            size: None,
            referenced: false,
            dangling: false,
            system: false,
        }
    }

    #[test]
    fn evaluates_owned_resource_types_and_sorts_output() {
        let config = config(
            r#"
[[rules.containers]]
name = "containers"
select.names = ["^agent-c"]
stopped_ttl = "1h"

[[rules.images]]
name = "images"
select.name_parts = ["agent-i"]
dangling = true
unused_for = "1h"

[[rules.volumes]]
name = "volumes"
select.labels = ["agent.volume=true"]
orphan_for = "1h"

[[rules.networks]]
name = "networks"
select.names = ["^agent-n"]
orphan = true
orphan_for = "1h"
"#,
        );
        let mut container = item(ResourceKind::Container, "agent-c");
        container.state = ResourceState::Stopped;
        container.state_since = Some(NOW - 7_200);
        let mut image = item(ResourceKind::Image, "agent-i:latest");
        image.dangling = true;
        let mut volume = item(ResourceKind::Volume, "agent-v");
        volume
            .labels
            .insert("agent.volume".to_owned(), "true".to_owned());
        let network = item(ResourceKind::Network, "agent-n");

        // Volumes, images, and networks age on the observed-unreferenced
        // clock, so this pass supplies a record that started a day ago.
        let inventory = vec![network, volume, image, container];
        let observations = ObservationState::default().folded(&inventory, NOW - 86_400);
        let plan = build_plan_with_context(
            &config,
            inventory,
            NOW,
            &PlanContext {
                observations: &observations,
                ..PlanContext::default()
            },
        )
        .expect("build plan");

        assert_eq!(plan.pending_count(), 4);
        assert_eq!(plan.decisions[0].resource.kind, ResourceKind::Container);
        assert_eq!(plan.decisions[3].resource.kind, ResourceKind::Network);
        assert_eq!(
            plan.render_table(),
            concat!(
                "TYPE       NAME            STATE      AGE  DISPOSITION  RULE        ACTION  REASON\n",
                "container  agent-c         stopped    2h   owned        containers  remove  matched name regex ^agent-c; state age 2h meets 1h\n",
                "image      agent-i:latest  available  1d   owned        images      remove  matched name part agent-i; unreferenced dangling image; observed unreferenced for 1d which meets 1h\n",
                "volume     agent-v         available  1d   owned        volumes     remove  matched label agent.volume=true (agent.volume=true); observed unattached for 1d which meets 1h\n",
                "network    agent-n         available  1d   owned        networks    remove  matched name regex ^agent-n; observed empty for 1d which meets 1h\n",
                "\nPending removals: 4\n",
                "Scanned: containers=1 images=1 volumes=1 networks=1 build_cache=0\n",
            )
        );
    }

    #[test]
    fn build_cache_age_and_budget_are_deterministic_and_unscoped() {
        let config = config(
            r#"
[rules.build_cache]
older_than = "12h"
max_bytes = 100
allow_unscoped = true
"#,
        );
        let mut old = item(ResourceKind::BuildCache, "old-cache");
        old.state_since = Some(NOW - 86_400);
        old.size = Some(40);
        let mut oldest = item(ResourceKind::BuildCache, "oldest-cache");
        oldest.state_since = Some(NOW - 7_200);
        oldest.size = Some(80);
        let mut newest = item(ResourceKind::BuildCache, "newest-cache");
        newest.state_since = Some(NOW - 3_600);
        newest.size = Some(80);
        let mut in_use = item(ResourceKind::BuildCache, "in-use-cache");
        in_use.state_since = Some(NOW - 86_400);
        in_use.size = Some(0);
        in_use.referenced = true;

        let plan = build_plan(&config, vec![newest, in_use, oldest, old], NOW).expect("plan");
        let removals = plan
            .decisions
            .iter()
            .filter(|decision| decision.action == Action::Remove)
            .map(|decision| decision.resource.name.as_str())
            .collect::<Vec<_>>();

        assert_eq!(removals, vec!["old-cache", "oldest-cache"]);
        assert!(plan.decisions.iter().all(|decision| {
            decision.disposition == Disposition::AuthorizedUnscoped
                && decision.matched_rule.as_deref() == Some("build-cache")
        }));
        let in_use = plan
            .decisions
            .iter()
            .find(|decision| decision.resource.name == "in-use-cache")
            .expect("in-use decision");
        assert_eq!(in_use.action, Action::Keep);
        assert!(in_use.reason.contains("in use"));
    }

    #[test]
    fn build_cache_budget_keeps_unknown_size_records() {
        let config = config(
            r"
[rules.build_cache]
max_bytes = 0
allow_unscoped = true
",
        );
        let mut unknown = item(ResourceKind::BuildCache, "unknown-size");
        unknown.state_since = Some(NOW - 86_400);

        let plan = build_plan(&config, vec![unknown], NOW).expect("plan");

        assert_eq!(plan.pending_count(), 0);
        assert_eq!(plan.decisions[0].action, Action::Keep);
    }

    #[test]
    fn protection_wins_before_the_first_matching_rule() {
        let config = config(
            r#"
[protect]
names = ["^agent-safe$"]

[[rules.networks]]
name = "first"
select.names = ["^agent-"]
orphan = true

[[rules.networks]]
name = "second"
select.names = ["safe"]
orphan = true
"#,
        );
        let plan = build_plan(
            &config,
            vec![item(ResourceKind::Network, "agent-safe")],
            NOW,
        )
        .expect("build plan");
        let decision = &plan.decisions[0];
        assert_eq!(decision.disposition, Disposition::Protected);
        assert_eq!(decision.action, Action::Keep);
        assert_eq!(decision.matched_rule, None);
    }

    #[test]
    fn typed_runtime_protection_wins_without_affecting_other_resource_kinds() {
        use crate::state::{ProtectionEntry, ProtectionKind, ProtectionState};

        let config = config(
            r#"
[[rules.networks]]
name = "networks"
select.names = ["^shared$"]
orphan = true

[[rules.volumes]]
name = "volumes"
select.names = ["^shared$"]
orphan_for = "1s"
"#,
        );
        let mut volume = item(ResourceKind::Volume, "shared");
        volume.created_at = Some(NOW - 60);
        let protection = ProtectionState {
            schema_version: 1,
            entries: vec![ProtectionEntry {
                kind: ProtectionKind::Network,
                value: "shared".to_owned(),
            }],
        };

        let inventory = vec![item(ResourceKind::Network, "shared"), volume];
        let observations = ObservationState::default().folded(&inventory, NOW - 60);
        let plan = build_plan_with_context(
            &config,
            inventory,
            NOW,
            &PlanContext {
                protection: &protection,
                observations: &observations,
            },
        )
        .expect("plan");

        let network = plan
            .decisions
            .iter()
            .find(|decision| decision.resource.kind == ResourceKind::Network)
            .expect("network decision");
        let volume = plan
            .decisions
            .iter()
            .find(|decision| decision.resource.kind == ResourceKind::Volume)
            .expect("volume decision");
        assert_eq!(network.disposition, Disposition::Protected);
        assert_eq!(network.action, Action::Keep);
        assert_eq!(volume.action, Action::Remove);
    }

    #[test]
    fn one_label_entry_protects_a_whole_family_from_every_matching_rule() {
        let config = config(
            r#"
[[rules.images]]
name = "images"
select.labels = ["com.docker.compose.project=*"]
dangling = true
unused_for = "1h"

[[rules.volumes]]
name = "volumes"
select.labels = ["com.docker.compose.project=*"]
orphan_for = "1h"

[[rules.networks]]
name = "networks"
select.labels = ["com.docker.compose.project=*"]
orphan = true
orphan_for = "1h"
"#,
        );
        let stack = |kind, name, project: &str| {
            let mut resource = item(kind, name);
            resource
                .labels
                .insert("com.docker.compose.project".to_owned(), project.to_owned());
            resource.dangling = kind == ResourceKind::Image;
            resource
        };
        let inventory = vec![
            stack(ResourceKind::Image, "immich/server:latest", "immich"),
            stack(ResourceKind::Volume, "immich_pgdata", "immich"),
            stack(ResourceKind::Network, "immich_default", "immich"),
            stack(ResourceKind::Volume, "scratch_data", "scratch"),
        ];
        let observations = ObservationState::default().folded(&inventory, NOW - 86_400);
        let protection = ProtectionState {
            schema_version: 2,
            entries: vec![crate::state::ProtectionEntry {
                kind: crate::state::ProtectionKind::Label,
                value: "com.docker.compose.project=immich".to_owned(),
            }],
        };
        let plan = build_plan_with_context(
            &config,
            inventory,
            NOW,
            &PlanContext {
                protection: &protection,
                observations: &observations,
            },
        )
        .expect("plan");

        for decision in &plan.decisions {
            let project = decision
                .resource
                .labels
                .get("com.docker.compose.project")
                .map(String::as_str);
            if project == Some("immich") {
                assert_eq!(
                    decision.disposition,
                    Disposition::Protected,
                    "{} must be protected",
                    decision.resource.name
                );
                assert_eq!(decision.action, Action::Keep);
                assert_eq!(
                    decision.reason,
                    "matched runtime protection label com.docker.compose.project=immich"
                );
            } else {
                // The unrelated family stays fully eligible.
                assert_eq!(decision.action, Action::Remove, "{decision:?}");
            }
        }
        assert_eq!(plan.pending_count(), 1);
    }

    #[test]
    fn a_rule_without_an_age_floor_never_removes_on_first_observation() {
        // Volumes already failed safe; networks and images now match them, so
        // no rule can delete a resource the observed clock has not measured.
        let config = config(
            r#"
[[rules.images]]
name = "images"
select.name_parts = ["agent-i"]
dangling = true

[[rules.volumes]]
name = "volumes"
select.name_parts = ["agent-v"]

[[rules.networks]]
name = "networks"
select.names = ["^agent-n"]
orphan = true
"#,
        );
        let mut image = item(ResourceKind::Image, "agent-i:latest");
        image.dangling = true;
        let inventory = vec![
            image,
            item(ResourceKind::Volume, "agent-v"),
            item(ResourceKind::Network, "agent-n"),
        ];
        // A record a day old: age is not the reason these survive.
        let observations = ObservationState::default().folded(&inventory, NOW - 86_400);
        let plan = build_plan_with_context(
            &config,
            inventory,
            NOW,
            &PlanContext {
                observations: &observations,
                ..PlanContext::default()
            },
        )
        .expect("plan");

        assert_eq!(plan.pending_count(), 0);
        for decision in &plan.decisions {
            assert!(
                decision.reason.contains("no orphan age policy is set")
                    || decision.reason.contains("no unused age policy is set"),
                "{decision:?}"
            );
        }
    }

    #[test]
    fn unowned_resources_are_never_removals() {
        let config = config(
            r#"
[[rules.networks]]
name = "agents"
select.names = ["^agent-"]
orphan = true
"#,
        );
        let plan = build_plan(
            &config,
            vec![item(ResourceKind::Network, "production")],
            NOW,
        )
        .expect("build plan");
        assert_eq!(plan.decisions[0].disposition, Disposition::Unowned);
        assert_eq!(plan.decisions[0].action, Action::Keep);
    }

    #[test]
    fn unscoped_escape_hatch_is_visible_in_plan() {
        let config = config(
            r#"
[[rules.images]]
name = "all-dangling"
scope = "all"
allow_unscoped = true
select.name_parts = ["sha256:"]
dangling = true
unused_for = "1h"
"#,
        );
        let mut image = item(ResourceKind::Image, "sha256:abc");
        image.dangling = true;
        let inventory = vec![image];
        let observations = ObservationState::default().folded(&inventory, NOW - 86_400);
        let plan = build_plan_with_context(
            &config,
            inventory,
            NOW,
            &PlanContext {
                observations: &observations,
                ..PlanContext::default()
            },
        )
        .expect("build plan");
        assert_eq!(
            plan.decisions[0].disposition,
            Disposition::AuthorizedUnscoped
        );
        let output = plan.render_table();
        assert!(output.contains("authorized-unscoped"));
        assert!(output.contains("WARNING: 1 pending removal(s) use authorized-unscoped policy."));
    }

    #[test]
    fn thresholds_and_references_prevent_early_removal() {
        let config = config(
            r#"
[[rules.containers]]
name = "containers"
select.names = ["^agent-"]
stopped_ttl = "2h"

[[rules.images]]
name = "images"
select.name_parts = ["agent-"]
dangling = true
"#,
        );
        let mut container = item(ResourceKind::Container, "agent-container");
        container.state = ResourceState::Stopped;
        container.state_since = Some(NOW - 60);
        let mut image = item(ResourceKind::Image, "agent-image");
        image.dangling = true;
        image.referenced = true;
        let plan = build_plan(&config, vec![container, image], NOW).expect("build plan");
        assert_eq!(plan.pending_count(), 0);
        assert_eq!(
            plan.render_table(),
            "No removals pending.\nScanned: containers=1 images=1 volumes=0 networks=0 build_cache=0\n"
        );
    }

    #[test]
    fn an_old_volume_freshly_detached_survives_its_first_observation() {
        let config = config(
            r#"
[[rules.volumes]]
name = "volumes"
select.names = ["^data$"]
orphan_for = "1h"
"#,
        );
        // Six months old, detached seconds ago: the creation-age reading would
        // delete it immediately. The observed clock starts now instead.
        let mut volume = item(ResourceKind::Volume, "data");
        volume.created_at = Some(NOW - 15_552_000);
        let inventory = vec![volume];

        let first = ObservationState::default().folded(&inventory, NOW);
        let plan = build_plan_with_context(
            &config,
            inventory.clone(),
            NOW,
            &PlanContext {
                observations: &first,
                ..PlanContext::default()
            },
        )
        .expect("first pass");
        assert_eq!(plan.pending_count(), 0);
        assert!(plan.decisions[0]
            .reason
            .contains("observed unattached for 0s which is below 1h"));

        // Only after a full TTL of continuous detachment does it become eligible.
        let later = NOW + 3_600;
        let carried = first.folded(&inventory, later);
        let plan = build_plan_with_context(
            &config,
            inventory,
            later,
            &PlanContext {
                observations: &carried,
                ..PlanContext::default()
            },
        )
        .expect("later pass");
        assert_eq!(plan.pending_count(), 1);
        assert!(plan.decisions[0]
            .reason
            .contains("observed unattached for 1h which meets 1h"));
    }

    #[test]
    fn a_network_emptied_by_compose_down_waits_for_its_orphan_floor() {
        let config = config(
            r#"
[[rules.networks]]
name = "networks"
select.names = ["^project_default$"]
orphan = true
orphan_for = "2h"
"#,
        );
        let network = item(ResourceKind::Network, "project_default");
        let inventory = vec![network];

        let first = ObservationState::default().folded(&inventory, NOW);
        let plan = build_plan_with_context(
            &config,
            inventory.clone(),
            NOW,
            &PlanContext {
                observations: &first,
                ..PlanContext::default()
            },
        )
        .expect("first pass");
        assert_eq!(plan.pending_count(), 0);

        let later = NOW + 7_200;
        let carried = first.folded(&inventory, later);
        let plan = build_plan_with_context(
            &config,
            inventory,
            later,
            &PlanContext {
                observations: &carried,
                ..PlanContext::default()
            },
        )
        .expect("later pass");
        assert_eq!(plan.pending_count(), 1);
        assert!(plan.decisions[0]
            .reason
            .contains("observed empty for 2h which meets 2h"));
    }

    #[test]
    fn a_reattached_resource_restarts_its_clock_and_survives() {
        let config = config(
            r#"
[[rules.volumes]]
name = "volumes"
select.names = ["^data$"]
orphan_for = "1h"
"#,
        );
        let detached = vec![item(ResourceKind::Volume, "data")];
        let mut attached_item = item(ResourceKind::Volume, "data");
        attached_item.referenced = true;
        let attached = vec![attached_item];

        let aged = ObservationState::default().folded(&detached, NOW - 7_200);
        let reattached = aged.folded(&attached, NOW);
        let detached_again = reattached.folded(&detached, NOW);

        let plan = build_plan_with_context(
            &config,
            detached.clone(),
            NOW,
            &PlanContext {
                observations: &detached_again,
                ..PlanContext::default()
            },
        )
        .expect("plan");
        assert_eq!(plan.pending_count(), 0);
    }

    #[test]
    fn without_durable_state_nothing_accumulates_or_is_removed() {
        let config = config(
            r#"
[[rules.volumes]]
name = "volumes"
select.names = ["^data$"]
orphan_for = "1s"

[[rules.images]]
name = "images"
select.names = ["^stale$"]
dangling = true
unused_for = "1s"

[[rules.networks]]
name = "networks"
select.names = ["^empty$"]
orphan = true
orphan_for = "1s"
"#,
        );
        let mut image = item(ResourceKind::Image, "stale");
        image.dangling = true;
        let inventory = vec![
            item(ResourceKind::Volume, "data"),
            image,
            item(ResourceKind::Network, "empty"),
        ];

        // An ephemeral host that cannot persist the record never accumulates
        // observed time, so every pass keeps every resource.
        let plan = build_plan(&config, inventory, NOW).expect("plan");
        assert_eq!(plan.pending_count(), 0);
    }

    #[test]
    fn system_networks_are_implicitly_protected() {
        let config = config(
            r#"
[[rules.networks]]
name = "everything"
scope = "all"
allow_unscoped = true
select.names = [".*"]
orphan = true
"#,
        );
        let mut network = item(ResourceKind::Network, "bridge");
        network.system = true;
        let plan = build_plan(&config, vec![network], NOW).expect("build plan");
        assert_eq!(plan.decisions[0].disposition, Disposition::Protected);
        assert_eq!(plan.pending_count(), 0);
    }
}
