//! Strict configuration loading and validation.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

/// The annotated configuration emitted by `docker_maid config default`.
pub const DEFAULT_CONFIG: &str = r#"# docker_maid configuration
# This starter is safe: it contains no active cleanup rule.
# Run `docker_maid tui` for daemon-backed discovery and a reviewed proposal.

# [defaults]
# interval = "5m"       # quiet-host backstop; Docker events also wake a pass

# [protect]
# names = ["^postgres-prod$"]
# labels = ["com.example.prod=true"]

# Rules need explicit ownership selectors. The configurator creates exact
# label or operator-approved name-prefix selectors and stable managed IDs.
# [[rules.containers]]
# id = "manual/example-container-rule"
# name = "example-containers"
# stopped_ttl = "24h"
# select.labels = ["com.example.owner=agent"]

# Volume, image, and network age floors measure continuous observed-unreferenced
# time, recorded under $XDG_STATE_HOME/docker_maid/observation.toml. The first
# pass that sees a resource unreferenced starts its clock at zero.
#
# [[rules.networks]]
# id = "manual/example-network-rule"
# name = "example-networks"
# orphan = true
# orphan_for = "2h"
# select.labels = ["com.example.owner=agent"]

# Build cache has no labels or names. It is always a separate, explicit,
# authorized-unscoped decision:
# [rules.build_cache]
# id = "manual/build-cache"
# older_than = "30d"
# max_bytes = 21474836480
# allow_unscoped = true

# [tui]
# refresh = "5m"
"#;

/// Configuration keys this build has retired, each with its migration.
///
/// A retired key is not a typo. An older file may carry it, and the strict
/// schema rejects it as unknown like any other stray key. Naming the
/// retirement turns that generic failure into an instruction the operator can
/// act on without reading a changelog.
const RETIRED_KEYS: &[(&str, &str)] = &[
    (
        "adopt",
        "this was a container rule key; a rule match already means the resource \
         is owned, so it never changed a decision. Delete the line",
    ),
    (
        "report",
        "this table promised a report file that nothing ever wrote. It parsed \
         and was then ignored, so enabling it changed nothing. Delete the table",
    ),
    (
        "log_level",
        "this was a `defaults` key that nothing ever read; this tool does not \
         have a configurable log level. Delete the line",
    ),
    (
        "mouse",
        "this was a `tui` key that nothing ever read; the dashboard is driven \
         by the keyboard. Delete the line",
    ),
];

/// Every configuration key this build has retired.
///
/// Published so the agent skills can be held to it: a document that taught a
/// retired key would send an agent straight to a parse failure, and the
/// retirement note is written for a file that already exists rather than for
/// new advice.
#[must_use]
pub fn retired_key_names() -> Vec<&'static str> {
    RETIRED_KEYS.iter().map(|(key, _)| *key).collect()
}

/// Return the retired key a parse failure names, with its migration note.
///
/// A rendered TOML error quotes the operator's own source line above its
/// message, so a plain substring search would fire on a file that merely
/// contains the phrase inside a string value. Only a line that *starts* with
/// the phrase is serde's own message: every quoted source line carries a
/// `N | ` gutter. A near-miss typo such as `adpot` therefore keeps the plain
/// unknown-field message instead of borrowing guidance meant for a real
/// retirement. The key is matched under any table, and the note says which
/// table it came from, so the guidance stays true for a file that misplaced it.
fn retired_key_hint(source: &toml::de::Error) -> Option<(&'static str, &'static str)> {
    let rendered = source.to_string();
    RETIRED_KEYS
        .iter()
        .find(|(key, _)| {
            let message = format!("unknown field `{key}`");
            rendered.lines().any(|line| line.starts_with(&message))
        })
        .copied()
}

#[derive(Debug)]
pub enum ConfigError {
    NotFound {
        searched: Vec<PathBuf>,
    },
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    Serialize(toml::ser::Error),
    Validation(Vec<String>),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { searched } => {
                let paths = searched
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(formatter, "no configuration file found; searched: {paths}")
            }
            Self::Read { path, source } => {
                write!(formatter, "cannot read {}: {source}", path.display())
            }
            Self::Parse { path, source } => {
                let hint = retired_key_hint(source).map_or_else(String::new, |(key, hint)| {
                    format!("\nretired key `{key}`: {hint}")
                });
                write!(
                    formatter,
                    "invalid configuration {}: {source}{hint}",
                    path.display()
                )
            }
            Self::Serialize(source) => {
                write!(formatter, "cannot serialize configuration: {source}")
            }
            Self::Validation(errors) => {
                write!(
                    formatter,
                    "configuration validation failed: {}",
                    errors.join("; ")
                )
            }
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::Parse { source, .. } => Some(source),
            Self::Serialize(source) => Some(source),
            Self::NotFound { .. } | Self::Validation(_) => None,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    #[serde(skip_serializing_if = "Defaults::is_empty")]
    pub defaults: Defaults,
    #[serde(skip_serializing_if = "Protection::is_empty")]
    pub protect: Protection,
    #[serde(skip_serializing_if = "Rules::is_empty")]
    pub rules: Rules,
    #[serde(skip_serializing_if = "Tui::is_empty")]
    pub tui: Tui,
}

impl Config {
    /// Parse TOML using the strict configuration schema.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Parse`] when the TOML is malformed, contains an
    /// unknown field, or has a value with the wrong type.
    pub fn parse(source: &str, path: &Path) -> Result<Self, ConfigError> {
        let config = toml::from_str(source).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(config)
    }

    /// Check cross-field safety invariants and duration values.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Validation`] with every validation failure found.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut errors = Vec::new();
        let mut rule_ids = std::collections::BTreeSet::new();

        validate_regexes("protect.names", &self.protect.names, &mut errors);
        validate_globs("protect.labels", &self.protect.labels, &mut errors);

        validate_duration(
            "defaults.interval",
            self.defaults.interval.as_deref(),
            &mut errors,
        );
        validate_duration("tui.refresh", self.tui.refresh.as_deref(), &mut errors);

        for (index, rule) in self.rules.containers.iter().enumerate() {
            let field = format!("rules.containers[{index}]");
            validate_common_rule(&field, &rule.common, &mut errors);
            validate_rule_id(
                &field,
                rule.common.id.as_deref(),
                &mut rule_ids,
                &mut errors,
            );
            validate_duration(
                &format!("{field}.stopped_ttl"),
                rule.stopped_ttl.as_deref(),
                &mut errors,
            );
            validate_duration(
                &format!("{field}.running_ttl"),
                rule.running_ttl.as_deref(),
                &mut errors,
            );
        }

        for (index, rule) in self.rules.images.iter().enumerate() {
            let field = format!("rules.images[{index}]");
            validate_common_rule(&field, &rule.common, &mut errors);
            validate_rule_id(
                &field,
                rule.common.id.as_deref(),
                &mut rule_ids,
                &mut errors,
            );
            validate_globs(
                &format!("{field}.image_tag_patterns"),
                &rule.image_tag_patterns,
                &mut errors,
            );
            validate_duration(
                &format!("{field}.unused_for"),
                rule.unused_for.as_deref(),
                &mut errors,
            );
        }

        for (index, rule) in self.rules.volumes.iter().enumerate() {
            let field = format!("rules.volumes[{index}]");
            validate_common_rule(&field, &rule.common, &mut errors);
            validate_rule_id(
                &field,
                rule.common.id.as_deref(),
                &mut rule_ids,
                &mut errors,
            );
            validate_duration(
                &format!("{field}.orphan_for"),
                rule.orphan_for.as_deref(),
                &mut errors,
            );
        }

        for (index, rule) in self.rules.networks.iter().enumerate() {
            let field = format!("rules.networks[{index}]");
            validate_common_rule(&field, &rule.common, &mut errors);
            validate_rule_id(
                &field,
                rule.common.id.as_deref(),
                &mut rule_ids,
                &mut errors,
            );
            validate_duration(
                &format!("{field}.orphan_for"),
                rule.orphan_for.as_deref(),
                &mut errors,
            );
        }

        validate_build_cache(self.rules.build_cache.as_ref(), &mut rule_ids, &mut errors);

        if errors.is_empty() {
            Ok(())
        } else {
            Err(ConfigError::Validation(errors))
        }
    }

    /// Serialize the parsed configuration as deterministic, normalized TOML.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Serialize`] if serialization fails.
    pub fn to_normalized_toml(&self) -> Result<String, ConfigError> {
        toml::to_string_pretty(self).map_err(ConfigError::Serialize)
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Defaults {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interval: Option<String>,
}

impl Defaults {
    fn is_empty(&self) -> bool {
        self.interval.is_none()
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Protection {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub names: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
}

impl Protection {
    fn is_empty(&self) -> bool {
        self.names.is_empty() && self.labels.is_empty()
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Rules {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub containers: Vec<ContainerRule>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageRule>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<VolumeRule>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub networks: Vec<NetworkRule>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_cache: Option<BuildCacheRule>,
}

impl Rules {
    fn is_empty(&self) -> bool {
        self.containers.is_empty()
            && self.images.is_empty()
            && self.volumes.is_empty()
            && self.networks.is_empty()
            && self.build_cache.is_none()
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct CommonRule {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub select: Selectors,
    pub scope: RuleScope,
    pub allow_unscoped: bool,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuleScope {
    #[default]
    Owned,
    All,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Selectors {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub names: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub name_parts: Vec<String>,
}

impl Selectors {
    fn is_empty(&self) -> bool {
        self.labels.is_empty() && self.names.is_empty() && self.name_parts.is_empty()
    }

    fn has_blank(&self) -> bool {
        self.labels
            .iter()
            .chain(&self.names)
            .chain(&self.name_parts)
            .any(|value| value.trim().is_empty())
    }
}

/// A container rule.
///
/// A rule match is itself the ownership statement, so there is no key that
/// turns adoption on: matching a selector already makes the resource owned.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ContainerRule {
    #[serde(flatten)]
    pub common: CommonRule,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stopped_ttl: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub running_ttl: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ImageRule {
    #[serde(flatten)]
    pub common: CommonRule,
    pub dangling: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unused_for: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub image_tag_patterns: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct VolumeRule {
    #[serde(flatten)]
    pub common: CommonRule,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orphan_for: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkRule {
    #[serde(flatten)]
    pub common: CommonRule,
    pub orphan: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orphan_for: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct BuildCacheRule {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub older_than: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
    pub allow_unscoped: bool,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Tui {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh: Option<String>,
}

impl Tui {
    fn is_empty(&self) -> bool {
        self.refresh.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedConfig {
    pub path: PathBuf,
    pub config: Config,
    pub source: String,
}

/// Resolve, read, parse, and validate a configuration file.
///
/// # Errors
///
/// Returns an error when no implicit configuration exists, the selected file
/// cannot be read, the TOML is invalid, or a safety invariant fails.
pub fn load_config(
    explicit: Option<&Path>,
    current_dir: &Path,
    xdg_config_home: Option<&Path>,
) -> Result<LoadedConfig, ConfigError> {
    let path = resolve_config_path(explicit, current_dir, xdg_config_home)?;
    let source = fs::read_to_string(&path).map_err(|source| ConfigError::Read {
        path: path.clone(),
        source,
    })?;
    let config = Config::parse(&source, &path)?;
    config.validate()?;
    Ok(LoadedConfig {
        path,
        config,
        source,
    })
}

/// Select a configuration path using explicit, local, then XDG precedence.
///
/// An explicit path is returned even when it does not exist so the later read
/// can report that exact operator-selected path.
///
/// # Errors
///
/// Returns [`ConfigError::NotFound`] when no explicit path was supplied and no
/// local or XDG configuration file exists.
pub fn resolve_config_path(
    explicit: Option<&Path>,
    current_dir: &Path,
    xdg_config_home: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    if let Some(path) = explicit {
        return Ok(path.to_path_buf());
    }

    let mut searched = vec![current_dir.join("docker_maid.toml")];
    if let Some(config_home) = xdg_config_home {
        searched.push(config_home.join("docker_maid/config.toml"));
    }

    searched
        .iter()
        .find(|path| path.is_file())
        .cloned()
        .ok_or(ConfigError::NotFound { searched })
}

fn validate_common_rule(field: &str, rule: &CommonRule, errors: &mut Vec<String>) {
    if rule.name.trim().is_empty() {
        errors.push(format!("{field}.name must not be blank"));
    }
    if rule.select.is_empty() {
        errors.push(format!("{field}.select must contain at least one selector"));
    } else if rule.select.has_blank() {
        errors.push(format!("{field}.select must not contain blank selectors"));
    }

    validate_regexes(&format!("{field}.select.names"), &rule.select.names, errors);
    validate_globs(
        &format!("{field}.select.labels"),
        &rule.select.labels,
        errors,
    );

    match (&rule.scope, rule.allow_unscoped) {
        (RuleScope::All, false) => errors.push(format!(
            "{field}.allow_unscoped must be true when scope = \"all\""
        )),
        (RuleScope::Owned, true) => errors.push(format!(
            "{field}.scope must be \"all\" when allow_unscoped = true"
        )),
        _ => {}
    }
}

fn validate_rule_id(
    field: &str,
    id: Option<&str>,
    ids: &mut std::collections::BTreeSet<String>,
    errors: &mut Vec<String>,
) {
    let Some(id) = id else {
        return;
    };
    if id.trim().is_empty() {
        errors.push(format!("{field}.id must not be blank"));
    } else if !ids.insert(id.to_owned()) {
        errors.push(format!("{field}.id duplicates rule id {id:?}"));
    }
}

fn validate_build_cache(
    build_cache: Option<&BuildCacheRule>,
    rule_ids: &mut std::collections::BTreeSet<String>,
    errors: &mut Vec<String>,
) {
    let Some(build_cache) = build_cache else {
        return;
    };
    validate_rule_id(
        "rules.build_cache",
        build_cache.id.as_deref(),
        rule_ids,
        errors,
    );
    if !build_cache.allow_unscoped {
        errors.push(
            "rules.build_cache.allow_unscoped must be true because Docker build-cache records have no ownership metadata"
                .to_owned(),
        );
    }
    validate_duration(
        "rules.build_cache.older_than",
        build_cache.older_than.as_deref(),
        errors,
    );
    if build_cache.older_than.is_none() && build_cache.max_bytes.is_none() {
        errors.push("rules.build_cache must set older_than, max_bytes, or both".to_owned());
    }
}

fn validate_regexes(field: &str, values: &[String], errors: &mut Vec<String>) {
    for (index, value) in values.iter().enumerate() {
        if value.trim().is_empty() {
            errors.push(format!("{field}[{index}] must not be blank"));
        } else if let Err(error) = regex::Regex::new(value) {
            errors.push(format!("{field}[{index}] is not a valid regex: {error}"));
        }
    }
}

fn validate_globs(field: &str, values: &[String], errors: &mut Vec<String>) {
    for (index, value) in values.iter().enumerate() {
        if value.trim().is_empty() {
            errors.push(format!("{field}[{index}] must not be blank"));
        } else if let Err(error) = globset::Glob::new(value) {
            errors.push(format!("{field}[{index}] is not a valid glob: {error}"));
        }
    }
}

fn validate_duration(field: &str, value: Option<&str>, errors: &mut Vec<String>) {
    if let Some(value) = value {
        match humantime::parse_duration(value) {
            Ok(duration) if duration.is_zero() => {
                errors.push(format!("{field} must be greater than zero"));
            }
            Ok(_) => {}
            Err(error) => errors.push(format!("{field} is not a valid duration: {error}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(label: &str) -> PathBuf {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("docker-maid-{label}-{}-{id}", std::process::id()));
        fs::create_dir_all(&path).expect("create test directory");
        path
    }

    #[test]
    fn default_configuration_round_trips() {
        let config = Config::parse(DEFAULT_CONFIG, Path::new("generated.toml"))
            .expect("parse generated config");
        config.validate().expect("validate generated config");
        let normalized = config.to_normalized_toml().expect("serialize config");
        let reparsed = Config::parse(&normalized, Path::new("normalized.toml"))
            .expect("parse normalized config");
        assert_eq!(config, reparsed);
    }

    #[test]
    fn unknown_nested_key_is_rejected() {
        let source = "[defaults]\nintervall = \"5m\"\n";
        let error = Config::parse(source, Path::new("bad.toml")).expect_err("must reject typo");
        assert!(error.to_string().contains("unknown field `intervall`"));
    }

    #[test]
    fn unknown_rule_key_is_rejected() {
        let source =
            "[[rules.containers]]\nname = \"agents\"\nstopped_ttl = \"2h\"\nselect.names = [\"^agent-\"]\nadpot = true\n";
        let error = Config::parse(source, Path::new("bad.toml")).expect_err("must reject typo");
        let message = error.to_string();
        assert!(message.contains("unknown field `adpot`"));
        // A typo is not a retirement. Rule structs flatten their common
        // fields, so serde cannot list the expected keys here; borrowing a
        // retired key's migration note would point the operator at the wrong
        // fix for a one-letter slip.
        assert!(!message.contains("retired key"));
    }

    #[test]
    fn the_retired_adopt_key_is_refused_and_names_its_migration() {
        let source =
            "[[rules.containers]]\nname = \"agents\"\nstopped_ttl = \"2h\"\nselect.names = [\"^agent-\"]\nadopt = true\n";
        let error =
            Config::parse(source, Path::new("legacy.toml")).expect_err("must reject retired key");
        let message = error.to_string();
        assert!(message.contains("unknown field `adopt`"));
        assert!(message.contains("retired key `adopt`:"));
        assert!(message.contains("a rule match already means the resource is owned"));
        assert!(message.contains("Delete the line"));
    }

    #[test]
    fn every_key_nothing_reads_is_refused_and_names_its_migration() {
        // These three parsed and validated for the whole of v0.1 and were then
        // ignored, so an operator who set them got silence instead of an
        // effect. Silence is the worst outcome: refusing beats accepting a
        // key whose only behaviour is to do nothing.
        for (source, key, phrase) in [
            (
                "[report]\nenabled = true\npath = \"/tmp/r.json\"\nkeep = 30\n",
                "report",
                "promised a report file that nothing ever wrote",
            ),
            (
                "[defaults]\nlog_level = \"info\"\n",
                "log_level",
                "does not have a configurable log level",
            ),
            ("[tui]\nmouse = false\n", "mouse", "driven by the keyboard"),
        ] {
            let error = Config::parse(source, Path::new("legacy.toml"))
                .expect_err("a key nothing reads must be refused, not ignored");
            let message = error.to_string();
            assert!(
                message.contains(&format!("unknown field `{key}`")),
                "{key}: {message}"
            );
            assert!(
                message.contains(&format!("retired key `{key}`:")),
                "{key}: {message}"
            );
            assert!(message.contains(phrase), "{key}: {message}");
        }
    }

    #[test]
    fn the_starter_never_advertises_a_key_this_build_would_refuse() {
        // `config default` is the file an operator or an agent copies from. A
        // commented key it cannot uncomment is a trap, so the starter and the
        // retirement list must never overlap.
        for (key, _) in RETIRED_KEYS {
            assert!(
                !DEFAULT_CONFIG.contains(&format!("{key} =")),
                "the starter still offers the retired key `{key}`"
            );
            assert!(
                !DEFAULT_CONFIG.contains(&format!("[{key}]")),
                "the starter still offers the retired table `{key}`"
            );
        }
    }

    #[test]
    fn a_quoted_source_line_never_triggers_the_retirement_note() {
        // The failure here is `bogus`. A rendered TOML error quotes the
        // operator's own line above its message, so a naive substring search
        // over the whole error would blame a retired key for someone else's
        // typo and hand out a migration that does not apply.
        let source = "[defaults]\nbogus = \"unknown field `adopt`\"\n";
        let error = Config::parse(source, Path::new("quoted.toml")).expect_err("must reject bogus");
        let message = error.to_string();
        assert!(message.contains("unknown field `bogus`"));
        assert!(!message.contains("retired key"));
    }

    #[test]
    fn a_retired_key_under_the_wrong_table_still_reads_truthfully() {
        let error = Config::parse("[defaults]\nadopt = true\n", Path::new("legacy.toml"))
            .expect_err("must reject retired key");
        let message = error.to_string();
        // A retired key is matched by name wherever it appears, so the note
        // has to say which table it belonged to instead of assuming the file
        // put it there. The migration is the same either way.
        assert!(message.contains("retired key `adopt`:"));
        assert!(message.contains("this was a container rule key"));
    }

    #[test]
    fn a_container_rule_serializes_without_any_adoption_key() {
        let source = "[[rules.containers]]\nname = \"agents\"\nstopped_ttl = \"2h\"\nselect.names = [\"^agent-\"]\n";
        let config = Config::parse(source, Path::new("ok.toml")).expect("parse config");
        let normalized = config.to_normalized_toml().expect("serialize config");
        // The schema must not re-offer the key it just retired, in either
        // direction: it is neither written out nor accepted back.
        assert!(!normalized.contains("adopt"));
        Config::parse(&normalized, Path::new("normalized.toml")).expect("reparse normalized");
    }

    #[test]
    fn removing_the_retired_key_leaves_the_rule_identical() {
        let with_key = "[[rules.containers]]\nname = \"agents\"\nstopped_ttl = \"2h\"\nselect.names = [\"^agent-\"]\nadopt = true\n";
        let without_key =
            "[[rules.containers]]\nname = \"agents\"\nstopped_ttl = \"2h\"\nselect.names = [\"^agent-\"]\n";
        Config::parse(with_key, Path::new("legacy.toml")).expect_err("retired key is refused");
        let migrated = Config::parse(without_key, Path::new("migrated.toml")).expect("parse rule");
        // The migration the error asks for is exactly "delete the line", so
        // the surviving rule must still carry every selector and floor.
        let rule = &migrated.rules.containers[0];
        assert_eq!(rule.common.name, "agents");
        assert_eq!(rule.common.scope, RuleScope::Owned);
        assert_eq!(rule.stopped_ttl.as_deref(), Some("2h"));
        assert_eq!(rule.common.select.names, vec!["^agent-".to_owned()]);
    }

    #[test]
    fn invalid_duration_is_rejected() {
        let source = "[defaults]\ninterval = \"soon\"\n";
        let config = Config::parse(source, Path::new("bad.toml")).expect("parse shape");
        let error = config.validate().expect_err("must reject duration");
        assert!(error.to_string().contains("defaults.interval"));
    }

    #[test]
    fn invalid_regex_and_glob_selectors_are_rejected() {
        let source = r#"
[protect]
labels = ["["]

[[rules.networks]]
name = "bad-regex"
select.names = ["("]
orphan = true
"#;
        let config = Config::parse(source, Path::new("bad.toml")).expect("parse shape");
        let error = config.validate().expect_err("must reject invalid patterns");
        let message = error.to_string();
        assert!(message.contains("protect.labels[0]"));
        assert!(message.contains("rules.networks[0].select.names[0]"));
    }

    #[test]
    fn selector_less_rule_is_rejected() {
        let source = "[[rules.containers]]\nname = \"unsafe\"\nstopped_ttl = \"1h\"\n";
        let config = Config::parse(source, Path::new("bad.toml")).expect("parse shape");
        let error = config.validate().expect_err("must require selector");
        assert!(error
            .to_string()
            .contains("must contain at least one selector"));
    }

    #[test]
    fn build_cache_requires_unscoped_authorization() {
        let source = "[rules.build_cache]\nolder_than = \"7d\"\n";
        let config = Config::parse(source, Path::new("bad.toml")).expect("parse shape");
        let error = config.validate().expect_err("must require authorization");
        assert!(error.to_string().contains("allow_unscoped must be true"));
    }

    #[test]
    fn build_cache_requires_a_removal_policy() {
        let source = "[rules.build_cache]\nallow_unscoped = true\n";
        let config = Config::parse(source, Path::new("bad.toml")).expect("parse shape");
        let error = config.validate().expect_err("must require a policy");
        assert!(error
            .to_string()
            .contains("must set older_than, max_bytes, or both"));
    }

    #[test]
    fn build_cache_live_fixture_is_valid() {
        let source = include_str!("../tests/fixtures/build_cache_apply.toml");
        let config =
            Config::parse(source, Path::new("build_cache_apply.toml")).expect("parse fixture");

        config.validate().expect("validate fixture");
    }

    #[test]
    fn all_scope_requires_unscoped_authorization() {
        let source =
            "[[rules.images]]\nname = \"all-images\"\nscope = \"all\"\nselect.name_parts = [\"agent-\"]\n";
        let config = Config::parse(source, Path::new("bad.toml")).expect("parse shape");
        let error = config.validate().expect_err("must require authorization");
        assert!(error.to_string().contains("allow_unscoped must be true"));
    }

    #[test]
    fn unscoped_authorization_requires_all_scope() {
        let source =
            "[[rules.images]]\nname = \"all-images\"\nallow_unscoped = true\nselect.name_parts = [\"agent-\"]\n";
        let config = Config::parse(source, Path::new("bad.toml")).expect("parse shape");
        let error = config.validate().expect_err("must require all scope");
        assert!(error.to_string().contains("scope must be \"all\""));
    }

    #[test]
    fn path_resolution_prefers_explicit_then_local_then_xdg() {
        let root = temp_dir("precedence");
        let current = root.join("current");
        let xdg = root.join("xdg");
        fs::create_dir_all(xdg.join("docker_maid")).expect("create xdg directory");
        fs::create_dir_all(&current).expect("create current directory");

        let local = current.join("docker_maid.toml");
        let xdg_file = xdg.join("docker_maid/config.toml");
        let explicit = root.join("explicit.toml");
        fs::write(&local, "").expect("write local config");
        fs::write(&xdg_file, "").expect("write xdg config");

        assert_eq!(
            resolve_config_path(Some(&explicit), &current, Some(&xdg)).expect("explicit path"),
            explicit
        );
        assert_eq!(
            resolve_config_path(None, &current, Some(&xdg)).expect("local path"),
            local
        );
        fs::remove_file(&local).expect("remove local config");
        assert_eq!(
            resolve_config_path(None, &current, Some(&xdg)).expect("xdg path"),
            xdg_file
        );

        fs::remove_dir_all(&root).expect("remove test directory");
    }
}
