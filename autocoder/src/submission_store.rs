//! Daemon-side execution-scoped submission store (a56).
//!
//! Parallels [`crate::outcome_store`]: holds the schema-validated payloads
//! that the per-execution MCP child relays via the `record_submission`
//! control-socket action, keyed by `(workspace_basename, change)`. The
//! role's daemon-side caller drains the store via `consume_submission`
//! AFTER the wrapped CLI exits.
//!
//! This change establishes the transport AND lifecycle. The concrete
//! per-role `submit_*` tools AND their payload schemas are registered by
//! the changes that consume them (the reviewer, contradiction-check, etc.);
//! a role with no registered schema accepts any payload. The seam those
//! changes plug into is [`SubmissionStore::register_schema`].
//!
//! Lifecycle: in-memory only, like the outcome store. A daemon restart
//! loses any in-flight entries; submission is synchronous (the tool call
//! happens milliseconds before the wrapped CLI exits AND `consume` runs
//! microseconds after).

use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// A per-role payload validator. Returns `Ok(())` when the payload
/// satisfies the role's schema, or `Err(reason)` with a correction-
/// suitable message the MCP relay surfaces to the agent.
pub type SubmissionValidator = Arc<dyn Fn(&Value) -> Result<(), String> + Send + Sync>;

/// Shared, mutex-protected store for in-flight submissions plus the
/// per-role schema registry. Cheap to clone: state lives behind `Arc`s.
#[derive(Clone, Default)]
pub struct SubmissionStore {
    inner: Arc<Mutex<HashMap<(String, String), Value>>>,
    schemas: Arc<Mutex<HashMap<String, SubmissionValidator>>>,
}

impl SubmissionStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a per-role payload validator. The concrete `submit_*`
    /// tools added by later changes register their schema here; this
    /// change registers none, so every role accepts any payload until
    /// then. Tests register a validator to exercise the rejection path.
    #[allow(dead_code)]
    pub fn register_schema(&self, role: impl Into<String>, validator: SubmissionValidator) {
        self.schemas
            .lock()
            .expect("submission schema registry mutex poisoned")
            .insert(role.into(), validator);
    }

    /// Validate `payload` against `role`'s registered schema (Ok when no
    /// schema is registered), then store it keyed by `(workspace_basename,
    /// change)`. On a validation failure NOTHING is stored AND the reason
    /// is returned for the relay to surface to the agent. Last-writer-wins
    /// on the key, matching the outcome store's retry semantics.
    pub fn record(
        &self,
        workspace_basename: String,
        change: String,
        role: &str,
        payload: Value,
    ) -> Result<(), String> {
        if let Some(validator) = self
            .schemas
            .lock()
            .expect("submission schema registry mutex poisoned")
            .get(role)
            .cloned()
        {
            validator(&payload)?;
        }
        self.inner
            .lock()
            .expect("submission store mutex poisoned")
            .insert((workspace_basename, change), payload);
        Ok(())
    }

    /// Atomically read AND remove the entry for `(workspace_basename,
    /// change)`. Subsequent calls for the same key return `None`.
    pub fn consume(&self, workspace_basename: &str, change: &str) -> Option<Value> {
        self.inner
            .lock()
            .expect("submission store mutex poisoned")
            .remove(&(workspace_basename.to_string(), change.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn record_then_consume_returns_payload_and_clears() {
        let store = SubmissionStore::new();
        let payload = json!({"verdict": "approve", "notes": "looks good"});
        store
            .record("my-repo".into(), "a30-foo".into(), "reviewer", payload.clone())
            .expect("no schema registered → accepts");
        let got = store.consume("my-repo", "a30-foo");
        assert_eq!(got, Some(payload));
        // Second consume drains to None.
        assert!(store.consume("my-repo", "a30-foo").is_none());
    }

    #[test]
    fn consume_unknown_key_returns_none() {
        let store = SubmissionStore::new();
        assert!(store.consume("my-repo", "never-recorded").is_none());
    }

    #[test]
    fn record_for_occupied_key_replaces_prior_entry() {
        let store = SubmissionStore::new();
        store
            .record("r".into(), "c".into(), "reviewer", json!({"v": 1}))
            .unwrap();
        store
            .record("r".into(), "c".into(), "reviewer", json!({"v": 2}))
            .unwrap();
        assert_eq!(store.consume("r", "c"), Some(json!({"v": 2})));
    }

    #[test]
    fn registered_schema_rejects_invalid_payload_without_storing() {
        let store = SubmissionStore::new();
        // A role whose validator requires a non-empty `verdict` field.
        store.register_schema(
            "reviewer",
            Arc::new(|p: &Value| {
                if p.get("verdict").and_then(|v| v.as_str()).is_some_and(|s| !s.is_empty()) {
                    Ok(())
                } else {
                    Err("verdict must be a non-empty string".to_string())
                }
            }),
        );
        let err = store
            .record("r".into(), "c".into(), "reviewer", json!({"verdict": ""}))
            .expect_err("schema-invalid payload must be rejected");
        assert!(err.contains("verdict"), "reason names the field: {err}");
        // Nothing stored.
        assert!(store.consume("r", "c").is_none());
        // A valid payload for the same role round-trips.
        store
            .record("r".into(), "c".into(), "reviewer", json!({"verdict": "approve"}))
            .expect("valid payload accepted");
        assert_eq!(store.consume("r", "c"), Some(json!({"verdict": "approve"})));
    }

    #[test]
    fn keys_do_not_collide_across_repos() {
        let store = SubmissionStore::new();
        store.record("a".into(), "c".into(), "reviewer", json!({"n": "a"})).unwrap();
        store.record("b".into(), "c".into(), "reviewer", json!({"n": "b"})).unwrap();
        assert_eq!(store.consume("a", "c"), Some(json!({"n": "a"})));
        assert_eq!(store.consume("b", "c"), Some(json!({"n": "b"})));
    }
}
