//! Daemon-side outcome store for a27a0. Holds outcomes recorded by the
//! per-execution MCP child's `record_outcome` action, keyed by
//! `(workspace_basename, change)`. The executor's classifier drains
//! the store via the `consume_outcome` action AFTER subprocess exit.
//!
//! Lifecycle: in-memory only. Created at daemon startup, populated by
//! `record_outcome` calls relayed from the MCP child, drained by
//! `consume_outcome` calls from `classify_outcome`. A daemon restart
//! loses any in-flight entries; this is acceptable because outcome
//! reporting is synchronous (the tool call happens milliseconds before
//! the wrapped CLI exits AND `consume_outcome` runs microseconds after).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Per-task entry inside a `SpecNeedsRevision` variant. Shape matches
/// the executor's `UnimplementableTask` AND the MCP tool's input
/// schema; kept as a dedicated type so the daemon-side outcome layer
/// does not depend on the executor module.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedUnimplementableTask {
    pub task_id: String,
    pub task_text: String,
    pub reason: String,
}

/// The variant-tagged outcome payload the MCP layer relays to the
/// daemon. Mirrors the documented `record_outcome` / `consume_outcome`
/// wire format.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RecordedOutcome {
    Success {
        #[serde(default)]
        final_answer: Option<String>,
    },
    SpecNeedsRevision {
        unimplementable_tasks: Vec<RecordedUnimplementableTask>,
        revision_suggestion: String,
    },
    IterationRequest {
        completed_tasks: Vec<String>,
        remaining_tasks: Vec<String>,
        reason: String,
    },
}

/// Shared, mutex-protected map for in-flight outcomes. Cheap to clone:
/// the underlying state lives behind an `Arc`.
#[derive(Default, Clone)]
pub struct OutcomeStore {
    inner: Arc<Mutex<HashMap<(String, String), RecordedOutcome>>>,
}

impl OutcomeStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Last-writer-wins. A second `record` for the same key replaces
    /// the prior entry (per the spec's documented semantics for retry).
    pub fn record(
        &self,
        workspace_basename: String,
        change: String,
        outcome: RecordedOutcome,
    ) {
        let mut guard = self.inner.lock().unwrap();
        guard.insert((workspace_basename, change), outcome);
    }

    /// Atomically reads AND removes the entry for `(workspace_basename,
    /// change)`. Subsequent `consume` calls for the same key return
    /// `None`.
    pub fn consume(
        &self,
        workspace_basename: &str,
        change: &str,
    ) -> Option<RecordedOutcome> {
        let mut guard = self.inner.lock().unwrap();
        guard.remove(&(workspace_basename.to_string(), change.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_success() -> RecordedOutcome {
        RecordedOutcome::Success {
            final_answer: Some("done".to_string()),
        }
    }

    fn sample_revision() -> RecordedOutcome {
        RecordedOutcome::SpecNeedsRevision {
            unimplementable_tasks: vec![RecordedUnimplementableTask {
                task_id: "6.4".to_string(),
                task_text: "Manual: SSH...".to_string(),
                reason: "no SSH access".to_string(),
            }],
            revision_suggestion: "Replace 6.4 with a mocked unit test".to_string(),
        }
    }

    #[test]
    fn record_then_consume_returns_payload_and_clears() {
        let store = OutcomeStore::new();
        store.record("my-repo".into(), "a30-foo".into(), sample_success());
        let got = store.consume("my-repo", "a30-foo");
        assert_eq!(got, Some(sample_success()));
        // Second consume returns None — the prior call drained the store.
        let again = store.consume("my-repo", "a30-foo");
        assert!(again.is_none());
    }

    #[test]
    fn record_for_occupied_key_replaces_prior_entry() {
        let store = OutcomeStore::new();
        store.record("my-repo".into(), "a30-foo".into(), sample_success());
        store.record("my-repo".into(), "a30-foo".into(), sample_revision());
        let got = store.consume("my-repo", "a30-foo").unwrap();
        assert_eq!(got, sample_revision());
    }

    #[test]
    fn consume_unknown_key_returns_none() {
        let store = OutcomeStore::new();
        assert!(store.consume("my-repo", "never-recorded").is_none());
    }

    #[test]
    fn keys_do_not_collide_across_repos() {
        let store = OutcomeStore::new();
        store.record("repo-a".into(), "a30-foo".into(), sample_success());
        store.record("repo-b".into(), "a30-foo".into(), sample_revision());
        assert_eq!(store.consume("repo-a", "a30-foo"), Some(sample_success()));
        assert_eq!(store.consume("repo-b", "a30-foo"), Some(sample_revision()));
    }

    #[test]
    fn success_variant_round_trips_serde_with_tag() {
        let v = sample_success();
        let json = serde_json::to_value(&v).unwrap();
        assert_eq!(json["type"], "success");
        assert_eq!(json["final_answer"], "done");
        let back: RecordedOutcome = serde_json::from_value(json).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn spec_needs_revision_variant_round_trips_serde_with_tag() {
        let v = sample_revision();
        let json = serde_json::to_value(&v).unwrap();
        assert_eq!(json["type"], "spec_needs_revision");
        assert_eq!(json["revision_suggestion"], "Replace 6.4 with a mocked unit test");
        assert_eq!(json["unimplementable_tasks"][0]["task_id"], "6.4");
        let back: RecordedOutcome = serde_json::from_value(json).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn success_variant_with_absent_final_answer_deserializes_as_none() {
        let json = serde_json::json!({ "type": "success" });
        let back: RecordedOutcome = serde_json::from_value(json).unwrap();
        assert_eq!(back, RecordedOutcome::Success { final_answer: None });
    }

    fn sample_iteration_request() -> RecordedOutcome {
        RecordedOutcome::IterationRequest {
            completed_tasks: vec!["1".to_string(), "2".to_string()],
            remaining_tasks: vec!["3".to_string()],
            reason: "task 3 needs a refactor I want to plan more carefully".to_string(),
        }
    }

    #[test]
    fn iteration_request_variant_round_trips_serde_with_tag() {
        let v = sample_iteration_request();
        let json = serde_json::to_value(&v).unwrap();
        assert_eq!(json["type"], "iteration_request");
        assert_eq!(json["completed_tasks"][0], "1");
        assert_eq!(json["completed_tasks"][1], "2");
        assert_eq!(json["remaining_tasks"][0], "3");
        assert_eq!(
            json["reason"],
            "task 3 needs a refactor I want to plan more carefully"
        );
        let back: RecordedOutcome = serde_json::from_value(json).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn iteration_request_record_then_consume_round_trip_byte_for_byte() {
        let store = OutcomeStore::new();
        let v = sample_iteration_request();
        store.record("my-repo".into(), "a30-foo".into(), v.clone());
        let got = store.consume("my-repo", "a30-foo").unwrap();
        // Byte-for-byte equivalence: serde_json::to_value of both
        // produces identical JSON.
        let v_json = serde_json::to_value(&v).unwrap();
        let got_json = serde_json::to_value(&got).unwrap();
        assert_eq!(v_json, got_json);
        assert_eq!(got, v);
    }
}
