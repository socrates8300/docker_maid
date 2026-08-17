//! The canonical Docker label vocabulary.
//!
//! Every part of `docker_maid` that asks "is this label one we understand?"
//! asks here. The ownership survey, the family lookup behind the TUI protect
//! action, and the `labels` command all read [`VOCABULARY`], so an operator who
//! runs `docker_maid labels` sees exactly the keys the policy engine acts on.
//! A key that is not in this table is not ownership evidence, however
//! convincing it looks.
//!
//! The table is deliberately small. Adding an entry widens what the survey will
//! offer to adopt, so it is a policy change and belongs in review, not in a
//! caller's local match arm.

use std::fmt::{Display, Formatter, Result as FmtResult};

/// How an entry matches a Docker label key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Match {
    /// The label key must equal the entry's key exactly.
    Exact,
    /// The label key must begin with the entry's key, which is a namespace.
    Prefix,
}

impl Display for Match {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> FmtResult {
        let text = match self {
            Self::Exact => "exact",
            Self::Prefix => "prefix",
        };
        formatter.write_str(text)
    }
}

/// What the policy engine does with a matching label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The key names an ownership family the survey can offer to adopt.
    ///
    /// The key and its value together identify the family, so two resources
    /// agree only when both halves match.
    Family,
}

impl Display for Role {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> FmtResult {
        let text = match self {
            Self::Family => "family",
        };
        formatter.write_str(text)
    }
}

/// One entry in the canonical vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LabelKey {
    /// The exact key, or the namespace prefix including its trailing dot.
    pub key: &'static str,
    /// Whether `key` is compared by equality or as a prefix.
    pub matching: Match,
    /// What the policy engine does with a match.
    pub role: Role,
    /// Who writes the label in practice.
    pub writer: &'static str,
    /// Why the key is ownership evidence, in one line.
    pub purpose: &'static str,
}

impl LabelKey {
    /// Return whether one Docker label key matches this entry.
    #[must_use]
    pub fn matches(&self, key: &str) -> bool {
        match self.matching {
            Match::Exact => key == self.key,
            Match::Prefix => key.starts_with(self.key),
        }
    }
}

/// Every label key `docker_maid` treats as ownership evidence.
///
/// Ordered most specific first, so a caller that wants the single best entry
/// for a key can take the first match. `com.docker.compose.project` leads
/// because a Compose project is the strongest ownership statement a host
/// offers: the operator declared the whole stack in one file.
pub const VOCABULARY: &[LabelKey] = &[
    LabelKey {
        key: "com.docker.compose.project",
        matching: Match::Exact,
        role: Role::Family,
        writer: "Docker Compose",
        purpose: "Groups every resource Compose created for one project, so the \
                  whole stack is adopted or left alone together",
    },
    LabelKey {
        key: "dev.docker-maid.managed",
        matching: Match::Exact,
        role: Role::Family,
        writer: "docker_maid",
        purpose: "Marks a resource this tool stamped, so a family it created is \
                  recognised on a later pass",
    },
    LabelKey {
        key: "ai-agent.",
        matching: Match::Prefix,
        role: Role::Family,
        writer: "coding agents",
        purpose: "Namespace an agent uses to claim what it created, for example \
                  ai-agent.owner to name the agent",
    },
    LabelKey {
        key: "devcontainer.",
        matching: Match::Prefix,
        role: Role::Family,
        writer: "Dev Containers",
        purpose: "Namespace the Dev Containers tooling writes, for example \
                  devcontainer.local_folder to name the workspace",
    },
];

/// Return the vocabulary entry a Docker label key matches, if any.
///
/// The first match in [`VOCABULARY`] order wins, so the answer never depends on
/// iteration luck when a resource carries more than one known key.
#[must_use]
pub fn lookup(key: &str) -> Option<&'static LabelKey> {
    VOCABULARY.iter().find(|entry| entry.matches(key))
}

/// Return whether a Docker label key is ownership evidence.
#[must_use]
pub fn is_known(key: &str) -> bool {
    lookup(key).is_some()
}

/// The Compose project key, which several call sites name directly.
///
/// It is read out of [`VOCABULARY`] rather than repeated as a literal, so the
/// table stays the only place the string appears.
#[must_use]
pub fn compose_project_key() -> &'static str {
    VOCABULARY[0].key
}

/// Return whether a key is the Compose project key.
#[must_use]
pub fn is_compose_project(key: &str) -> bool {
    key == compose_project_key()
}

/// The key `docker_maid` writes when it marks a resource it created.
///
/// This is the writing side of the same table the survey reads, so anything
/// stamped with it is evidence the survey already understands.
#[must_use]
pub fn managed_key() -> &'static str {
    VOCABULARY[1].key
}

/// The namespace a coding agent uses to claim what it created.
///
/// The entry is a prefix, so this string is not a usable label on its own. A
/// caller appends a leaf, such as `owner`, to name one fact about the family.
#[must_use]
pub fn agent_namespace() -> &'static str {
    VOCABULARY[2].key
}

#[cfg(test)]
mod tests {
    use super::{
        agent_namespace, compose_project_key, is_compose_project, is_known, lookup, managed_key,
        Match, Role, VOCABULARY,
    };

    #[test]
    fn every_entry_is_usable_and_distinct() {
        // A blank key would match every label under prefix rules and silently
        // adopt the whole host, so the table must never carry one.
        for entry in VOCABULARY {
            assert!(!entry.key.is_empty(), "a vocabulary key must not be blank");
            assert!(
                !entry.purpose.is_empty(),
                "{} must explain why it is evidence",
                entry.key
            );
            assert!(
                !entry.writer.is_empty(),
                "{} must name who writes it",
                entry.key
            );
            if entry.matching == Match::Prefix {
                assert!(
                    entry.key.ends_with('.'),
                    "prefix entry {} must end at a namespace boundary",
                    entry.key
                );
            }
        }
        let mut keys = VOCABULARY.iter().map(|entry| entry.key).collect::<Vec<_>>();
        keys.sort_unstable();
        let count = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), count, "vocabulary keys must be distinct");
    }

    #[test]
    fn exact_entries_do_not_match_a_longer_key() {
        // `dev.docker-maid.managed` is exact, so a neighbouring key in the same
        // namespace is not evidence and must not be adopted by accident.
        assert!(is_known("dev.docker-maid.managed"));
        assert!(!is_known("dev.docker-maid.managed.extra"));
        assert!(!is_known("dev.docker-maid"));
        assert!(is_known(compose_project_key()));
        assert!(!is_known("com.docker.compose.projectx"));
        assert!(!is_known("com.docker.compose.service"));
    }

    #[test]
    fn prefix_entries_match_inside_their_namespace_only() {
        assert!(is_known("ai-agent.owner"));
        assert!(is_known("ai-agent.session"));
        assert!(is_known("devcontainer.local_folder"));
        // A near miss keeps the namespace dot from being optional, so
        // `ai-agentx.owner` is a different vendor and not ours to delete.
        assert!(!is_known("ai-agentx.owner"));
        assert!(!is_known("ai-agent"));
        assert!(!is_known("my-devcontainer.local_folder"));
    }

    #[test]
    fn an_unknown_key_is_never_evidence() {
        for key in [
            "",
            "maintainer",
            "org.opencontainers.image.source",
            "com.example.owner",
        ] {
            assert!(!is_known(key), "{key} must not be ownership evidence");
        }
    }

    #[test]
    fn lookup_returns_the_most_specific_entry_first() {
        // Compose leads the table, so a Compose key resolves to Compose even
        // though later prefix entries exist.
        let entry = lookup(compose_project_key()).expect("compose key is known");
        assert_eq!(entry.key, compose_project_key());
        assert_eq!(entry.matching, Match::Exact);
        assert_eq!(entry.role, Role::Family);
        assert!(is_compose_project(compose_project_key()));
        assert!(!is_compose_project("ai-agent.owner"));
    }

    #[test]
    fn the_compose_key_accessor_agrees_with_the_table() {
        // The accessor exists so no call site repeats the literal. If the table
        // is ever reordered, this catches the accessor pointing at the wrong row.
        assert_eq!(compose_project_key(), "com.docker.compose.project");
        assert!(VOCABULARY
            .iter()
            .any(|entry| entry.key == compose_project_key() && entry.matching == Match::Exact));
    }

    #[test]
    fn the_writing_accessors_agree_with_the_table() {
        // These two name the rows `stamp` writes. Reordering the table would
        // silently repoint them at another vendor's namespace, so pin them.
        assert_eq!(managed_key(), "dev.docker-maid.managed");
        assert_eq!(agent_namespace(), "ai-agent.");
        let managed = lookup(managed_key()).expect("the managed key is in the table");
        assert_eq!(managed.matching, Match::Exact);
        let agent = lookup(&format!("{}owner", agent_namespace()))
            .expect("a leaf in the agent namespace is in the table");
        assert_eq!(agent.matching, Match::Prefix);
        assert_eq!(agent.key, agent_namespace());
        // Neither row is Compose, so a stamped resource is an agent family and
        // never masquerades as an operator-declared Compose stack.
        assert!(!is_compose_project(managed_key()));
        assert!(!is_compose_project(agent_namespace()));
    }
}
