//! Canonical-spec RAG (retrieval-augmented context) for the
//! per-execution implementer (a21).
//!
//! ## Daemon plumbing
//!
//! The daemon's `cli/run.rs` startup registers a single
//! [`CanonicalRagRegistry`] AND exposes it via the
//! [`shared_registry`]/[`set_shared_registry`] process-global. The
//! polling loop's workspace-init AND post-archive hooks read this
//! global to register/rebuild stores. The control-socket handler
//! ([`crate::control_socket::handle_query_canonical_specs`]) reads it
//! to look up the right store for the calling workspace.
//!
//! Using a process-global is the pragmatic alternative to threading the
//! registry through every polling-loop entry point; the registry is
//! conceptually a singleton per-daemon, and `cli/run.rs` constructs it
//! once at startup AND publishes it before the polling tasks spawn.
//!
//! Design summary:
//! - Embeds every `openspec/specs/<capability>/spec.md` chunk at
//!   workspace init.
//! - Re-embeds affected capabilities after archives that touch
//!   canonical specs.
//! - In-memory only. Daemon restart re-embeds from scratch.
//! - Per-workspace store registry, keyed by sanitized basename, lives
//!   in the daemon (`CanonicalRagRegistry`); the control socket relays
//!   `query_canonical_specs` requests from per-execution MCP children
//!   to the right store.

pub mod chunking;
pub mod embedding;

use std::sync::OnceLock;

/// Process-global registry handle. Set once at daemon startup by
/// `cli/run.rs`; read by the polling loop's RAG hooks AND the control
/// socket's `query_canonical_specs` handler.
static SHARED_REGISTRY: OnceLock<CanonicalRagRegistry> = OnceLock::new();
/// Process-global snapshot of the active `CanonicalRagConfig`. Set
/// alongside [`SHARED_REGISTRY`] at startup; consulted by the polling
/// loop's RAG hooks to decide whether to build/rebuild stores.
static SHARED_CONFIG: OnceLock<crate::config::CanonicalRagConfig> = OnceLock::new();

/// Set the process-global registry + config. Called once by `cli/run.rs`
/// after parsing the config; idempotent only in the sense that
/// `OnceLock::set` returns `Err` on the second call (silently ignored).
pub fn set_shared(registry: CanonicalRagRegistry, config: crate::config::CanonicalRagConfig) {
    let _ = SHARED_REGISTRY.set(registry);
    let _ = SHARED_CONFIG.set(config);
}

/// Read the process-global registry handle, if set.
pub fn shared_registry() -> Option<&'static CanonicalRagRegistry> {
    SHARED_REGISTRY.get()
}

/// Read the process-global config snapshot, if set.
pub fn shared_config() -> Option<&'static crate::config::CanonicalRagConfig> {
    SHARED_CONFIG.get()
}

/// Workspace-init RAG hook. Called once per workspace, on the first
/// iteration after daemon startup. Builds + embeds the canonical
/// corpus and registers the store under the workspace's sanitized
/// basename. Fail-open: any error logs WARN and the store is omitted
/// from the registry (subsequent queries return empty Vec).
pub async fn workspace_init_hook(workspace: &std::path::Path) {
    let Some(registry) = shared_registry() else {
        return;
    };
    let Some(config) = shared_config() else {
        return;
    };
    if !config.is_active() {
        return;
    }
    let basename = sanitize_workspace_basename(workspace);
    if registry.contains(&basename).await {
        return; // Already initialized for this workspace.
    }
    match CanonicalRagStore::rebuild_for_workspace(workspace, config.clone()).await {
        Ok(store) => {
            let count = store.entry_count().await;
            registry
                .register(basename.clone(), std::sync::Arc::new(store))
                .await;
            tracing::info!(
                workspace_basename = %basename,
                "canonical RAG embedded {count} chunks for workspace `{basename}`"
            );
        }
        Err(e) => {
            tracing::warn!(
                workspace_basename = %basename,
                "canonical RAG workspace-init failed: {e:#}; \
                 query_canonical_specs will return empty Vec"
            );
        }
    }
}

/// Post-archive RAG hook. Given the workspace path AND the list of
/// canonical-spec capability slugs that the just-landed archive
/// touched, re-embed those capabilities in the store. Fail-open: any
/// error logs WARN and the prior embeds are retained.
pub async fn post_archive_hook(
    workspace: &std::path::Path,
    affected_capabilities: &[String],
) {
    if affected_capabilities.is_empty() {
        return;
    }
    let Some(registry) = shared_registry() else {
        return;
    };
    let Some(config) = shared_config() else {
        return;
    };
    if !config.is_active() || !config.reembed_on_archive {
        return;
    }
    let basename = sanitize_workspace_basename(workspace);
    let Some(store) = registry.get(&basename).await else {
        return;
    };
    match store
        .rebuild_capabilities(workspace, affected_capabilities)
        .await
    {
        Ok(()) => {
            tracing::info!(
                workspace_basename = %basename,
                "canonical RAG re-embedded {} capabilities after archive: {:?}",
                affected_capabilities.len(),
                affected_capabilities
            );
        }
        Err(e) => {
            tracing::warn!(
                workspace_basename = %basename,
                "canonical RAG post-archive re-embed failed: {e:#}; prior embeds retained"
            );
        }
    }
}

/// Inspect a git diff between two refs in `workspace` and return the
/// set of capability slugs whose `openspec/specs/<cap>/spec.md` was
/// touched. Used by the polling loop's post-archive hook to drive
/// [`post_archive_hook`].
pub fn capabilities_touched_between(
    workspace: &std::path::Path,
    range: &str,
) -> Vec<String> {
    let output = match std::process::Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(["diff", "--name-only", range, "--", "openspec/specs/"])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    let mut caps = std::collections::HashSet::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let parts: Vec<&str> = trimmed.split('/').collect();
        if parts.len() >= 4
            && parts[0] == "openspec"
            && parts[1] == "specs"
            && parts.last().map(|p| *p == "spec.md").unwrap_or(false)
        {
            caps.insert(parts[2].to_string());
        }
    }
    let mut out: Vec<String> = caps.into_iter().collect();
    out.sort();
    out
}

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::config::{CanonicalRagConfig, ChunkStrategy};

pub use chunking::{ChunkInput, chunk_canonical_spec};
pub use embedding::{EmbedClient, build_client};

/// A single retrieved chunk + its similarity score.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RagHit {
    pub capability: String,
    pub requirement_title: String,
    pub requirement_body: String,
    pub scenario_titles: Vec<String>,
    pub relevance_score: f32,
}

struct StoreEntry {
    input: ChunkInput,
    embedding: Vec<f32>,
}

/// In-memory canonical-spec store for one workspace.
pub struct CanonicalRagStore {
    #[allow(dead_code)]
    workspace_basename: String,
    provider: Arc<dyn EmbedClient>,
    config: CanonicalRagConfig,
    entries: RwLock<Vec<StoreEntry>>,
}

impl CanonicalRagStore {
    /// Build a store from a workspace by globbing
    /// `<workspace>/openspec/specs/<cap>/spec.md`, chunking each, and
    /// embedding every chunk via the configured provider. Fails open on
    /// any error — the daemon's workspace-init hook logs WARN and
    /// omits the store from the registry on failure.
    pub async fn rebuild_for_workspace(
        workspace: &Path,
        config: CanonicalRagConfig,
    ) -> Result<Self> {
        let workspace_basename = sanitize_workspace_basename(workspace);
        let provider = build_client(&config)?;
        let store = Self {
            workspace_basename,
            provider,
            config,
            entries: RwLock::new(Vec::new()),
        };
        let spec_paths = discover_canonical_specs(workspace)?;
        store.embed_paths(&spec_paths).await?;
        Ok(store)
    }

    /// Re-embed a named set of capabilities. Removes existing entries
    /// for each capability, re-chunks + re-embeds the matching spec
    /// file, and appends. Capabilities whose spec file is missing
    /// (e.g. removed by the archive) are dropped from the store.
    pub async fn rebuild_capabilities(
        &self,
        workspace: &Path,
        capabilities: &[String],
    ) -> Result<()> {
        let mut new_paths = Vec::new();
        let to_remove: std::collections::HashSet<&str> =
            capabilities.iter().map(|s| s.as_str()).collect();
        {
            let mut guard = self.entries.write().await;
            guard.retain(|e| !to_remove.contains(e.input.capability.as_str()));
        }
        for cap in capabilities {
            let path = workspace
                .join("openspec/specs")
                .join(cap)
                .join("spec.md");
            if path.is_file() {
                new_paths.push(path);
            }
        }
        self.embed_paths(&new_paths).await
    }

    /// Embed the query and return the top-k chunks by cosine
    /// similarity. `top_k` defaults to the config's `top_k`.
    pub async fn query(&self, query: &str, top_k: Option<usize>) -> Result<Vec<RagHit>> {
        let q_embed = self.provider.embed_one(query).await?;
        let top_k = top_k.unwrap_or(self.config.top_k);
        let guard = self.entries.read().await;
        let mut scored: Vec<(f32, &StoreEntry)> = guard
            .iter()
            .map(|e| (cosine_similarity(&q_embed, &e.embedding), e))
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(top_k);
        Ok(scored
            .into_iter()
            .map(|(score, entry)| RagHit {
                capability: entry.input.capability.clone(),
                requirement_title: entry.input.requirement_title.clone(),
                requirement_body: entry.input.text.clone(),
                scenario_titles: entry.input.scenario_titles.clone(),
                relevance_score: score,
            })
            .collect())
    }

    #[allow(dead_code)]
    pub fn workspace_basename(&self) -> &str {
        &self.workspace_basename
    }

    #[allow(dead_code)]
    pub fn config(&self) -> &CanonicalRagConfig {
        &self.config
    }

    async fn embed_paths(&self, paths: &[PathBuf]) -> Result<()> {
        let mut all_chunks: Vec<ChunkInput> = Vec::new();
        for path in paths {
            let chunks =
                chunk_canonical_spec(path, self.config.chunk_strategy.clone_or_default())?;
            all_chunks.extend(chunks);
        }
        if all_chunks.is_empty() {
            return Ok(());
        }
        let texts: Vec<String> = all_chunks.iter().map(|c| c.text.clone()).collect();
        let embeddings = self.provider.embed_batch(&texts).await?;
        if embeddings.len() != all_chunks.len() {
            return Err(anyhow::anyhow!(
                "provider returned {} embeddings for {} chunks",
                embeddings.len(),
                all_chunks.len()
            ));
        }
        let mut guard = self.entries.write().await;
        for (input, embedding) in all_chunks.into_iter().zip(embeddings) {
            guard.push(StoreEntry { input, embedding });
        }
        Ok(())
    }

    pub async fn entry_count(&self) -> usize {
        self.entries.read().await.len()
    }
}

/// `ChunkStrategy: Copy` is intentional; this helper exists so the call
/// site reads as "clone or default" rather than "deref then copy".
trait ChunkStrategyExt {
    fn clone_or_default(&self) -> ChunkStrategy;
}

impl ChunkStrategyExt for ChunkStrategy {
    fn clone_or_default(&self) -> ChunkStrategy {
        *self
    }
}

/// Per-workspace store registry. The daemon holds one of these; the
/// control socket's `query_canonical_specs` handler looks up the store
/// for the requesting workspace.
#[derive(Default, Clone)]
pub struct CanonicalRagRegistry {
    inner: Arc<RwLock<HashMap<String, Arc<CanonicalRagStore>>>>,
}

impl CanonicalRagRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn register(&self, basename: String, store: Arc<CanonicalRagStore>) {
        let mut guard = self.inner.write().await;
        guard.insert(basename, store);
    }

    #[allow(dead_code)]
    pub async fn remove(&self, basename: &str) {
        let mut guard = self.inner.write().await;
        guard.remove(basename);
    }

    pub async fn get(&self, basename: &str) -> Option<Arc<CanonicalRagStore>> {
        let guard = self.inner.read().await;
        guard.get(basename).cloned()
    }

    pub async fn contains(&self, basename: &str) -> bool {
        let guard = self.inner.read().await;
        guard.contains_key(basename)
    }

    #[allow(dead_code)]
    pub async fn len(&self) -> usize {
        let guard = self.inner.read().await;
        guard.len()
    }
}

/// Cosine similarity between two equal-length vectors. Returns `0.0`
/// for mismatched dimensions OR zero-norm vectors (a defensive
/// not-a-number guard — the provider should never return either, but
/// silent NaN propagation would break the top-k sort).
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    let denom = na.sqrt() * nb.sqrt();
    if denom == 0.0 { 0.0 } else { dot / denom }
}

/// Compute the sanitized workspace basename used as the registry key.
/// Matches the per-workspace path resolution: the file-name component
/// of the workspace path.
pub fn sanitize_workspace_basename(workspace: &Path) -> String {
    workspace
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| "unknown_workspace".to_string())
}

fn discover_canonical_specs(workspace: &Path) -> Result<Vec<PathBuf>> {
    let specs_root = workspace.join("openspec/specs");
    if !specs_root.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&specs_root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let spec_path = entry.path().join("spec.md");
        if spec_path.is_file() {
            out.push(spec_path);
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CanonicalRagConfig, ChunkStrategy, RagProvider};
    use async_trait::async_trait;
    use tempfile::TempDir;

    fn config_for_tests() -> CanonicalRagConfig {
        CanonicalRagConfig {
            enabled: true,
            provider: Some(RagProvider::Ollama),
            model: "nomic-embed-text".into(),
            api_base_url: "http://localhost:11434".into(),
            api_key_env: None,
            api_key: None,
            top_k: 10,
            chunk_strategy: ChunkStrategy::PerRequirement,
            reembed_on_archive: true,
        }
    }

    fn write_spec(workspace: &Path, capability: &str, body: &str) {
        let dir = workspace.join("openspec/specs").join(capability);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("spec.md"), body).unwrap();
    }

    /// Test client: maps a chunk's first non-empty word to a one-hot
    /// embedding so cosine similarity is predictable.
    struct WordMatchClient;

    #[async_trait]
    impl EmbedClient for WordMatchClient {
        async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|t| Self::embed_text(t)).collect())
        }
    }
    impl WordMatchClient {
        fn embed_text(text: &str) -> Vec<f32> {
            // Map the first non-heading word to one of three slots so
            // queries match deterministically.
            let lower = text.to_ascii_lowercase();
            let hit_audit = lower.contains("audit");
            let mut hit_review = lower.contains("review");
            let mut hit_other = !hit_audit && !hit_review;
            // Ensure exactly one is set:
            if hit_audit && hit_review {
                hit_review = false;
            }
            if !hit_audit && !hit_review {
                hit_other = true;
            }
            let v_audit = if hit_audit { 1.0 } else { 0.0 };
            let v_review = if hit_review { 1.0 } else { 0.0 };
            let v_other = if hit_other { 1.0 } else { 0.0 };
            vec![v_audit, v_review, v_other]
        }
    }

    async fn build_store(workspace: &Path) -> CanonicalRagStore {
        let provider: Arc<dyn EmbedClient> = Arc::new(WordMatchClient);
        let store = CanonicalRagStore {
            workspace_basename: sanitize_workspace_basename(workspace),
            provider: provider.clone(),
            config: config_for_tests(),
            entries: RwLock::new(Vec::new()),
        };
        let paths = discover_canonical_specs(workspace).unwrap();
        store.embed_paths(&paths).await.unwrap();
        store
    }

    #[tokio::test]
    async fn store_query_ranks_audit_chunk_first() {
        let tmp = TempDir::new().unwrap();
        write_spec(
            tmp.path(),
            "audits",
            "### Requirement: Audit cadence\nSHALL run audits on a schedule.\n",
        );
        write_spec(
            tmp.path(),
            "reviewer",
            "### Requirement: Review block verdict\nSHALL block when policy fails.\n",
        );
        write_spec(
            tmp.path(),
            "other-cap",
            "### Requirement: Other thing\nSHALL do something else.\n",
        );
        let store = build_store(tmp.path()).await;
        let hits = store.query("audit framework cadence", Some(3)).await.unwrap();
        assert_eq!(hits.len(), 3);
        // The audit chunk wins because cosine sim hits the audit slot.
        assert_eq!(hits[0].capability, "audits");
    }

    #[tokio::test]
    async fn rebuild_single_capability_leaves_others_alone() {
        let tmp = TempDir::new().unwrap();
        write_spec(
            tmp.path(),
            "audits",
            "### Requirement: Audit cadence\nSHALL run audits.\n",
        );
        write_spec(
            tmp.path(),
            "reviewer",
            "### Requirement: Review block verdict\nSHALL block.\n",
        );
        let store = build_store(tmp.path()).await;
        assert_eq!(store.entry_count().await, 2);

        // Mutate the audits spec and rebuild just that capability.
        write_spec(
            tmp.path(),
            "audits",
            "### Requirement: Audit cadence\nSHALL run audits.\n\n### Requirement: New audit type\nSHALL register new type.\n",
        );
        store
            .rebuild_capabilities(tmp.path(), &["audits".to_string()])
            .await
            .unwrap();
        let entries = store.entries.read().await;
        let audit_entries: Vec<_> = entries
            .iter()
            .filter(|e| e.input.capability == "audits")
            .collect();
        let reviewer_entries: Vec<_> = entries
            .iter()
            .filter(|e| e.input.capability == "reviewer")
            .collect();
        assert_eq!(audit_entries.len(), 2);
        assert_eq!(reviewer_entries.len(), 1);
    }

    #[tokio::test]
    async fn empty_workspace_yields_empty_store() {
        let tmp = TempDir::new().unwrap();
        // No openspec/specs/ at all.
        let store = build_store(tmp.path()).await;
        assert_eq!(store.entry_count().await, 0);
        let hits = store.query("anything", Some(5)).await.unwrap();
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn registry_routes_per_workspace() {
        let registry = CanonicalRagRegistry::new();
        let tmp_a = TempDir::new().unwrap();
        let tmp_b = TempDir::new().unwrap();
        // Build two stores with one chunk each (distinct content).
        write_spec(
            tmp_a.path(),
            "audits",
            "### Requirement: Audit cadence\nSHALL.\n",
        );
        write_spec(
            tmp_b.path(),
            "reviewer",
            "### Requirement: Review verdict\nSHALL.\n",
        );
        let a = Arc::new(build_store(tmp_a.path()).await);
        let b = Arc::new(build_store(tmp_b.path()).await);
        let basename_a = sanitize_workspace_basename(tmp_a.path());
        let basename_b = sanitize_workspace_basename(tmp_b.path());
        registry.register(basename_a.clone(), a.clone()).await;
        registry.register(basename_b.clone(), b.clone()).await;
        assert!(registry.contains(&basename_a).await);
        assert!(registry.contains(&basename_b).await);
        let got_a = registry.get(&basename_a).await.unwrap();
        assert_eq!(got_a.entry_count().await, 1);
        let got_b = registry.get(&basename_b).await.unwrap();
        assert_eq!(got_b.entry_count().await, 1);
        let nope = registry.get("never-registered").await;
        assert!(nope.is_none());
    }

    #[test]
    fn cosine_similarity_handles_edge_cases() {
        assert_eq!(cosine_similarity(&[], &[]), 0.0);
        assert_eq!(cosine_similarity(&[1.0], &[1.0, 0.0]), 0.0);
        let a = [1.0f32, 0.0];
        let b = [1.0f32, 0.0];
        assert!((cosine_similarity(&a, &b) - 1.0).abs() < 1e-6);
        let c = [0.0f32, 1.0];
        assert!(cosine_similarity(&a, &c).abs() < 1e-6);
    }
}
