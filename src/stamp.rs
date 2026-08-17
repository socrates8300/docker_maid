//! The ownership stamp a caller applies when it creates a Docker resource.
//!
//! Docker fixes a resource's labels at creation. A container, image, volume,
//! or network cannot be relabelled afterwards, so `docker_maid` cannot walk up
//! to an existing resource and mark it as its own. Stamping is therefore an
//! emit, not an edit: this module builds the exact labels a caller passes to
//! `docker run`, `docker volume create`, a build, or the Docker API, and the
//! caller applies them at the moment the resource comes into existence.
//!
//! Every key comes from [`crate::labels`], so a stamped resource is evidence
//! `config survey` already understands. That is the whole point of the stamp:
//! the writer and the reader share one table, so an agent that stamps its work
//! becomes discoverable without any further configuration. A resource that was
//! never stamped is not lost either — adoption by rule stays the first route,
//! and stamping only makes a new resource obvious.

use crate::labels;
use std::collections::BTreeMap;
use std::fmt::{Display, Formatter, Result as FmtResult};

/// The value written alongside [`labels::managed_key`].
///
/// The key carries the meaning; the value only has to be stable and non-empty
/// so an exact `key=value` selector can name the family.
const MANAGED_VALUE: &str = "true";

/// The leaf appended to [`labels::agent_namespace`] to name the owning agent.
const OWNER_LEAF: &str = "owner";

/// Why an owner name cannot be stamped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StampError {
    /// The owner name was empty or only whitespace.
    BlankOwner,
    /// The owner name held a character the stamp will not write.
    UnsupportedOwner {
        /// The rejected name, echoed so the caller can see what it sent.
        value: String,
    },
}

impl Display for StampError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> FmtResult {
        match self {
            Self::BlankOwner => formatter
                .write_str("owner name is empty; give the agent a name or omit --owner entirely"),
            Self::UnsupportedOwner { value } => write!(
                formatter,
                "owner name {value:?} is not supported; use letters, digits, dot, dash, \
                 and underscore only"
            ),
        }
    }
}

impl std::error::Error for StampError {}

/// Return whether one character may appear in an owner name.
///
/// The set is deliberately narrow. The stamp is printed as a shell-ready flag
/// line, so a name holding a space, quote, or `$` would either split into two
/// arguments or be interpreted by the shell that expands it. Refusing is
/// safer than quoting, because a quoted answer would then be wrong for the
/// callers that read the JSON document instead.
fn is_supported_owner_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_')
}

/// The labels that mark a resource as created by `docker_maid` or its agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stamp {
    labels: BTreeMap<String, String>,
}

impl Stamp {
    /// Build the stamp, optionally naming the agent that owns the resource.
    ///
    /// # Errors
    ///
    /// Returns [`StampError`] when `owner` is blank or holds a character the
    /// stamp will not write.
    pub fn new(owner: Option<&str>) -> Result<Self, StampError> {
        let mut labels = BTreeMap::new();
        labels.insert(labels::managed_key().to_owned(), MANAGED_VALUE.to_owned());
        if let Some(owner) = owner {
            if owner.trim().is_empty() {
                return Err(StampError::BlankOwner);
            }
            if !owner.chars().all(is_supported_owner_char) {
                return Err(StampError::UnsupportedOwner {
                    value: owner.to_owned(),
                });
            }
            labels.insert(
                format!("{}{OWNER_LEAF}", labels::agent_namespace()),
                owner.to_owned(),
            );
        }
        Ok(Self { labels })
    }

    /// The labels to apply, in canonical key order.
    #[must_use]
    pub fn labels(&self) -> &BTreeMap<String, String> {
        &self.labels
    }

    /// The stamp as Docker command-line arguments.
    ///
    /// Every Docker create surface spells a label the same way, so one list
    /// serves `docker run`, `docker create`, `docker volume create`,
    /// `docker network create`, and `docker build`.
    #[must_use]
    pub fn docker_arguments(&self) -> Vec<String> {
        self.labels
            .iter()
            .flat_map(|(key, value)| ["--label".to_owned(), format!("{key}={value}")])
            .collect()
    }

    /// The stamp as one line a shell can expand into arguments.
    ///
    /// No argument ever needs quoting, because the keys are fixed and the
    /// owner charset is restricted, so `$(docker_maid stamp --docker-args)`
    /// splits into exactly the arguments [`Self::docker_arguments`] returns.
    #[must_use]
    pub fn docker_argument_line(&self) -> String {
        self.docker_arguments().join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::{Stamp, StampError, MANAGED_VALUE};
    use crate::labels;

    #[test]
    fn the_bare_stamp_marks_the_resource_as_managed() {
        let stamp = Stamp::new(None).expect("the bare stamp is always valid");
        assert_eq!(stamp.labels().len(), 1);
        assert_eq!(
            stamp
                .labels()
                .get(labels::managed_key())
                .map(String::as_str),
            Some(MANAGED_VALUE)
        );
    }

    #[test]
    fn naming_an_owner_adds_exactly_one_label() {
        let stamp = Stamp::new(Some("claude-code")).expect("a plain name is supported");
        assert_eq!(stamp.labels().len(), 2);
        let owner_key = format!("{}owner", labels::agent_namespace());
        assert_eq!(
            stamp.labels().get(&owner_key).map(String::as_str),
            Some("claude-code")
        );
    }

    #[test]
    fn every_stamped_key_is_a_key_the_survey_reads() {
        // This is the promise the stamp exists to keep. If it ever wrote a key
        // outside the vocabulary, the resource would carry a label nothing
        // reads, and the agent would believe it was discoverable when it was
        // not.
        for stamp in [
            Stamp::new(None).expect("bare stamp"),
            Stamp::new(Some("agent-7")).expect("named stamp"),
        ] {
            for key in stamp.labels().keys() {
                assert!(labels::is_known(key), "{key} is not ownership evidence");
                assert!(
                    !labels::is_compose_project(key),
                    "{key} would claim to be an operator-declared Compose stack"
                );
            }
        }
    }

    #[test]
    fn a_stamped_value_is_never_empty() {
        // The survey builds a `key=value` selector from the pair, so an empty
        // value would produce a family no resource can reliably rejoin.
        let stamp = Stamp::new(Some("a")).expect("a one-character name is supported");
        for (key, value) in stamp.labels() {
            assert!(!value.is_empty(), "{key} must carry a value");
        }
    }

    #[test]
    fn a_blank_owner_is_refused_rather_than_dropped() {
        // Silently ignoring `--owner ""` would hand back a stamp the caller did
        // not ask for, and the resource would be created without its owner.
        assert_eq!(Stamp::new(Some("")), Err(StampError::BlankOwner));
        assert_eq!(Stamp::new(Some("   ")), Err(StampError::BlankOwner));
    }

    #[test]
    fn an_owner_needing_a_shell_quote_is_refused() {
        for value in ["two words", "a$b", "a\"b", "a'b", "a;b", "a\nb", "a=b"] {
            assert_eq!(
                Stamp::new(Some(value)),
                Err(StampError::UnsupportedOwner {
                    value: value.to_owned()
                }),
                "{value:?} must not reach a shell"
            );
        }
    }

    #[test]
    fn an_owner_is_never_trimmed_into_something_else() {
        // Trimming would turn ` agent ` into a different family than the caller
        // asked for, and the mismatch would only show up as a rule that never
        // matches.
        assert!(Stamp::new(Some(" agent")).is_err());
        assert!(Stamp::new(Some("agent ")).is_err());
    }

    #[test]
    fn the_flag_line_splits_back_into_the_argument_list() {
        // The line exists for `$(docker_maid stamp --docker-args)`, so plain
        // whitespace splitting must reproduce the arguments exactly.
        let stamp = Stamp::new(Some("agent.7_b-c")).expect("the full charset is supported");
        let arguments = stamp.docker_arguments();
        let split = stamp
            .docker_argument_line()
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        assert_eq!(split, arguments);
        assert_eq!(arguments.len(), stamp.labels().len() * 2);
        for pair in arguments.chunks(2) {
            assert_eq!(pair[0], "--label");
            assert!(pair[1].contains('='));
        }
    }

    #[test]
    fn a_stamped_resource_becomes_an_adoption_candidate() {
        // The stamp, the vocabulary, and the survey must agree, or an agent
        // would stamp its work and `config survey` would still show nothing to
        // adopt. Build a resource carrying the stamp and check the survey
        // offers it.
        use crate::configurator::CandidateSelector;
        use crate::plan::{InventoryItem, ResourceKind, ResourceState};

        let stamp = Stamp::new(Some("agent-7")).expect("named stamp");
        let item = InventoryItem {
            kind: ResourceKind::Container,
            id: "stamped-id".to_owned(),
            name: "stamped".to_owned(),
            search_names: vec!["stamped".to_owned()],
            parent_ids: Vec::new(),
            labels: stamp.labels().clone(),
            mounts: Vec::new(),
            state: ResourceState::Stopped,
            created_at: Some(1),
            state_since: Some(1),
            size: Some(10),
            referenced: false,
            dangling: false,
            system: false,
        };
        let survey = crate::configurator::survey_inventory(std::slice::from_ref(&item));
        let mut offered = survey
            .candidates
            .iter()
            .filter_map(|candidate| match &candidate.selector {
                CandidateSelector::ExactLabel { key, value } => Some((key.clone(), value.clone())),
                _ => None,
            })
            .collect::<Vec<_>>();
        offered.sort_unstable();
        let expected = stamp
            .labels()
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<Vec<_>>();
        assert_eq!(
            offered, expected,
            "every stamped label must reach the survey as adoptable evidence"
        );
        // The candidate names the resource itself, not just the family.
        for candidate in &survey.candidates {
            assert!(
                candidate.resources.iter().any(|entry| entry.id == item.id),
                "candidate {} lost the stamped resource",
                candidate.id
            );
        }
    }
}
