//! Architecture-brightline audit. Pure-code metrics; no LLM invocation,
//! no network. `requires_head_change = true`, `WritePolicy::None`.
//!
//! Surfaces structural metrics that frequently signal drift in a code
//! base: oversize source files and identical function signatures across
//! files. The set is intentionally small in the foundation change;
//! future audits can plug in more checks via additional `Audit`
//! implementations or by extending this module's metric list.
//!
//! The `🔍 created proposal` chatops notification documented in
//! `a02-audit-proposal-created-notification` does NOT fire from this
//! audit — brightline produces pure-data findings and does not
//! generate an LLM proposal under `openspec/changes/<slug>/`, so
//! there is no proposal-creation event to signal.

use anyhow::Result;
use async_trait::async_trait;
use regex::Regex;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use super::{
    Audit, AuditContext, AuditOutcome, Finding, Severity, WritePolicy, workspace_is_valid,
    workspace_unavailable_outcome,
};
use crate::config::AuditSettings;

pub mod ignore;

/// Subject prefix used for stale `.brightline-ignore` entries. The
/// chatops top-line formatter (`format_audit_top_line`) counts findings
/// whose subject starts with this prefix to render the trailing
/// `; <K> stale ignore entries to clean up` clause.
pub const STALE_IGNORE_SUBJECT_PREFIX: &str = "stale ignore entry: ";

/// Subject prefix for whole-file size findings (`"file <path> is <N> …"`).
/// The chatops top-line formatter discriminates finding kinds by these
/// prefixes when rendering the per-metric counts.
pub const FILE_SIZE_SUBJECT_PREFIX: &str = "file ";
/// Subject prefix for function size findings
/// (`"function <name> in <path> is <N> …"`).
pub const FUNCTION_SIZE_SUBJECT_PREFIX: &str = "function ";
/// Subject prefix for duplicate-signature findings.
pub const DUPLICATE_SIGNATURE_SUBJECT_PREFIX: &str = "duplicate signature ";
/// Subject prefix for duplicate-body findings.
pub const DUPLICATE_BODY_SUBJECT_PREFIX: &str = "duplicate body ";

pub(crate) const DEFAULT_FILE_LINES_THRESHOLD: u64 = 800;
const SETTINGS_KEY_FILE_LINES: &str = "file_lines_threshold";

pub(crate) const DEFAULT_FUNCTION_LINES_THRESHOLD: u64 = 200;
const SETTINGS_KEY_FUNCTION_LINES: &str = "function_lines_threshold";

/// Directories to skip entirely. Vendored / generated trees would
/// dominate the findings otherwise.
const EXCLUDED_DIR_COMPONENTS: &[&str] = &[
    "node_modules",
    "target",
    "vendor",
    "dist",
    "build",
    "out",
    ".git",
    ".cache",
    ".venv",
    "venv",
    "__pycache__",
];

/// Extensions the scanner examines. Anything else is ignored; binary
/// formats and asset blobs would just clutter the report.
const SCANNED_EXTENSIONS: &[&str] = &[
    "rs", "py", "cs", "ts", "tsx", "js", "jsx", "go", "java", "kt", "swift",
];

#[derive(Clone)]
pub struct ArchitectureBrightlineAudit {
    file_lines_threshold: u64,
    function_lines_threshold: u64,
}

impl ArchitectureBrightlineAudit {
    /// Build the audit, pulling thresholds out of `audit_settings`
    /// (under the audit's slug key in `settings.extra`). Falls back to
    /// the compile-time defaults when a knob is unset.
    pub fn new(audit_settings: &HashMap<String, AuditSettings>) -> Self {
        let settings = audit_settings.get(Self::TYPE);
        let file_lines_threshold = settings
            .and_then(|s| s.extra.get(SETTINGS_KEY_FILE_LINES))
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_FILE_LINES_THRESHOLD);
        let function_lines_threshold = settings
            .and_then(|s| s.extra.get(SETTINGS_KEY_FUNCTION_LINES))
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_FUNCTION_LINES_THRESHOLD);
        Self {
            file_lines_threshold,
            function_lines_threshold,
        }
    }

    pub const TYPE: &'static str = "architecture_brightline";

    /// Run both metrics against `workspace`. Returned findings sort by
    /// severity (high → medium → low) then by subject for stability
    /// across invocations.
    pub fn analyze(&self, workspace: &Path) -> Result<Vec<Finding>> {
        let scanned = collect_source_files(workspace)?;
        let ignore_entries = ignore::load(workspace);
        let mut findings = Vec::new();
        for path in &scanned {
            if let Some(f) = check_file_size(path, workspace, self.file_lines_threshold) {
                findings.push(f);
            }
            findings.extend(check_function_sizes(
                path,
                workspace,
                self.function_lines_threshold,
            ));
        }
        findings.extend(check_signature_duplicates(&scanned, workspace, &ignore_entries));
        findings.extend(check_body_duplicates(&scanned, workspace, &ignore_entries));
        // Validate every loaded ignore entry against the current
        // workspace state. Stale entries surface as findings with a
        // dedicated subject prefix that the chatops top-line formatter
        // counts separately to render the trailing
        // `; <K> stale ignore entries to clean up` clause. The audit
        // does NOT modify the on-disk file (WritePolicy::None is
        // unchanged); cleanup is operator-driven.
        let stale = ignore::collect_stale(workspace, &ignore_entries);
        for entry in stale {
            findings.push(stale_finding(&entry));
        }
        // Deterministic ordering: severity (high first), then subject.
        findings.sort_by(|a, b| {
            severity_rank(b.severity)
                .cmp(&severity_rank(a.severity))
                .then(a.subject.cmp(&b.subject))
        });
        Ok(findings)
    }
}

fn stale_finding(entry: &ignore::IgnoreEntry) -> Finding {
    let file = entry.file.to_string_lossy();
    let subject = format!(
        "{prefix}{file} :: {function} — {reason}",
        prefix = STALE_IGNORE_SUBJECT_PREFIX,
        file = file,
        function = entry.function,
        reason = entry.reason,
    );
    let body = format!(
        "file: {file}\nfunction: {function}\nreason: {reason}",
        file = file,
        function = entry.function,
        reason = entry.reason,
    );
    Finding {
        severity: Severity::Low,
        subject,
        body,
        anchor: None,
    }
}

#[async_trait]
impl Audit for ArchitectureBrightlineAudit {
    fn audit_type(&self) -> &'static str {
        Self::TYPE
    }

    fn description(&self) -> &'static str {
        "file-size / module-size guidelines (architecture brightline)"
    }

    fn requires_head_change(&self) -> bool {
        true
    }

    fn write_policy(&self) -> WritePolicy {
        WritePolicy::None
    }

    async fn run(&self, ctx: &mut AuditContext<'_>) -> Result<AuditOutcome> {
        // Workspace-validity gate (see `audits-require-valid-workspace`).
        // Brightline doesn't write proposals, but running it against a
        // missing workspace produces garbage zero-file counts and is
        // gated uniformly with every other audit type so the framework
        // contract holds.
        if !workspace_is_valid(ctx.workspace) {
            return Ok(workspace_unavailable_outcome(
                Self::TYPE,
                ctx.workspace,
                &ctx.repo.url,
            ));
        }
        let findings = self.analyze(ctx.workspace)?;
        let _ = ctx.log_writer.write_section(
            "brightline_summary",
            &format!(
                "file_lines_threshold: {}\nfunction_lines_threshold: {}\nfindings_count: {}",
                self.file_lines_threshold,
                self.function_lines_threshold,
                findings.len()
            ),
        );
        // The architecture_brightline audit is pure-data file-line-counting
        // — it does NOT invoke an LLM and does NOT write proposals. The
        // post-write `openspec validate --strict` retry machinery in
        // `audits::validate_with_retry` does not apply here. (See change
        // `a01-audit-proposal-self-validation`.)
        Ok(AuditOutcome::reported(findings))
    }
}

fn severity_rank(s: Severity) -> u8 {
    match s {
        Severity::High => 3,
        Severity::Medium => 2,
        Severity::Low => 1,
    }
}

fn collect_source_files(workspace: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    walk(workspace, workspace, &mut out)?;
    out.sort();
    Ok(out)
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return Ok(()),
    };
    for entry in read {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if EXCLUDED_DIR_COMPONENTS.iter().any(|d| *d == name) {
            continue;
        }
        let ft = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if ft.is_dir() {
            walk(root, &path, out)?;
        } else if ft.is_file() {
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                if SCANNED_EXTENSIONS.contains(&ext) {
                    out.push(path);
                }
            }
        }
    }
    Ok(())
}

fn relative_path(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| path.to_string_lossy().to_string())
}

/// Map a measured line count to a graduated severity by its ratio to
/// `threshold`. `Low` below `1.5×`, `Medium` in `[1.5×, 2.5×)`, `High`
/// at or above `2.5×`. Integer-ratio arithmetic (`threshold * 3 / 2`,
/// `threshold * 5 / 2`) keeps the band edges exact and float-free.
pub(crate) fn severity_for_ratio(n: u64, threshold: u64) -> Severity {
    if n < threshold.saturating_mul(3) / 2 {
        Severity::Low
    } else if n < threshold.saturating_mul(5) / 2 {
        Severity::Medium
    } else {
        Severity::High
    }
}

fn check_file_size(path: &Path, root: &Path, threshold: u64) -> Option<Finding> {
    let contents = std::fs::read_to_string(path).ok()?;
    let n = contents.lines().count() as u64;
    if n <= threshold {
        return None;
    }
    let rel = relative_path(path, root);
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    // Where test-only regions are identifiable (Rust `#[cfg(test)]`
    // modules today), report the production/test split so the operator
    // can tell "extract the tests" from "decompose the production code".
    let body = match production_test_split(&contents, ext) {
        Some((production, test)) => format!(
            "path: {rel}\nlines: {n}\nproduction_lines: {production}\ntest_lines: {test}\nthreshold: {threshold}"
        ),
        None => format!("path: {rel}\nlines: {n}\nthreshold: {threshold}"),
    };
    Some(Finding {
        severity: severity_for_ratio(n, threshold),
        subject: format!("file {rel} is {n} lines (threshold: {threshold})"),
        body,
        anchor: Some(format!("{rel}:1")),
    })
}

/// Compute the production-line / test-line split for a file when its
/// test-only regions are identifiable. Today only Rust `#[cfg(test)]`
/// (`mod tests { … }`) modules are recognized, reusing the same module-
/// boundary detection the duplicate-signature scan uses. Returns `None`
/// when no test-only region is present (callers then report the total
/// only). `production + test` always equals the file's total line count.
fn production_test_split(contents: &str, ext: &str) -> Option<(u64, u64)> {
    if ext != "rs" {
        return None;
    }
    let spans = rust_test_module_spans(contents);
    if spans.is_empty() {
        return None;
    }
    let total = contents.lines().count() as u64;
    let mut test_lines = 0u64;
    let mut offset = 0usize;
    for line in contents.split_inclusive('\n') {
        let line_start = offset;
        offset += line.len();
        if spans.iter().any(|(s, e)| line_start >= *s && line_start < *e) {
            test_lines += 1;
        }
    }
    let production = total.saturating_sub(test_lines);
    Some((production, test_lines))
}

/// One occurrence of a function signature in a file. Used to apply
/// `.brightline-ignore` match-suppression per-site (before grouping).
#[derive(Debug, Clone)]
struct SignatureSite {
    rel_path: String,
    line_number: usize,
    function: String,
    signature_line: String,
}

/// Detect identical function/method signatures across files. We use a
/// simple regex per language and stay deliberately approximate — the
/// audit's value is fast smoke-testing, not full parsing.
///
/// `ignore_entries` carries the parsed `.brightline-ignore` content;
/// every constituent site of a duplicate-signature finding is matched
/// against the ignore list before the finding is emitted. A finding
/// whose every site matches an ignore entry is dropped entirely. A
/// finding where only some sites match is emitted with the unmatched
/// sites only, plus a "(N suppressed by .brightline-ignore)" tail in
/// the subject.
fn check_signature_duplicates(
    files: &[PathBuf],
    root: &Path,
    ignore_entries: &[ignore::IgnoreEntry],
) -> Vec<Finding> {
    // signature_key → list of SignatureSite
    let mut occurrences: BTreeMap<String, Vec<SignatureSite>> = BTreeMap::new();
    for path in files {
        let ext = match path.extension().and_then(|e| e.to_str()) {
            Some(e) => e,
            None => continue,
        };
        let contents = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        // Strip Rust `mod tests { ... }` blocks (brace-counted) so test
        // helpers don't pollute the duplicate set.
        let stripped = if ext == "rs" {
            strip_rust_tests_modules(&contents)
        } else {
            contents.clone()
        };
        for (lineno, sig_key, function, signature_line) in extract_signature_sites(&stripped, ext) {
            occurrences
                .entry(sig_key)
                .or_default()
                .push(SignatureSite {
                    rel_path: relative_path(path, root),
                    line_number: lineno,
                    function,
                    signature_line,
                });
        }
    }
    let mut findings = Vec::new();
    for (sig_key, places) in occurrences {
        if places.len() < 2 {
            continue;
        }
        // Group by file: a signature appearing twice in the SAME file is
        // not a cross-file collision and isn't what this metric is for.
        let mut files_seen: BTreeMap<String, Vec<&SignatureSite>> = BTreeMap::new();
        for site in &places {
            files_seen.entry(site.rel_path.clone()).or_default().push(site);
        }
        if files_seen.len() < 2 {
            continue;
        }
        // Partition the distinct files into "matches an ignore entry"
        // and "doesn't". A file is considered matched when at least one
        // of its occurrences matches an entry — sites in the same file
        // are treated as one site per the audit's grouping rule above.
        let mut unmatched_files: Vec<(String, &SignatureSite)> = Vec::new();
        let mut suppressed_count: usize = 0;
        for (file, sites) in &files_seen {
            let any_matched = sites.iter().any(|s| {
                ignore_entries.iter().any(|e| {
                    ignore::entry_matches_site(e, &s.rel_path, &s.function, &s.signature_line)
                })
            });
            if any_matched {
                suppressed_count += 1;
            } else {
                let first = sites.first().copied().expect("non-empty per construction");
                unmatched_files.push((file.clone(), first));
            }
        }
        if unmatched_files.is_empty() {
            // Every constituent site is intentional — drop the finding.
            continue;
        }
        let mut subject_locations: Vec<String> = unmatched_files
            .iter()
            .map(|(p, site)| format!("{p}:{ln}", ln = site.line_number))
            .collect();
        subject_locations.sort();
        let mut body = subject_locations.join("\n");
        if suppressed_count > 0 {
            body.push_str(&format!(
                "\n({suppressed_count} site(s) suppressed by .brightline-ignore)"
            ));
        }
        let unmatched_count = unmatched_files.len();
        let subject = if suppressed_count > 0 {
            format!(
                "duplicate signature `{sig_key}` across {n} files ({suppressed_count} suppressed by .brightline-ignore)",
                n = unmatched_count,
            )
        } else {
            format!(
                "duplicate signature `{sig_key}` across {n} files",
                n = unmatched_count,
            )
        };
        findings.push(Finding {
            severity: Severity::Low,
            subject,
            body,
            anchor: subject_locations.first().cloned(),
        });
    }
    findings
}

#[allow(dead_code)]
fn extract_signatures(contents: &str, ext: &str) -> Vec<(usize, String)> {
    extract_signature_sites(contents, ext)
        .into_iter()
        .map(|(line, key, _name, _line_text)| (line, key))
        .collect()
}

/// Like [`extract_signatures`] but also returns the parsed function
/// name and the raw signature line — both needed to apply
/// `.brightline-ignore` match-suppression.
///
/// The `sig_key` is the function's **I/O profile** — name + parameter
/// *types* (parameter names normalized away) + return type where the
/// language exposes it — NOT the verbatim parameter text. Two
/// declarations with the same interface but different parameter names
/// therefore key identically. The raw `signature_line` (4th tuple
/// element) is the unchanged source line `.brightline-ignore` matches on.
fn extract_signature_sites(contents: &str, ext: &str) -> Vec<(usize, String, String, String)> {
    let re = match signature_regex(ext) {
        Some(r) => r,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    for (idx, line) in contents.lines().enumerate() {
        if let Some(caps) = re.captures(line) {
            let name = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            let params = caps.get(2).map(|m| m.as_str()).unwrap_or("");
            let ret = caps.get(3).map(|m| m.as_str()).unwrap_or("");
            let key = signature_io_key(name, params, ret, ext);
            out.push((idx + 1, key, name.to_string(), line.to_string()));
        }
    }
    out
}

/// Build the I/O-profile signature key: `name(<param-type-sequence>)`
/// optionally suffixed with ` -> <return-type>`. Parameter names are
/// normalized away; for languages without static parameter types the
/// param sequence falls back to `arity=<N>`.
fn signature_io_key(name: &str, params: &str, ret: &str, ext: &str) -> String {
    let param_profile = param_io_profile(params, ext);
    let ret_norm = normalize_return_type(ret);
    if ret_norm.is_empty() {
        format!("{name}({param_profile})")
    } else {
        format!("{name}({param_profile}) -> {ret_norm}")
    }
}

/// Languages whose parameter syntax is `name: type` (type follows a
/// colon), so a parameter's type can be recovered by stripping the name.
/// Other scanned languages fall back to parameter arity.
fn uses_colon_param_types(ext: &str) -> bool {
    matches!(ext, "rs" | "ts" | "tsx" | "py" | "swift" | "kt")
}

/// Render a parameter list as its type sequence (names normalized away).
/// For `name: type` languages each parameter contributes the text after
/// its `:`; a parameter with no `:` (e.g. a Rust `&self` receiver)
/// contributes its whitespace-collapsed text verbatim. For languages
/// without static parameter types the profile is `arity=<N>`.
fn param_io_profile(params: &str, ext: &str) -> String {
    let trimmed = params.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let parts: Vec<&str> = trimmed.split(',').map(|p| p.trim()).collect();
    if uses_colon_param_types(ext) {
        let types: Vec<String> = parts.iter().map(|p| colon_param_type(p)).collect();
        types.join(", ")
    } else {
        format!("arity={}", parts.len())
    }
}

/// The type of a single `name: type` parameter — the whitespace-collapsed
/// text after the first `:`. A parameter with no `:` (a receiver like
/// `&self`, or an untyped parameter) yields its collapsed text verbatim.
fn colon_param_type(p: &str) -> String {
    let collapse = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    match p.find(':') {
        Some(idx) => collapse(&p[idx + 1..]),
        None => collapse(p),
    }
}

/// Normalize a captured return type: collapse whitespace, drop a Rust
/// `where`-clause tail, AND trim trailing block-open / terminator
/// punctuation. Empty when the language exposes no return type.
fn normalize_return_type(ret: &str) -> String {
    let ret = ret.trim();
    if ret.is_empty() {
        return String::new();
    }
    // A `where` clause is part of the bounds, not the I/O type.
    let ret = ret.split(" where ").next().unwrap_or(ret);
    let collapsed = ret.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed
        .trim_end_matches('{')
        .trim_end_matches(';')
        .trim()
        .to_string()
}

/// One function definition's line span within a file: its signature
/// line through the line bearing its matching closing brace. Indices are
/// 0-based into the (test-stripped) line vector.
#[derive(Debug, Clone)]
struct FunctionSpan {
    name: String,
    start_idx: usize,
    end_idx: usize,
}

/// Scan `lines` (already test-stripped for Rust) for function
/// definitions, returning each one's name AND line span. The span runs
/// from the signature line to the line carrying its matching closing
/// delimiter, brace-matched while skipping comments AND double-quoted
/// strings. Reuses the same per-language [`signature_regex`] that powers
/// the duplicate-signature metric to locate each function start.
/// Declarations with no `{ … }` body (e.g. trait-method signatures) are
/// skipped because no balanced span is found.
fn scan_function_spans(lines: &[&str], ext: &str) -> Vec<FunctionSpan> {
    let re = match signature_regex(ext) {
        Some(r) => r,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        if let Some(caps) = re.captures(line) {
            let name = caps.get(1).map(|m| m.as_str()).unwrap_or("").to_string();
            if let Some(end_idx) = find_function_end(lines, idx) {
                out.push(FunctionSpan {
                    name,
                    start_idx: idx,
                    end_idx,
                });
            }
        }
    }
    out
}

/// Locate the line index of the matching closing brace for the function
/// whose signature is on `lines[start_idx]`. Brace-matches from the first
/// `{` at/after the signature line, skipping `//` line comments, `/* */`
/// block comments, AND double-quoted string literals (with `\` escapes).
/// Returns `None` when no balanced body is found.
fn find_function_end(lines: &[&str], start_idx: usize) -> Option<usize> {
    let mut depth: i64 = 0;
    let mut seen_open = false;
    let mut in_block_comment = false;
    for (i, line) in lines.iter().enumerate().skip(start_idx) {
        let bytes = line.as_bytes();
        let mut j = 0;
        let mut in_string = false;
        while j < bytes.len() {
            let b = bytes[j];
            if in_block_comment {
                if b == b'*' && j + 1 < bytes.len() && bytes[j + 1] == b'/' {
                    in_block_comment = false;
                    j += 2;
                    continue;
                }
                j += 1;
                continue;
            }
            if in_string {
                if b == b'\\' {
                    j += 2;
                    continue;
                }
                if b == b'"' {
                    in_string = false;
                }
                j += 1;
                continue;
            }
            if b == b'/' && j + 1 < bytes.len() && bytes[j + 1] == b'/' {
                break; // rest of the line is a line comment
            }
            if b == b'/' && j + 1 < bytes.len() && bytes[j + 1] == b'*' {
                in_block_comment = true;
                j += 2;
                continue;
            }
            if b == b'"' {
                in_string = true;
                j += 1;
                continue;
            }
            if b == b'{' {
                depth += 1;
                seen_open = true;
            } else if b == b'}' {
                depth -= 1;
                if seen_open && depth <= 0 {
                    return Some(i);
                }
            }
            j += 1;
        }
    }
    None
}

/// Measure each function's line span (outside test-only regions) AND emit
/// a finding for any function longer than `threshold` lines, with
/// graduated severity. Mirrors `check_file_size`'s subject/anchor shape
/// at the function granularity. Functions inside Rust `#[cfg(test)]`
/// modules are skipped (their lines are stripped before scanning).
fn check_function_sizes(path: &Path, root: &Path, threshold: u64) -> Vec<Finding> {
    let ext = match path.extension().and_then(|e| e.to_str()) {
        Some(e) => e,
        None => return Vec::new(),
    };
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let stripped = if ext == "rs" {
        strip_rust_tests_modules(&contents)
    } else {
        contents.clone()
    };
    let lines: Vec<&str> = stripped.lines().collect();
    let rel = relative_path(path, root);
    let mut out = Vec::new();
    for span in scan_function_spans(&lines, ext) {
        let n = (span.end_idx - span.start_idx + 1) as u64;
        if n <= threshold {
            continue;
        }
        let start_line = span.start_idx + 1;
        let name = span.name;
        out.push(Finding {
            severity: severity_for_ratio(n, threshold),
            subject: format!("function {name} in {rel} is {n} lines (threshold: {threshold})"),
            body: format!(
                "path: {rel}\nfunction: {name}\nstart_line: {start_line}\nlines: {n}\nthreshold: {threshold}"
            ),
            anchor: Some(format!("{rel}:{start_line}")),
        });
    }
    out
}

/// One occurrence of a normalized function body. Mirrors `SignatureSite`
/// so duplicate-body findings reuse the same `.brightline-ignore`
/// suppression path (`file` / `function` / `signature_match`).
#[derive(Debug, Clone)]
struct BodySite {
    rel_path: String,
    line_number: usize,
    function: String,
    signature_line: String,
}

/// Detect groups of two-or-more functions in different files (outside
/// test-only regions) whose normalized bodies are identical.
/// Normalization strips comments, collapses whitespace, AND canonicalizes
/// local identifier and string-literal spellings, so rename-only clones
/// collide despite differing function names. Each qualifying group emits
/// one `Severity::Low` finding listing its sites. A group is suppressed
/// in full when every constituent site matches a `.brightline-ignore`
/// entry, reusing [`ignore::entry_matches_site`].
fn check_body_duplicates(
    files: &[PathBuf],
    root: &Path,
    ignore_entries: &[ignore::IgnoreEntry],
) -> Vec<Finding> {
    let mut groups: BTreeMap<String, Vec<BodySite>> = BTreeMap::new();
    for path in files {
        let ext = match path.extension().and_then(|e| e.to_str()) {
            Some(e) => e,
            None => continue,
        };
        let contents = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let stripped = if ext == "rs" {
            strip_rust_tests_modules(&contents)
        } else {
            contents.clone()
        };
        let lines: Vec<&str> = stripped.lines().collect();
        for span in scan_function_spans(&lines, ext) {
            let body = extract_function_body(&lines, &span);
            let normalized = normalize_body(&body);
            // Skip empty / trivial bodies: an empty normalized body is not
            // a meaningful clone signal AND would group unrelated stubs.
            if normalized.is_empty() {
                continue;
            }
            groups.entry(normalized).or_default().push(BodySite {
                rel_path: relative_path(path, root),
                line_number: span.start_idx + 1,
                function: span.name.clone(),
                signature_line: lines[span.start_idx].to_string(),
            });
        }
    }
    let mut findings = Vec::new();
    for sites in groups.into_values() {
        if sites.len() < 2 {
            continue;
        }
        let distinct_files: std::collections::BTreeSet<&str> =
            sites.iter().map(|s| s.rel_path.as_str()).collect();
        if distinct_files.len() < 2 {
            continue;
        }
        // Suppression: drop the group when EVERY site matches an ignore
        // entry (same basis as duplicate-signature suppression).
        let all_ignored = sites.iter().all(|s| {
            ignore_entries.iter().any(|e| {
                ignore::entry_matches_site(e, &s.rel_path, &s.function, &s.signature_line)
            })
        });
        if all_ignored {
            continue;
        }
        let mut locations: Vec<String> = sites
            .iter()
            .map(|s| {
                format!(
                    "{p}:{ln} {f}",
                    p = s.rel_path,
                    ln = s.line_number,
                    f = s.function
                )
            })
            .collect();
        locations.sort();
        let names: std::collections::BTreeSet<&str> =
            sites.iter().map(|s| s.function.as_str()).collect();
        let names_joined = names.into_iter().collect::<Vec<_>>().join(", ");
        let anchor = locations
            .first()
            .and_then(|l| l.split(' ').next())
            .map(|s| s.to_string());
        findings.push(Finding {
            severity: Severity::Low,
            subject: format!(
                "duplicate body across {n} files ({names_joined})",
                n = distinct_files.len(),
            ),
            body: locations.join("\n"),
            anchor,
        });
    }
    findings
}

/// Public view of a function's line span, exposed for cross-module reuse
/// (the code reviewer's advisory size flag). Line numbers are 1-based AND
/// inclusive, matching the diff/anchor convention.
#[derive(Debug, Clone)]
pub(crate) struct FunctionLineSpan {
    pub name: String,
    pub start_line: usize,
    pub end_line: usize,
}

impl FunctionLineSpan {
    /// Number of source lines the function spans (signature through
    /// closing delimiter, inclusive).
    pub fn line_count(&self) -> u64 {
        (self.end_line - self.start_line + 1) as u64
    }
}

/// Scan `contents` for function definitions outside test-only regions,
/// returning each one's name AND 1-based inclusive line span. Reuses the
/// same scanning the function-size metric uses (test modules are stripped
/// before scanning, preserving line numbers). Exposed for the reviewer's
/// advisory size flag.
pub(crate) fn function_line_spans(contents: &str, ext: &str) -> Vec<FunctionLineSpan> {
    let stripped = if ext == "rs" {
        strip_rust_tests_modules(contents)
    } else {
        contents.to_string()
    };
    let lines: Vec<&str> = stripped.lines().collect();
    scan_function_spans(&lines, ext)
        .into_iter()
        .map(|s| FunctionLineSpan {
            name: s.name,
            start_line: s.start_idx + 1,
            end_line: s.end_idx + 1,
        })
        .collect()
}

/// Production/test line split for `contents`, exposed for the reviewer's
/// advisory. `None` when no test-only region is identifiable; otherwise
/// `(production_lines, test_lines)` summing to the total line count.
pub(crate) fn production_test_line_split(contents: &str, ext: &str) -> Option<(u64, u64)> {
    production_test_split(contents, ext)
}

/// Extract a function's body text — between its first `{` and its last
/// `}` — from the span's lines. Falls back to the full span text when a
/// balanced brace pair isn't found.
fn extract_function_body(lines: &[&str], span: &FunctionSpan) -> String {
    let slice = lines[span.start_idx..=span.end_idx].join("\n");
    match (slice.find('{'), slice.rfind('}')) {
        (Some(open), Some(close)) if open < close => slice[open + 1..close].to_string(),
        _ => slice,
    }
}

/// Normalize a function body for clone detection: strip comments,
/// canonicalize non-keyword identifiers to positional tokens (`v0`,
/// `v1`, … by first appearance), AND replace every string literal with a
/// single `STR` placeholder. Keywords, numbers, AND punctuation are
/// preserved verbatim; whitespace is collapsed (tokens are space-joined).
/// Two bodies differing only in local names or string spellings normalize
/// to the same token stream.
fn normalize_body(body: &str) -> String {
    let bytes = body.as_bytes();
    let mut tokens: Vec<String> = Vec::new();
    let mut ident_map: HashMap<String, String> = HashMap::new();
    let mut i = 0;
    let mut in_block_comment = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_block_comment {
            if b == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                in_block_comment = false;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            in_block_comment = true;
            i += 2;
            continue;
        }
        if b == b'"' {
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if bytes[i] == b'"' {
                    i += 1;
                    break;
                }
                i += 1;
            }
            tokens.push("STR".to_string());
            continue;
        }
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if b.is_ascii_alphabetic() || b == b'_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let word = &body[start..i];
            if is_normalize_keyword(word) {
                tokens.push(word.to_string());
            } else {
                let next_id = ident_map.len();
                let canon = ident_map
                    .entry(word.to_string())
                    .or_insert_with(|| format!("v{next_id}"));
                tokens.push(canon.clone());
            }
            continue;
        }
        if b.is_ascii_digit() {
            let start = i;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'.' || bytes[i] == b'_')
            {
                i += 1;
            }
            tokens.push(body[start..i].to_string());
            continue;
        }
        // Single punctuation byte (ASCII operators / delimiters). A
        // multibyte UTF-8 lead byte falls here too; it only needs to be
        // deterministic, not semantically a char.
        tokens.push((b as char).to_string());
        i += 1;
    }
    tokens.join(" ")
}

/// Keywords preserved verbatim during body normalization so the control-
/// flow skeleton survives identifier canonicalization. Covers Rust plus a
/// few cross-language declaration keywords so clones in the other scanned
/// languages keep their structure too.
fn is_normalize_keyword(w: &str) -> bool {
    matches!(
        w,
        "as" | "async"
            | "await"
            | "break"
            | "const"
            | "continue"
            | "crate"
            | "dyn"
            | "else"
            | "enum"
            | "extern"
            | "false"
            | "fn"
            | "for"
            | "if"
            | "impl"
            | "in"
            | "let"
            | "loop"
            | "match"
            | "mod"
            | "move"
            | "mut"
            | "pub"
            | "ref"
            | "return"
            | "self"
            | "Self"
            | "static"
            | "struct"
            | "super"
            | "trait"
            | "true"
            | "type"
            | "unsafe"
            | "use"
            | "where"
            | "while"
            // cross-language declaration / control keywords
            | "function"
            | "def"
            | "var"
            | "val"
            | "func"
            | "null"
            | "None"
            | "True"
            | "False"
            | "new"
            | "class"
    )
}

pub(super) fn signature_regex(ext: &str) -> Option<Regex> {
    let pattern = match ext {
        "rs" => Some(r"^\s*(?:pub\s+(?:\([^)]*\)\s+)?)?(?:async\s+)?(?:const\s+)?(?:unsafe\s+)?fn\s+(\w+)\s*(?:<[^>]*>)?\s*\(([^)]*)\)(?:\s*->\s*([^{]+))?"),
        "py" => Some(r"^\s*(?:async\s+)?def\s+(\w+)\s*\(([^)]*)\)\s*(?:->[^:]+)?\s*:"),
        "cs" => Some(r"^\s*(?:public|private|protected|internal)?\s*(?:static\s+)?(?:async\s+)?\w[\w<>?]*\s+(\w+)\s*\(([^)]*)\)"),
        "ts" | "tsx" | "js" | "jsx" => Some(
            r"^\s*(?:export\s+)?(?:async\s+)?function\s+(\w+)\s*\(([^)]*)\)",
        ),
        "go" => Some(r"^\s*func\s+(?:\([^)]+\)\s+)?(\w+)\s*\(([^)]*)\)"),
        "java" | "kt" => Some(
            r"^\s*(?:public|private|protected)?\s*(?:static\s+)?(?:async\s+)?[\w<>?\[\]]+\s+(\w+)\s*\(([^)]*)\)",
        ),
        "swift" => Some(r"^\s*(?:public|private|internal|fileprivate)?\s*func\s+(\w+)\s*\(([^)]*)\)"),
        _ => None,
    };
    pattern.and_then(|p| Regex::new(p).ok())
}

/// Byte ranges of every `mod tests { ... }` block in Rust source,
/// brace-matched so nested braces don't trip the scanner. The block can
/// be preceded by `#[cfg(test)]` or any other attribute. Each returned
/// `(start, end)` is a half-open byte range `[start, end)` spanning from
/// the `mod` keyword (or its leading attribute) through the closing
/// brace. Shared by [`strip_rust_tests_modules`] and the production/test
/// line split so both agree on what counts as a test-only region.
pub(super) fn rust_test_module_spans(src: &str) -> Vec<(usize, usize)> {
    let re = match Regex::new(r"(?m)^\s*(?:#\[[^\]]+\]\s*)?mod\s+tests\s*\{") {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let mut spans = Vec::new();
    let mut last = 0;
    let bytes = src.as_bytes();
    while let Some(m) = re.find_at(src, last) {
        // Walk forward from the opening brace position to find the
        // matching closing brace.
        let body_start = m.end(); // position right after `{`
        let mut depth: i64 = 1;
        let mut idx = body_start;
        let mut in_line_comment = false;
        let mut in_block_comment = false;
        while idx < bytes.len() {
            let b = bytes[idx];
            if in_line_comment {
                if b == b'\n' {
                    in_line_comment = false;
                }
                idx += 1;
                continue;
            }
            if in_block_comment {
                if b == b'*' && idx + 1 < bytes.len() && bytes[idx + 1] == b'/' {
                    in_block_comment = false;
                    idx += 2;
                    continue;
                }
                idx += 1;
                continue;
            }
            if b == b'/' && idx + 1 < bytes.len() {
                let next = bytes[idx + 1];
                if next == b'/' {
                    in_line_comment = true;
                    idx += 2;
                    continue;
                } else if next == b'*' {
                    in_block_comment = true;
                    idx += 2;
                    continue;
                }
            }
            if b == b'{' {
                depth += 1;
            } else if b == b'}' {
                depth -= 1;
                if depth == 0 {
                    idx += 1;
                    break;
                }
            }
            idx += 1;
        }
        spans.push((m.start(), idx));
        last = idx;
    }
    spans
}

/// Strip every `mod tests { ... }` block from Rust source, brace-matched
/// so nested braces don't trip the scanner. The block can be preceded by
/// `#[cfg(test)]` or any other attribute. Returns the source with the
/// matched ranges replaced by empty lines (preserving line numbers for
/// the duplicate-signature anchors).
pub(super) fn strip_rust_tests_modules(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut last = 0;
    for (start, end) in rust_test_module_spans(src) {
        out.push_str(&src[last..start]);
        // Replace the entire `mod tests { ... }` span with blank lines
        // matching the original newline count, preserving line numbers.
        let span = &src[start..end];
        let newlines = span.bytes().filter(|b| *b == b'\n').count();
        for _ in 0..newlines {
            out.push('\n');
        }
        last = end;
    }
    out.push_str(&src[last..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(p: &Path, contents: &str) {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, contents).unwrap();
    }

    fn settings_with_threshold(t: u64) -> HashMap<String, AuditSettings> {
        let mut extra = HashMap::new();
        extra.insert(
            SETTINGS_KEY_FILE_LINES.to_string(),
            serde_yml::Value::Number(serde_yml::Number::from(t)),
        );
        let mut s = HashMap::new();
        s.insert(
            ArchitectureBrightlineAudit::TYPE.to_string(),
            AuditSettings {
                prompt_path: None,
                notify_on_clean: false,
                extra,
            },
        );
        s
    }

    #[test]
    fn file_size_metric_flags_long_files() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        // 1200 lines under src/ — exceeds default 800.
        let big: String = (0..1200).map(|i| format!("// line {i}\n")).collect();
        write(&ws.join("src/big.rs"), &big);
        let audit = ArchitectureBrightlineAudit::new(&HashMap::new());
        let findings = audit.analyze(ws).unwrap();
        assert!(
            findings
                .iter()
                .any(|f| f.subject.contains("src/big.rs") && f.subject.contains("1200")),
            "expected a finding for src/big.rs; got: {findings:?}"
        );
    }

    #[test]
    fn file_size_metric_respects_threshold_override() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        let medium: String = (0..600).map(|i| format!("// line {i}\n")).collect();
        write(&ws.join("src/medium.rs"), &medium);
        // Default (800) → no finding.
        let audit = ArchitectureBrightlineAudit::new(&HashMap::new());
        let findings = audit.analyze(ws).unwrap();
        assert!(
            !findings.iter().any(|f| f.subject.contains("medium.rs")),
            "default threshold should not flag 600-line file: {findings:?}"
        );
        // Override (400) → finding.
        let audit2 = ArchitectureBrightlineAudit::new(&settings_with_threshold(400));
        let findings2 = audit2.analyze(ws).unwrap();
        assert!(
            findings2.iter().any(|f| f.subject.contains("medium.rs")),
            "override threshold should flag 600-line file: {findings2:?}"
        );
    }

    #[test]
    fn file_size_metric_ignores_excluded_dirs() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        let big: String = (0..2000).map(|i| format!("// l {i}\n")).collect();
        write(&ws.join("node_modules/lib/big.js"), &big);
        write(&ws.join("target/debug/big.rs"), &big);
        write(&ws.join("vendor/dep/big.go"), &big);
        let audit = ArchitectureBrightlineAudit::new(&HashMap::new());
        let findings = audit.analyze(ws).unwrap();
        assert!(
            findings.is_empty(),
            "excluded dirs must not contribute findings: {findings:?}"
        );
    }

    #[test]
    fn signature_duplicate_metric_flags_cross_file_collisions_rust() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        write(
            &ws.join("src/a.rs"),
            "pub fn helper(x: u32, y: u32) -> u32 { x + y }\n",
        );
        write(
            &ws.join("src/b.rs"),
            "pub fn helper(x: u32, y: u32) -> u32 { x * y }\n",
        );
        let audit = ArchitectureBrightlineAudit::new(&HashMap::new());
        let findings = audit.analyze(ws).unwrap();
        assert!(
            findings.iter().any(|f| f.subject.contains("helper")),
            "expected a duplicate-signature finding for `helper`: {findings:?}"
        );
    }

    #[test]
    fn signature_duplicate_metric_ignores_tests_module() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        write(
            &ws.join("src/a.rs"),
            "pub fn alpha() {}\n#[cfg(test)]\nmod tests { fn alpha() {} }\n",
        );
        write(
            &ws.join("src/b.rs"),
            "#[cfg(test)]\nmod tests { fn alpha() {} }\n",
        );
        let audit = ArchitectureBrightlineAudit::new(&HashMap::new());
        let findings = audit.analyze(ws).unwrap();
        // `alpha` appears as a real fn only in src/a.rs; the others are
        // inside `mod tests { ... }` and must be stripped → no cross-file
        // collision.
        assert!(
            !findings
                .iter()
                .any(|f| f.subject.contains("duplicate signature") && f.subject.contains("alpha")),
            "tests module signatures must be ignored: {findings:?}"
        );
    }

    #[test]
    fn audit_returns_no_findings_on_clean_codebase() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        // A small file, no duplicates.
        write(&ws.join("src/lib.rs"), "pub fn one() {}\n");
        write(&ws.join("src/main.rs"), "fn two() {}\nfn main() { one(); two(); }\n");
        let audit = ArchitectureBrightlineAudit::new(&HashMap::new());
        let findings = audit.analyze(ws).unwrap();
        assert!(findings.is_empty(), "clean codebase: {findings:?}");
    }

    #[test]
    fn audit_returns_findings_for_known_violations() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        // Both: file-size violation AND signature duplicate.
        let big: String = (0..1500)
            .map(|i| format!("fn shared_name() {{ /* {i} */ }}\n"))
            .collect();
        write(&ws.join("src/giant.rs"), &big);
        write(&ws.join("src/other.rs"), "fn shared_name() {}\n");
        let audit = ArchitectureBrightlineAudit::new(&HashMap::new());
        let findings = audit.analyze(ws).unwrap();
        assert!(findings.len() >= 2);
        let mut subjects: Vec<&String> = findings.iter().map(|f| &f.subject).collect();
        subjects.sort();
        let joined = subjects
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(
            joined.contains("giant.rs is 1500 lines"),
            "expected size finding for giant.rs: {joined}"
        );
        assert!(
            joined.contains("duplicate signature `shared_name"),
            "expected duplicate-signature finding: {joined}"
        );
    }

    #[test]
    fn excluded_dirs_skipped_during_walk() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        write(&ws.join("src/a.rs"), "fn x() {}\n");
        write(&ws.join("node_modules/a.js"), "function x() {}\n");
        write(&ws.join(".git/HEAD"), "ref: refs/heads/main\n");
        let collected = collect_source_files(ws).unwrap();
        let rels: Vec<String> = collected
            .iter()
            .map(|p| relative_path(p, ws))
            .collect();
        assert_eq!(rels, vec!["src/a.rs".to_string()]);
    }

    #[test]
    fn signature_regex_parses_async_pub_rust() {
        let re = signature_regex("rs").unwrap();
        let captures = re
            .captures("    pub async fn do_thing(a: u32) -> Result<()> {")
            .unwrap();
        assert_eq!(&captures[1], "do_thing");
        assert_eq!(captures[2].trim(), "a: u32");
    }

    /// Workspace-validity gate (see `audits-require-valid-workspace`):
    /// brightline must skip cleanly when the workspace is missing, even
    /// though it doesn't write proposals. The gate is uniform across
    /// every audit type for framework-contract consistency.
    #[tokio::test]
    async fn workspace_unavailable_when_path_does_not_exist() {
        use crate::audits::{AuditContext, AuditLogWriter};
        use crate::config::RepositoryConfig;

        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("never-existed");
        assert!(!workspace.exists());

        let paths = crate::paths::DaemonPaths::under_root(tmp.path());
        let log_writer = AuditLogWriter::open(&paths, tmp.path(), ArchitectureBrightlineAudit::TYPE)
            .expect("log writer opens");
        let log_path = log_writer.path().to_path_buf();
        let repo = RepositoryConfig {
            url: "git@github.com:test/repo.git".into(),
            local_path: None,
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            poll_interval_sec: 60,
            chatops_channel_id: None,
            max_changes_per_pr: None,
            audits: None,
            spec_storage: None,
            upstream: None,
            auto_submit_pr: true,
        };
        let mut ctx = AuditContext {
            workspace: &workspace,
            repo: &repo,
            chatops_ctx: None,
            log_writer,
            max_validation_retries: 0,
        };
        let audit = ArchitectureBrightlineAudit::new(&HashMap::new());
        let outcome = audit.run(&mut ctx).await.expect("gate returns Ok");
        match outcome {
            AuditOutcome::WorkspaceUnavailable {
                audit_type,
                workspace_path,
                reason,
            } => {
                assert_eq!(audit_type, ArchitectureBrightlineAudit::TYPE);
                assert_eq!(workspace_path, workspace);
                assert_eq!(reason, "workspace directory does not exist");
            }
            other => panic!("expected WorkspaceUnavailable, got {other:?}"),
        }
        assert!(!workspace.exists());
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    /// Workspace-validity gate: existing directory without `.git/` →
    /// WorkspaceUnavailable; the audit must not contribute zero-file
    /// garbage findings.
    #[tokio::test]
    async fn workspace_unavailable_when_dot_git_missing() {
        use crate::audits::{AuditContext, AuditLogWriter};
        use crate::config::RepositoryConfig;

        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("ws-no-git");
        std::fs::create_dir_all(&workspace).unwrap();
        let before: Vec<std::ffi::OsString> = std::fs::read_dir(&workspace)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();

        let paths = crate::paths::DaemonPaths::under_root(tmp.path());
        let log_writer = AuditLogWriter::open(&paths, tmp.path(), ArchitectureBrightlineAudit::TYPE)
            .expect("log writer opens");
        let log_path = log_writer.path().to_path_buf();
        let repo = RepositoryConfig {
            url: "git@github.com:test/repo.git".into(),
            local_path: None,
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            poll_interval_sec: 60,
            chatops_channel_id: None,
            max_changes_per_pr: None,
            audits: None,
            spec_storage: None,
            upstream: None,
            auto_submit_pr: true,
        };
        let mut ctx = AuditContext {
            workspace: &workspace,
            repo: &repo,
            chatops_ctx: None,
            log_writer,
            max_validation_retries: 0,
        };
        let audit = ArchitectureBrightlineAudit::new(&HashMap::new());
        let outcome = audit.run(&mut ctx).await.expect("gate returns Ok");
        match outcome {
            AuditOutcome::WorkspaceUnavailable { reason, .. } => {
                assert_eq!(reason, "workspace exists but has no .git/ subdirectory");
            }
            other => panic!("expected WorkspaceUnavailable, got {other:?}"),
        }
        let after: Vec<std::ffi::OsString> = std::fs::read_dir(&workspace)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(before, after);
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    #[test]
    fn ignore_fully_matching_finding_is_suppressed() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        write(
            &ws.join("src/a.rs"),
            "pub fn helper(x: u32, y: u32) -> u32 { x + y }\n",
        );
        write(
            &ws.join("src/b.rs"),
            "pub fn helper(x: u32, y: u32) -> u32 { x * y }\n",
        );
        write(
            &ws.join(".brightline-ignore"),
            r#"ignore:
  - file: src/a.rs
    function: helper
    signature_match: "fn helper(x: u32"
    reason: "intentional"
  - file: src/b.rs
    function: helper
    signature_match: "fn helper(x: u32"
    reason: "intentional"
"#,
        );
        let audit = ArchitectureBrightlineAudit::new(&HashMap::new());
        let findings = audit.analyze(ws).unwrap();
        assert!(
            !findings.iter().any(|f| f.subject.contains("duplicate signature")),
            "all sites matched; finding should be suppressed: {findings:?}"
        );
    }

    #[test]
    fn ignore_partial_match_emits_unmatched_sites_only() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        write(
            &ws.join("src/a.rs"),
            "pub fn helper(x: u32, y: u32) -> u32 { x + y }\n",
        );
        write(
            &ws.join("src/b.rs"),
            "pub fn helper(x: u32, y: u32) -> u32 { x * y }\n",
        );
        write(
            &ws.join("src/c.rs"),
            "pub fn helper(x: u32, y: u32) -> u32 { x - y }\n",
        );
        write(
            &ws.join(".brightline-ignore"),
            r#"ignore:
  - file: src/a.rs
    function: helper
    signature_match: "fn helper(x: u32"
    reason: "intentional"
  - file: src/b.rs
    function: helper
    signature_match: "fn helper(x: u32"
    reason: "intentional"
"#,
        );
        let audit = ArchitectureBrightlineAudit::new(&HashMap::new());
        let findings = audit.analyze(ws).unwrap();
        let dupes: Vec<&Finding> = findings
            .iter()
            .filter(|f| f.subject.starts_with("duplicate signature"))
            .collect();
        assert_eq!(dupes.len(), 1, "expected one partial-suppression finding: {findings:?}");
        let f = dupes[0];
        assert!(
            f.subject.contains("across 1 files"),
            "subject should reflect unmatched site count: {}",
            f.subject
        );
        assert!(
            f.subject.contains("2 suppressed by .brightline-ignore"),
            "subject should note the suppressed count: {}",
            f.subject
        );
        assert!(
            f.body.contains("src/c.rs"),
            "body should name the unmatched site: {}",
            f.body
        );
        assert!(
            !f.body.contains("src/a.rs") && !f.body.contains("src/b.rs"),
            "body should omit suppressed sites: {}",
            f.body
        );
        assert!(
            f.body.contains("2 site(s) suppressed by .brightline-ignore"),
            "body should mention suppressed count: {}",
            f.body
        );
    }

    #[test]
    fn ignore_no_matches_behaves_like_today() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        write(
            &ws.join("src/a.rs"),
            "pub fn helper(x: u32, y: u32) -> u32 { x + y }\n",
        );
        write(
            &ws.join("src/b.rs"),
            "pub fn helper(x: u32, y: u32) -> u32 { x * y }\n",
        );
        write(
            &ws.join("src/c.rs"),
            "pub fn helper(x: u32, y: u32) -> u32 { x - y }\n",
        );
        // No .brightline-ignore.
        let audit = ArchitectureBrightlineAudit::new(&HashMap::new());
        let findings = audit.analyze(ws).unwrap();
        let dupes: Vec<&Finding> = findings
            .iter()
            .filter(|f| f.subject.starts_with("duplicate signature"))
            .collect();
        assert_eq!(dupes.len(), 1, "expected one finding: {findings:?}");
        let f = dupes[0];
        assert!(
            f.subject.contains("across 3 files"),
            "subject should list all sites: {}",
            f.subject
        );
        assert!(
            f.body.contains("src/a.rs")
                && f.body.contains("src/b.rs")
                && f.body.contains("src/c.rs"),
            "body should list every site: {}",
            f.body
        );
        assert!(
            !f.body.contains("suppressed"),
            "body should NOT mention suppression when no entries match: {}",
            f.body
        );
    }

    #[test]
    fn ignore_empty_list_no_suppression() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        write(
            &ws.join("src/a.rs"),
            "pub fn helper(x: u32, y: u32) -> u32 { x + y }\n",
        );
        write(
            &ws.join("src/b.rs"),
            "pub fn helper(x: u32, y: u32) -> u32 { x * y }\n",
        );
        // Empty top-level `ignore` list.
        write(
            &ws.join(".brightline-ignore"),
            "ignore: []\n",
        );
        let audit = ArchitectureBrightlineAudit::new(&HashMap::new());
        let findings = audit.analyze(ws).unwrap();
        let dupes: Vec<&Finding> = findings
            .iter()
            .filter(|f| f.subject.starts_with("duplicate signature"))
            .collect();
        assert_eq!(dupes.len(), 1, "expected one finding: {findings:?}");
    }

    #[test]
    fn stale_entries_emit_findings_with_documented_prefix() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        // src/present.rs has `kept`; src/gone.rs is missing.
        write(&ws.join("src/present.rs"), "pub fn kept(x: u32) -> u32 { x }\n");
        write(
            &ws.join(".brightline-ignore"),
            r#"ignore:
  - file: src/gone.rs
    function: vanished
    signature_match: "fn vanished("
    reason: "this file was deleted"
  - file: src/present.rs
    function: kept
    signature_match: "fn kept(x: u32"
    reason: "still here"
"#,
        );
        let audit = ArchitectureBrightlineAudit::new(&HashMap::new());
        let findings = audit.analyze(ws).unwrap();
        let stale: Vec<&Finding> = findings
            .iter()
            .filter(|f| f.subject.starts_with(STALE_IGNORE_SUBJECT_PREFIX))
            .collect();
        assert_eq!(stale.len(), 1, "expected exactly one stale entry: {findings:?}");
        let f = stale[0];
        assert!(
            f.subject.contains("src/gone.rs") && f.subject.contains("vanished"),
            "stale subject should name file + function: {}",
            f.subject
        );
        assert!(
            f.subject.contains("this file was deleted"),
            "stale subject should carry the reason: {}",
            f.subject
        );
        assert!(
            f.body.contains("file: src/gone.rs"),
            "stale body should name file: {}",
            f.body
        );
    }

    /// Regression guard for `a02-audit-proposal-created-notification`.
    /// `architecture_brightline` does NOT generate an LLM proposal, so
    /// the `🔍 created proposal` chatops notification must NEVER fire
    /// from this audit — even when it produces a non-empty findings
    /// set. The test runs the full `Audit::run` entry point through
    /// the trait and asserts that the recording chatops backend
    /// captured zero notifications.
    #[tokio::test]
    async fn brightline_does_not_post_proposal_created_notification() {
        use super::super::test_support::{RecordingBackend, make_recording_ctx};
        use crate::audits::{AuditContext, AuditLogWriter};
        use crate::config::RepositoryConfig;
        use std::sync::Arc;

        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        // Workspace-validity gate (see `audits-require-valid-workspace`)
        // requires a `.git/` subdirectory; without it the audit would
        // short-circuit to `WorkspaceUnavailable` and never reach the
        // Reported branch this test exercises.
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        // Force at least one finding (size + duplicate signature) so
        // the audit returns Reported with non-empty findings.
        let big: String = (0..1500)
            .map(|i| format!("fn shared_name() {{ /* {i} */ }}\n"))
            .collect();
        write(&ws.join("src/giant.rs"), &big);
        write(&ws.join("src/other.rs"), "fn shared_name() {}\n");

        let backend = Arc::new(RecordingBackend::new());
        let chatops = make_recording_ctx(backend.clone());

        let audit = ArchitectureBrightlineAudit::new(&HashMap::new());
        let repo = RepositoryConfig {
            url: "git@github.com:test/repo.git".into(),
            local_path: None,
            base_branch: "main".into(),
            agent_branch: "agent-q".into(),
            poll_interval_sec: 60,
            chatops_channel_id: None,
            max_changes_per_pr: None,
            audits: None,
            spec_storage: None,
            upstream: None,
            auto_submit_pr: true,
        };
        let paths = crate::paths::DaemonPaths::under_root(ws);
        let log_writer =
            AuditLogWriter::open(&paths, ws, ArchitectureBrightlineAudit::TYPE)
                .expect("log writer opens");
        let log_path = log_writer.path().to_path_buf();
        let mut ctx = AuditContext {
            workspace: ws,
            repo: &repo,
            chatops_ctx: Some(&chatops),
            log_writer,
            max_validation_retries: 0,
        };
        let outcome = audit.run(&mut ctx).await.expect("brightline runs");
        match outcome {
            AuditOutcome::Reported { findings, .. } => {
                assert!(
                    !findings.is_empty(),
                    "fixture must produce findings so the no-fire assertion is meaningful"
                );
            }
            other => panic!("brightline must return Reported, got {other:?}"),
        }
        let calls = backend.calls();
        assert!(
            calls.is_empty(),
            "🔍 created proposal must NOT fire from architecture_brightline; got: {calls:?}"
        );

        if let Some(parent) = log_path.parent() {
            let _ = std::fs::remove_dir_all(parent.parent().unwrap_or(parent));
        }
    }

    // ====================================================================
    // a67: graduated severity, function-size, duplicate-body, I/O profile
    // ====================================================================

    /// Build settings overriding both the file AND function line
    /// thresholds for the audit.
    fn settings_with_thresholds(file_t: u64, func_t: u64) -> HashMap<String, AuditSettings> {
        let mut extra = HashMap::new();
        extra.insert(
            SETTINGS_KEY_FILE_LINES.to_string(),
            serde_yml::Value::Number(serde_yml::Number::from(file_t)),
        );
        extra.insert(
            SETTINGS_KEY_FUNCTION_LINES.to_string(),
            serde_yml::Value::Number(serde_yml::Number::from(func_t)),
        );
        let mut s = HashMap::new();
        s.insert(
            ArchitectureBrightlineAudit::TYPE.to_string(),
            AuditSettings {
                prompt_path: None,
                notify_on_clean: false,
                extra,
            },
        );
        s
    }

    /// 8.1 — `severity_for_ratio` band edges are exact. Threshold 100 so
    /// the documented ratios land on round integers.
    #[test]
    fn severity_for_ratio_band_edges() {
        let t = 100u64;
        // 1× and 1.49× → Low; 1.5× and 2.49× → Medium; 2.5× and 10× → High.
        assert_eq!(severity_for_ratio(100, t), Severity::Low); // 1×
        assert_eq!(severity_for_ratio(149, t), Severity::Low); // 1.49×
        assert_eq!(severity_for_ratio(150, t), Severity::Medium); // 1.5×
        assert_eq!(severity_for_ratio(249, t), Severity::Medium); // 2.49×
        assert_eq!(severity_for_ratio(250, t), Severity::High); // 2.5×
        assert_eq!(severity_for_ratio(1000, t), Severity::High); // 10×
    }

    /// 8.2 — file metric: just-over → Low, ≥ 2.5× → High; a file with a
    /// `#[cfg(test)]` region reports a production/test split summing to
    /// the total.
    #[test]
    fn file_metric_graduated_severity_and_split() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        // Just over a threshold of 100 → Low.
        let low_file: String = (0..120).map(|i| format!("// line {i}\n")).collect();
        write(&ws.join("src/low.rs"), &low_file);
        // 3× the threshold → High.
        let high_file: String = (0..300).map(|i| format!("// line {i}\n")).collect();
        write(&ws.join("src/high.rs"), &high_file);
        let audit = ArchitectureBrightlineAudit::new(&settings_with_threshold(100));
        let findings = audit.analyze(ws).unwrap();
        let low = findings
            .iter()
            .find(|f| f.subject.contains("src/low.rs"))
            .expect("low.rs flagged");
        assert_eq!(low.severity, Severity::Low, "120/100 must grade Low");
        let high = findings
            .iter()
            .find(|f| f.subject.contains("src/high.rs"))
            .expect("high.rs flagged");
        assert_eq!(high.severity, Severity::High, "300/100 must grade High");

        // Production/test split: 20 production lines + an 18-line
        // `#[cfg(test)]` region = 38 total.
        let dir2 = TempDir::new().unwrap();
        let ws2 = dir2.path();
        let mut src = String::new();
        for i in 0..20 {
            src.push_str(&format!("pub fn prod_{i}() {{ let _ = {i}; }}\n"));
        }
        src.push_str("#[cfg(test)]\n");
        src.push_str("mod tests {\n");
        for i in 0..15 {
            src.push_str(&format!("    fn test_{i}() {{}}\n"));
        }
        src.push_str("}\n");
        write(&ws2.join("src/split.rs"), &src);
        let audit2 = ArchitectureBrightlineAudit::new(&settings_with_threshold(10));
        let findings2 = audit2.analyze(ws2).unwrap();
        let f = findings2
            .iter()
            .find(|f| f.subject.starts_with("file ") && f.subject.contains("src/split.rs"))
            .expect("split.rs flagged for size");
        // Parse the split from the body and assert it sums to the total.
        let total = body_value(&f.body, "lines:");
        let prod = body_value(&f.body, "production_lines:");
        let test = body_value(&f.body, "test_lines:");
        assert_eq!(total, 38);
        assert_eq!(prod + test, total, "production + test must sum to total");
        assert!(test > 0, "the #[cfg(test)] region must be counted: {}", f.body);
    }

    /// Pull a `key: <N>` numeric value out of a finding body.
    fn body_value(body: &str, key: &str) -> u64 {
        body.lines()
            .find_map(|l| l.trim().strip_prefix(key))
            .map(|v| v.trim().parse().unwrap())
            .unwrap_or_else(|| panic!("missing `{key}` in body: {body}"))
    }

    /// 8.3 — function metric: a non-test function over the threshold is
    /// reported with graduated severity AND a `<file>:<start-line>`
    /// anchor; an equally-long function inside `#[cfg(test)]` is NOT.
    #[test]
    fn function_metric_reports_nontest_and_skips_test() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        let mut src = String::new();
        src.push_str("pub fn big_prod() {\n");
        for i in 0..600 {
            src.push_str(&format!("    let _x{i} = {i};\n"));
        }
        src.push_str("}\n");
        // An equally-long function buried in a test module.
        src.push_str("#[cfg(test)]\n");
        src.push_str("mod tests {\n");
        src.push_str("    fn big_test() {\n");
        for i in 0..600 {
            src.push_str(&format!("        let _y{i} = {i};\n"));
        }
        src.push_str("    }\n");
        src.push_str("}\n");
        write(&ws.join("src/funcs.rs"), &src);
        // High file threshold so only function findings appear.
        let audit = ArchitectureBrightlineAudit::new(&settings_with_thresholds(100_000, 200));
        let findings = audit.analyze(ws).unwrap();
        let fn_findings: Vec<&Finding> = findings
            .iter()
            .filter(|f| f.subject.starts_with("function "))
            .collect();
        assert_eq!(
            fn_findings.len(),
            1,
            "only the non-test function is reported: {findings:?}"
        );
        let f = fn_findings[0];
        assert!(f.subject.contains("big_prod"), "subject: {}", f.subject);
        assert!(
            !findings.iter().any(|f| f.subject.contains("big_test")),
            "the #[cfg(test)] function must not be reported: {findings:?}"
        );
        // 602-line span vs 200 threshold → ≥ 2.5× → High.
        assert_eq!(f.severity, Severity::High);
        assert_eq!(
            f.anchor.as_deref(),
            Some("src/funcs.rs:1"),
            "anchor names the function's start line"
        );
    }

    /// 8.4 — duplicate-body metric: rename-only clones in different files
    /// collide into one finding; a clone inside `#[cfg(test)]` is
    /// excluded; an ignore entry covering the sites suppresses it.
    #[test]
    fn duplicate_body_metric_collapses_renamed_clones() {
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        write(
            &ws.join("src/a.rs"),
            "pub fn alert_disk(threshold: u32) -> String {\n    let label = \"disk\";\n    format!(\"{label} over {threshold}\")\n}\n",
        );
        write(
            &ws.join("src/b.rs"),
            "pub fn alert_mem(limit: u32) -> String {\n    let name = \"mem\";\n    format!(\"{name} over {limit}\")\n}\n",
        );
        // Same body, but inside a `#[cfg(test)]` module → excluded.
        write(
            &ws.join("src/c.rs"),
            "#[cfg(test)]\nmod tests {\n    fn alert_net(cap: u32) -> String {\n        let tag = \"net\";\n        format!(\"{tag} over {cap}\")\n    }\n}\n",
        );
        let audit = ArchitectureBrightlineAudit::new(&HashMap::new());
        let findings = audit.analyze(ws).unwrap();
        let bodies: Vec<&Finding> = findings
            .iter()
            .filter(|f| f.subject.starts_with(DUPLICATE_BODY_SUBJECT_PREFIX))
            .collect();
        assert_eq!(bodies.len(), 1, "one duplicate-body group: {findings:?}");
        let f = bodies[0];
        assert!(
            f.subject.contains("across 2 files"),
            "test-module clone must be excluded: {}",
            f.subject
        );
        assert!(
            !f.body.contains("alert_net") && !f.body.contains("src/c.rs"),
            "test-module clone must not appear among the sites: {}",
            f.body
        );

        // With an ignore entry covering BOTH live sites, the group is
        // suppressed.
        write(
            &ws.join(".brightline-ignore"),
            "ignore:\n  - file: src/a.rs\n    function: alert_disk\n    signature_match: \"fn alert_disk(\"\n    reason: \"intentional\"\n  - file: src/b.rs\n    function: alert_mem\n    signature_match: \"fn alert_mem(\"\n    reason: \"intentional\"\n",
        );
        let findings2 = audit.analyze(ws).unwrap();
        assert!(
            !findings2
                .iter()
                .any(|f| f.subject.starts_with(DUPLICATE_BODY_SUBJECT_PREFIX)),
            "ignore entries covering all sites suppress the group: {findings2:?}"
        );
    }

    /// 8.5 — signature key is the I/O profile: same name + parameter
    /// *types* but different parameter *names* collide; same name but
    /// different parameter types do not.
    #[test]
    fn signature_io_profile_keys_on_types_not_names() {
        // Different parameter names, same types → collision.
        let dir = TempDir::new().unwrap();
        let ws = dir.path();
        write(
            &ws.join("src/a.rs"),
            "pub fn calc(x: u32, y: u32) -> u32 { x + y }\n",
        );
        write(
            &ws.join("src/b.rs"),
            "pub fn calc(a: u32, b: u32) -> u32 { a * b }\n",
        );
        let audit = ArchitectureBrightlineAudit::new(&HashMap::new());
        let findings = audit.analyze(ws).unwrap();
        assert!(
            findings
                .iter()
                .any(|f| f.subject.starts_with(DUPLICATE_SIGNATURE_SUBJECT_PREFIX)
                    && f.subject.contains("calc")),
            "param-name-only difference must still collide: {findings:?}"
        );

        // Same name, different parameter types → no collision.
        let dir2 = TempDir::new().unwrap();
        let ws2 = dir2.path();
        write(
            &ws2.join("src/a.rs"),
            "pub fn calc(x: u32) -> u32 { x + 1 }\n",
        );
        write(
            &ws2.join("src/b.rs"),
            "pub fn calc(x: String) -> usize { x.len() }\n",
        );
        let findings2 = audit.analyze(ws2).unwrap();
        assert!(
            !findings2
                .iter()
                .any(|f| f.subject.starts_with(DUPLICATE_SIGNATURE_SUBJECT_PREFIX)),
            "differing parameter types must not collide: {findings2:?}"
        );
    }
}
