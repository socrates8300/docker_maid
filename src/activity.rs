//! Durable JSON Lines activity journal.

use crate::executor::{ExecutionReport, TargetStatus};
use crate::plan::{Action, Plan};
use crate::state::{set_private_file_permissions, StateError, StatePaths};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const SCHEMA_VERSION: u32 = 1;
const MAX_EVENTS: usize = 10_000;
const MAX_BYTES: usize = 5 * 1024 * 1024;
static NEXT_PASS_ID: AtomicU64 = AtomicU64::new(0);
static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub enum ActivityError {
    State(StateError),
    Io {
        path: PathBuf,
        source: io::Error,
    },
    Encode(serde_json::Error),
    Decode {
        path: PathBuf,
        line: usize,
        source: serde_json::Error,
    },
    Invalid(String),
}

impl fmt::Display for ActivityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::State(source) => write!(formatter, "{source}"),
            Self::Io { path, source } => {
                write!(
                    formatter,
                    "cannot access activity journal {}: {source}",
                    path.display()
                )
            }
            Self::Encode(source) => write!(formatter, "cannot encode activity event: {source}"),
            Self::Decode { path, line, source } => write!(
                formatter,
                "invalid activity event {} line {line}: {source}",
                path.display()
            ),
            Self::Invalid(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for ActivityError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::State(source) => Some(source),
            Self::Io { source, .. } => Some(source),
            Self::Encode(source) | Self::Decode { source, .. } => Some(source),
            Self::Invalid(_) => None,
        }
    }
}

impl From<StateError> for ActivityError {
    fn from(source: StateError) -> Self {
        Self::State(source)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActivityEvent {
    pub schema_version: u32,
    pub pass_id: String,
    pub source: String,
    pub sequence: u64,
    pub timestamp: i64,
    pub config_hash: String,
    #[serde(flatten)]
    pub data: EventData,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventData {
    PassStarted,
    Action {
        action: String,
        resource_kind: String,
        resource_id: String,
        resource_name: String,
        matched_rule: String,
        age_seconds: Option<u64>,
        freed_bytes: u64,
        detail: String,
    },
    PassSummary {
        completed_at: i64,
        rule_match_counts: BTreeMap<String, u64>,
        removed_count: u64,
        skipped_count: u64,
        failure_count: u64,
        reclaimed_bytes: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletedPass {
    pub pass_id: String,
    pub source: String,
    pub started_at: i64,
    pub completed_at: i64,
    pub config_hash: String,
    pub actions: Vec<ActivityEvent>,
    pub rule_match_counts: BTreeMap<String, u64>,
    pub removed_count: u64,
    pub skipped_count: u64,
    pub failure_count: u64,
    pub reclaimed_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct ActivityJournal {
    paths: StatePaths,
}

impl ActivityJournal {
    #[must_use]
    pub fn new(paths: StatePaths) -> Self {
        Self { paths }
    }

    /// Resolve the journal from XDG/HOME state conventions.
    ///
    /// # Errors
    ///
    /// Returns an error when no state base can be resolved.
    pub fn from_env() -> Result<Self, ActivityError> {
        Ok(Self::new(StatePaths::from_env()?))
    }

    /// Start and durably record one cleanup pass before mutation.
    ///
    /// # Errors
    ///
    /// Returns an error if the event cannot be safely appended.
    pub fn start_pass(
        &self,
        source: &str,
        config_hash: &str,
        timestamp: i64,
    ) -> Result<ActivityPass, ActivityError> {
        // Refuse mutation when existing audit history is unreadable. This
        // check happens before the caller starts any Docker deletion.
        self.snapshot()?;
        let pass = ActivityPass {
            journal: self.clone(),
            pass_id: next_pass_id(),
            source: source.to_owned(),
            config_hash: config_hash.to_owned(),
            started_at: timestamp,
        };
        pass.journal.append(&ActivityEvent {
            schema_version: SCHEMA_VERSION,
            pass_id: pass.pass_id.clone(),
            source: pass.source.clone(),
            sequence: 0,
            timestamp,
            config_hash: pass.config_hash.clone(),
            data: EventData::PassStarted,
        })?;
        Ok(pass)
    }

    /// Read the most recent completed pass and its correlated actions.
    ///
    /// Incomplete passes are ignored.
    ///
    /// # Errors
    ///
    /// Returns an error when existing journal data cannot be read or parsed.
    pub fn last_completed_pass(&self) -> Result<Option<CompletedPass>, ActivityError> {
        let events = self.snapshot()?;
        let Some(summary) = events
            .iter()
            .rev()
            .find(|event| matches!(event.data, EventData::PassSummary { .. }))
        else {
            return Ok(None);
        };
        let pass_id = summary.pass_id.clone();
        let started_at = events
            .iter()
            .find(|event| event.pass_id == pass_id && matches!(event.data, EventData::PassStarted))
            .map_or(summary.timestamp, |event| event.timestamp);
        let mut actions = events
            .iter()
            .filter(|event| {
                event.pass_id == pass_id && matches!(event.data, EventData::Action { .. })
            })
            .cloned()
            .collect::<Vec<_>>();
        actions.sort_by_key(|event| event.sequence);
        let EventData::PassSummary {
            completed_at,
            rule_match_counts,
            removed_count,
            skipped_count,
            failure_count,
            reclaimed_bytes,
        } = &summary.data
        else {
            unreachable!("selected a pass summary")
        };
        Ok(Some(CompletedPass {
            pass_id,
            source: summary.source.clone(),
            started_at,
            completed_at: *completed_at,
            config_hash: summary.config_hash.clone(),
            actions,
            rule_match_counts: rule_match_counts.clone(),
            removed_count: *removed_count,
            skipped_count: *skipped_count,
            failure_count: *failure_count,
            reclaimed_bytes: *reclaimed_bytes,
        }))
    }

    fn append(&self, event: &ActivityEvent) -> Result<(), ActivityError> {
        self.paths.prepare_root()?;
        let lock_path = self.paths.activity_lock();
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|source| io_error(&lock_path, source))?;
        set_private_file_permissions(&lock_path)?;
        FileExt::lock_exclusive(&lock).map_err(|source| io_error(&lock_path, source))?;

        let active = self.paths.activity_file();
        let mut writer = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&active)
            .map_err(|source| io_error(&active, source))?;
        set_private_file_permissions(&active)?;
        serde_json::to_writer(&mut writer, event).map_err(ActivityError::Encode)?;
        writer
            .write_all(b"\n")
            .map_err(|source| io_error(&active, source))?;
        writer
            .sync_data()
            .map_err(|source| io_error(&active, source))?;
        self.rotate_if_needed()?;
        Ok(())
    }

    fn snapshot(&self) -> Result<Vec<ActivityEvent>, ActivityError> {
        let active = self.paths.activity_file();
        let rotated = self.paths.rotated_activity_file();
        if !active.exists() && !rotated.exists() {
            return Ok(Vec::new());
        }
        self.paths.prepare_root()?;
        let lock_path = self.paths.activity_lock();
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|source| io_error(&lock_path, source))?;
        set_private_file_permissions(&lock_path)?;
        FileExt::lock_shared(&lock).map_err(|source| io_error(&lock_path, source))?;
        read_events(&[rotated, active])
    }

    fn rotate_if_needed(&self) -> Result<(), ActivityError> {
        let active = self.paths.activity_file();
        let rotated = self.paths.rotated_activity_file();
        let total = file_len(&active)? + file_len(&rotated)?;
        if total <= MAX_BYTES as u64 {
            let line_count = count_lines(&active)? + count_lines(&rotated)?;
            if line_count <= MAX_EVENTS {
                return Ok(());
            }
        }

        let mut lines = read_lines(&rotated)?;
        lines.extend(read_lines(&active)?);
        let retained = retain_newest_lines(lines, MAX_EVENTS, MAX_BYTES);
        let bytes = retained.iter().map(|line| line.len() + 1).sum();
        let mut payload = Vec::with_capacity(bytes);
        for line in retained {
            payload.extend_from_slice(&line);
            payload.push(b'\n');
        }
        write_atomic(&self.paths, &rotated, &payload)?;
        let file = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&active)
            .map_err(|source| io_error(&active, source))?;
        file.sync_all()
            .map_err(|source| io_error(&active, source))?;
        Ok(())
    }
}

fn retain_newest_lines(lines: Vec<Vec<u8>>, max_events: usize, max_bytes: usize) -> Vec<Vec<u8>> {
    let mut retained = Vec::new();
    let mut bytes = 0usize;
    for line in lines.into_iter().rev() {
        let length = line.len() + 1;
        if retained.len() == max_events || bytes.saturating_add(length) > max_bytes {
            break;
        }
        bytes += length;
        retained.push(line);
    }
    retained.reverse();
    retained
}

#[derive(Debug, Clone)]
pub struct ActivityPass {
    journal: ActivityJournal,
    pass_id: String,
    source: String,
    config_hash: String,
    started_at: i64,
}

impl ActivityPass {
    #[must_use]
    pub fn id(&self) -> &str {
        &self.pass_id
    }

    /// Append action events and the completed summary for this pass.
    ///
    /// # Errors
    ///
    /// Returns an error when any complete JSONL record cannot be appended.
    pub fn finish(
        &self,
        plan: &Plan,
        report: &ExecutionReport,
        completed_at: i64,
    ) -> Result<(), ActivityError> {
        let decisions = plan
            .decisions
            .iter()
            .filter(|decision| decision.action == Action::Remove)
            .map(|decision| {
                (
                    (decision.resource.kind, decision.resource.id.as_str()),
                    decision,
                )
            })
            .collect::<BTreeMap<_, _>>();
        let mut removed_count = 0u64;
        let mut skipped_count = 0u64;
        let mut failure_count = 0u64;
        let mut reclaimed_bytes = 0u64;

        for (index, outcome) in report.outcomes.iter().enumerate() {
            let decision = decisions.get(&(outcome.kind, outcome.id.as_str()));
            let freed_bytes = if outcome.status == TargetStatus::Removed {
                removed_count += 1;
                decision.and_then(|value| value.resource.size).unwrap_or(0)
            } else {
                if outcome.status == TargetStatus::Skipped {
                    skipped_count += 1;
                } else {
                    failure_count += 1;
                }
                0
            };
            reclaimed_bytes = reclaimed_bytes.saturating_add(freed_bytes);
            self.journal.append(&ActivityEvent {
                schema_version: SCHEMA_VERSION,
                pass_id: self.pass_id.clone(),
                source: self.source.clone(),
                sequence: u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1),
                timestamp: completed_at,
                config_hash: self.config_hash.clone(),
                data: EventData::Action {
                    action: outcome.status.to_string(),
                    resource_kind: outcome.kind.to_string(),
                    resource_id: outcome.id.clone(),
                    resource_name: outcome.name.clone(),
                    matched_rule: outcome.matched_rule.clone(),
                    age_seconds: decision.and_then(|value| value.age_seconds),
                    freed_bytes,
                    detail: outcome.detail.clone(),
                },
            })?;
        }

        let mut rule_match_counts = BTreeMap::<String, u64>::new();
        for decision in &plan.decisions {
            if let Some(rule) = &decision.matched_rule {
                *rule_match_counts.entry(rule.clone()).or_default() += 1;
            }
        }
        self.journal.append(&ActivityEvent {
            schema_version: SCHEMA_VERSION,
            pass_id: self.pass_id.clone(),
            source: self.source.clone(),
            sequence: u64::try_from(report.outcomes.len())
                .unwrap_or(u64::MAX)
                .saturating_add(1),
            timestamp: completed_at,
            config_hash: self.config_hash.clone(),
            data: EventData::PassSummary {
                completed_at,
                rule_match_counts,
                removed_count,
                skipped_count,
                failure_count,
                reclaimed_bytes,
            },
        })
    }

    #[must_use]
    pub fn started_at(&self) -> i64 {
        self.started_at
    }
}

#[must_use]
pub fn stable_config_hash(source: &str) -> String {
    let hash = source
        .as_bytes()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
        });
    format!("{hash:016x}")
}

fn next_pass_id() -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = NEXT_PASS_ID.fetch_add(1, Ordering::Relaxed);
    format!(
        "{timestamp:032x}-{:08x}-{sequence:016x}",
        std::process::id()
    )
}

fn read_events(paths: &[PathBuf]) -> Result<Vec<ActivityEvent>, ActivityError> {
    let mut events = Vec::new();
    let mut seen = BTreeSet::new();
    for path in paths {
        for (index, line) in read_lines(path)?.into_iter().enumerate() {
            let event: ActivityEvent =
                serde_json::from_slice(&line).map_err(|source| ActivityError::Decode {
                    path: path.clone(),
                    line: index + 1,
                    source,
                })?;
            if event.schema_version != SCHEMA_VERSION {
                return Err(ActivityError::Invalid(format!(
                    "unsupported activity schema_version {} in {} line {}; expected {SCHEMA_VERSION}",
                    event.schema_version,
                    path.display(),
                    index + 1
                )));
            }
            if seen.insert((event.pass_id.clone(), event.sequence)) {
                events.push(event);
            }
        }
    }
    Ok(events)
}

fn read_lines(path: &Path) -> Result<Vec<Vec<u8>>, ActivityError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(io_error(path, source)),
    };
    Ok(bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(<[u8]>::to_vec)
        .collect())
}

fn count_lines(path: &Path) -> Result<usize, ActivityError> {
    Ok(read_lines(path)?.len())
}

fn file_len(path: &Path) -> Result<u64, ActivityError> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(source) => Err(io_error(path, source)),
    }
}

fn write_atomic(
    paths: &StatePaths,
    destination: &Path,
    payload: &[u8],
) -> Result<(), ActivityError> {
    let nonce = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let temporary = paths
        .root()
        .join(format!(".activity.tmp-{}-{nonce}", std::process::id()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .map_err(|source| io_error(&temporary, source))?;
        set_private_file_permissions(&temporary)?;
        file.write_all(payload)
            .map_err(|source| io_error(&temporary, source))?;
        file.sync_all()
            .map_err(|source| io_error(&temporary, source))?;
        fs::rename(&temporary, destination).map_err(|source| io_error(destination, source))?;
        File::open(paths.root())
            .and_then(|directory| directory.sync_all())
            .map_err(|source| io_error(paths.root(), source))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn io_error(path: impl AsRef<Path>, source: io::Error) -> ActivityError {
    ActivityError::Io {
        path: path.as_ref().to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::{ExecutionReport, TargetOutcome};
    use crate::plan::{Decision, Disposition, InventoryItem, ResourceKind, ResourceState};
    use std::collections::BTreeMap;

    fn fixture() -> (Plan, ExecutionReport) {
        let item = InventoryItem {
            kind: ResourceKind::Volume,
            id: "v1".to_owned(),
            name: "volume-one".to_owned(),
            search_names: vec!["volume-one".to_owned()],
            parent_ids: Vec::new(),
            labels: BTreeMap::new(),
            state: ResourceState::Available,
            created_at: Some(1),
            state_since: None,
            size: Some(42),
            referenced: false,
            dangling: false,
            system: false,
        };
        let plan = Plan {
            decisions: vec![Decision {
                resource: item,
                disposition: Disposition::Owned,
                matched_rule: Some("old-volumes".to_owned()),
                action: Action::Remove,
                age_seconds: Some(9),
                reason: "fixture".to_owned(),
            }],
        };
        let report = ExecutionReport {
            outcomes: vec![TargetOutcome {
                kind: ResourceKind::Volume,
                id: "v1".to_owned(),
                name: "volume-one".to_owned(),
                matched_rule: "old-volumes".to_owned(),
                status: TargetStatus::Removed,
                detail: "removed".to_owned(),
            }],
        };
        (plan, report)
    }

    #[test]
    fn completed_pass_survives_a_new_journal_instance() {
        let root =
            std::env::temp_dir().join(format!("docker-maid-activity-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let paths = StatePaths::new(root.clone());
        let journal = ActivityJournal::new(paths.clone());
        let pass = journal.start_pass("clean", "abc", 10).unwrap();
        let (plan, report) = fixture();
        pass.finish(&plan, &report, 11).unwrap();

        let reloaded = ActivityJournal::new(paths)
            .last_completed_pass()
            .unwrap()
            .unwrap();
        assert_eq!(reloaded.removed_count, 1);
        assert_eq!(reloaded.reclaimed_bytes, 42);
        assert_eq!(reloaded.actions.len(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn incomplete_pass_is_not_reported_as_last_completed() {
        let root =
            std::env::temp_dir().join(format!("docker-maid-incomplete-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let journal = ActivityJournal::new(StatePaths::new(root.clone()));
        journal.start_pass("clean", "abc", 10).unwrap();
        assert!(journal.last_completed_pass().unwrap().is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn concurrent_passes_append_only_complete_json_records() {
        let root = std::env::temp_dir().join(format!(
            "docker-maid-activity-concurrent-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let journal = ActivityJournal::new(StatePaths::new(root.clone()));
        let (plan, report) = fixture();
        let threads = (0..12)
            .map(|index| {
                let journal = journal.clone();
                let plan = plan.clone();
                let report = report.clone();
                std::thread::spawn(move || {
                    let pass = journal
                        .start_pass("clean", &format!("config-{index}"), index)
                        .expect("start pass");
                    pass.finish(&plan, &report, index + 1).expect("finish pass");
                })
            })
            .collect::<Vec<_>>();
        for thread in threads {
            thread.join().expect("join activity writer");
        }

        let events = journal.snapshot().expect("read journal");
        assert_eq!(events.len(), 36);
        let completed = events
            .iter()
            .filter(|event| matches!(event.data, EventData::PassSummary { .. }))
            .count();
        assert_eq!(completed, 12);
        assert!(journal.last_completed_pass().unwrap().is_some());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rotation_retains_only_the_newest_records_within_both_caps() {
        let lines = vec![b"old".to_vec(), b"middle".to_vec(), b"new".to_vec()];
        assert_eq!(
            retain_newest_lines(lines.clone(), 2, 100),
            vec![b"middle".to_vec(), b"new".to_vec()]
        );
        assert_eq!(
            retain_newest_lines(lines, 10, 11),
            vec![b"middle".to_vec(), b"new".to_vec()]
        );
    }

    #[test]
    fn corrupt_history_blocks_a_new_pass_before_it_is_appended() {
        let root = std::env::temp_dir().join(format!(
            "docker-maid-activity-corrupt-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("activity.jsonl"), b"not-json\n").unwrap();
        let journal = ActivityJournal::new(StatePaths::new(root.clone()));

        let error = journal
            .start_pass("clean", "abc", 10)
            .expect_err("corrupt journal must block the pass");
        assert!(error.to_string().contains("invalid activity event"));
        assert_eq!(
            fs::read(root.join("activity.jsonl")).unwrap(),
            b"not-json\n"
        );
        fs::remove_dir_all(root).unwrap();
    }
}
