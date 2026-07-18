//! CCX native contracts (U6): the blast-radius postflight for `forge contract run`.
//!
//! Ports `tools/ccx/ccx-blast.py`'s `--diff` mode onto the native tree diff (KTD7):
//! a completed task's agent patch (the `diff_native_content_refs` between the
//! post-dependency baseline and the post-run tree) is classified path-by-path
//! against three layers, in the Python's precedence order (`classify`):
//!
//!   1. the ALWAYS-forbidden [`DEFAULT_FORBID`] list (`.forge/**`, env files, private
//!      keys, credential paths) — NOT weakenable per-contract (R12/AE7);
//!   2. the contract's own `allowed_changes.forbidden_paths` globs;
//!   3. the contract's `allowed_changes.paths` allowlist — anything outside it is a
//!      violation, EXCEPT a declaration-only hunk in a default facade file
//!      ([`DEFAULT_FACADES`]), judged statement-aware exactly like the Python
//!      ([`hunks_decl_only`]).
//!
//! Two native adaptations of the Python (documented, deliberate):
//!
//!   * The snapshot step ([`snapshot_worktree_into_store_excluding`]) drops
//!     `is_ignored_by_policy` paths (`.forge/`, `.env`, `*.pem`, `*credentials*`, …)
//!     BEFORE the tree diff, so the most important default-forbid classes never
//!     appear in `diff`. Those paths are therefore caught by walking the scratch
//!     workspace filesystem directly ([`scan_scratch_default_forbid`]): base and
//!     dependency materialization both enforce the same exclusions, so ANY
//!     policy-excluded file present in the scratch tree was written by the agent.
//!   * A secret-content scan (R16 detect-and-refuse), DIFF-AWARE so it covers only
//!     agent-AUTHORED content: for an ADDED file the whole post-state content is run
//!     through the shared `redact_evidence_excerpt` detector; for a MODIFIED file only
//!     the lines the agent ADDED relative to the pre-agent baseline (a line-set
//!     difference — post lines not present in the baseline blob) are scanned. A
//!     non-empty redaction set is a violation whose patch is refused persistence —
//!     storing redacted bytes would corrupt the R27 integration payload, so the whole
//!     run fails fail-closed instead. Only the PATH is ever surfaced or recorded,
//!     never the detected content. Scanning the whole post-state (the pre-fix
//!     behavior) false-positived when a modified file already carried secret-shaped
//!     fixture strings signed into the base tree, contradicting R16's "agent-authored
//!     content" wording; the diff-aware scan flags only genuinely new secret lines.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result};
use forge_content::{is_ignored_by_policy, DiffLineTag, FileDiff, HunkDiff, TreeDiff};
use forge_content_native::{read_blob_at_path, NativeObjectStore};

use super::contract::glob_match;

/// Always-forbidden path globs, applied regardless of contract or flags (R12).
/// Ported character-for-character from `ccx-blast.py`'s `DEFAULT_FORBID`, including
/// the root-anchored + any-depth (`**/`) twins so a nested `sub/.forge/…` or
/// monorepo `pkg/.env` is forbidden even under a broad allowlist. NOT weakenable
/// per-contract: a contract that explicitly allows `.forge/**` still violates.
pub(crate) const DEFAULT_FORBID: [&str; 14] = [
    ".forge/**",
    "**/.forge/**",
    ".env",
    "**/.env",
    ".env.*",
    "**/.env.*",
    "*.pem",
    "*_rsa",
    "*_ed25519",
    "*.key",
    ".aws/**",
    "**/.aws/**",
    ".ssh/**",
    "**/.ssh/**",
];

// `*credentials*` is applied separately: a bare `*credentials*` glob (no `/`) must
// match at ANY depth, but the ported `glob_match` normalizes `**`→`*` and `*`
// already crosses `/`, so `*credentials*` matches `a/b/credentials.txt`. Kept in the
// same conceptual list as the Python's final `*credentials*` entry.
const DEFAULT_FORBID_CREDENTIALS: &str = "*credentials*";

/// Facade files (docs/adr/0001-domain-modules.md): declaration/re-export-only hunks
/// are permitted here even outside the contract allowlist. Ported from
/// `ccx-blast.py`'s `DEFAULT_FACADES`.
pub(crate) const DEFAULT_FACADES: [&str; 2] = [
    "crates/forge-store/src/lib.rs",
    "crates/forge-cli/src/main.rs",
];

/// Upper bound on the post-state bytes scanned for secret-like content per file
/// (1 MiB). A larger file is scanned only over its first `SECRET_SCAN_LIMIT` bytes
/// (documented bound); this keeps the postflight from loading an unbounded blob into
/// memory while still covering the realistic secret-in-source case.
const SECRET_SCAN_LIMIT: usize = 1024 * 1024;

/// Directories never descended when walking the scratch tree for default-forbid
/// hits — heavy, irrelevant to the blast concern, and never carriers of the
/// policy-excluded classes we look for. `.forge` is DELIBERATELY absent so its
/// agent-written contents are found.
const WALK_SKIP_DIRS: [&str; 3] = [".git", "target", "node_modules"];

/// The class of a single blast violation. Both map to exit 3 and a `blast` verdict;
/// the distinction drives the verdict detail and the envelope so an operator can tell
/// a forbidden-path escape from a secret-in-content refusal apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlastViolationClass {
    /// The path is on the default-forbid list, the contract's forbidden_paths, or
    /// outside the allowlist (`ccx-blast.py`'s `classify` kinds).
    ForbiddenPath,
    /// The path is inside the blast radius, but its post-state content tripped the
    /// secret detector (R16). The offending content is never recorded.
    SecretContent,
}

impl BlastViolationClass {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            BlastViolationClass::ForbiddenPath => "forbidden_path",
            BlastViolationClass::SecretContent => "secret_content",
        }
    }
}

/// One blast violation. `detail` names the offending path (and, for a forbidden
/// path, the `classify` kind) — NEVER any file content (R16).
#[derive(Debug, Clone)]
pub(crate) struct BlastViolation {
    pub(crate) path: String,
    pub(crate) class: BlastViolationClass,
    pub(crate) detail: String,
}

/// The result of the blast postflight for one completed task.
#[derive(Debug, Clone, Default)]
pub(crate) struct BlastOutcome {
    pub(crate) violations: Vec<BlastViolation>,
    /// Facade paths that were outside the allowlist but permitted because every
    /// changed line was declaration-only (surfaced for transparency, like the
    /// Python's `facade_allowed`).
    pub(crate) facade_allowed: Vec<String>,
}

impl BlastOutcome {
    pub(crate) fn has_violation(&self) -> bool {
        !self.violations.is_empty()
    }
}

/// True if `path` matches any default-forbid glob (including the `*credentials*`
/// suffix form). Not weakenable — checked before any contract allow/forbid (R12).
fn is_default_forbidden(path: &str) -> bool {
    DEFAULT_FORBID.iter().any(|glob| glob_match(path, glob))
        || glob_match(path, DEFAULT_FORBID_CREDENTIALS)
}

/// Shared allow/forbid classification (`ccx-blast.py`'s `classify`): returns a
/// violation-kind label or `None` when the path is inside the blast radius. Default
/// forbid wins over contract forbid, which wins over the allowlist.
fn classify_path(path: &str, allow: &[String], forbid: &[String]) -> Option<&'static str> {
    if is_default_forbidden(path) {
        return Some("default_forbidden");
    }
    if forbid.iter().any(|glob| glob_match(path, glob)) {
        return Some("forbidden");
    }
    if !allow.iter().any(|glob| glob_match(path, glob)) {
        return Some("outside allowlist");
    }
    None
}

// ---------------------------------------------------------------------------
// Statement-aware facade allowance (port of ccx-blast.py's decl-only logic)
// ---------------------------------------------------------------------------

/// Characters that may legitimately appear inside a `use`/`mod` declaration path or
/// group. Deliberately EXCLUDES `(` `)` `=` `!` `<` `>` `"` `'` `.` `|` `&` `#` `/`
/// so any executable code smuggled onto a facade line (a macro call, a fn/const/
/// static item, a closure) contains an out-of-set character and is rejected. Ported
/// from `ccx-blast.py`'s `SAFE_DECL_CHARS`.
fn is_safe_decl_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, ' ' | '\t' | '_' | ':' | ',' | '{' | '}' | '*' | ';')
}

/// Start of a (possibly multi-line) `use`/`mod` declaration: `mod x;`, `pub mod x;`,
/// `use …;`, `pub use …;`, `pub(crate) use …;`. Mirrors `STMT_RE`.
fn starts_declaration(rest: &str) -> bool {
    let after_vis = strip_visibility(rest);
    starts_keyword(after_vis, "use") || starts_keyword(after_vis, "mod")
}

/// True if `rest` (after any visibility prefix) begins a `mod` declaration. Mirrors
/// `MOD_RE`.
fn starts_mod(rest: &str) -> bool {
    starts_keyword(strip_visibility(rest), "mod")
}

/// Strip an optional leading `pub` / `pub(...)` visibility prefix and following
/// whitespace, returning the remainder.
fn strip_visibility(rest: &str) -> &str {
    let Some(after_pub) = rest.strip_prefix("pub") else {
        return rest;
    };
    // Optional `(...)` restriction immediately after `pub`.
    let after_group = match after_pub.strip_prefix('(') {
        Some(inner) => match inner.find(')') {
            Some(idx) => &inner[idx + 1..],
            None => after_pub, // unbalanced; let the char scan reject it
        },
        None => after_pub,
    };
    // `pub` must be followed by whitespace (or the group) to count as a prefix; if
    // not (e.g. `public_fn`), treat the whole token as non-visibility.
    if after_group.starts_with([' ', '\t']) || after_pub.starts_with('(') {
        after_group.trim_start_matches([' ', '\t'])
    } else {
        rest
    }
}

/// True if `rest` begins with `keyword` followed by a word boundary (`\b`).
fn starts_keyword(rest: &str, keyword: &str) -> bool {
    let Some(after) = rest.strip_prefix(keyword) else {
        return false;
    };
    after
        .chars()
        .next()
        .map(|c| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(true)
}

/// Consume one line as `use`/`mod` declaration text, updating `(in_stmt, depth)`
/// carried across the lines of one diff side (so a multi-line `pub use foo::{ … };`
/// group is tracked). In `strict` mode (CHANGED lines) every consumed character must
/// be safe, a brace-form `mod x { … }` is refused, and code after a statement's `;`
/// must itself open another declaration. In lenient mode (context lines) the scan
/// only advances state and never rejects. Port of `_scan_decl_line`.
fn scan_decl_line(code: &str, in_stmt: &mut bool, depth: &mut i64, strict: bool) -> bool {
    let chars: Vec<char> = code.chars().collect();
    let n = chars.len();
    let mut i = 0;
    while i < n {
        while i < n && matches!(chars[i], ' ' | '\t') {
            i += 1;
        }
        if i >= n {
            break;
        }
        if chars[i] == '/' && chars.get(i + 1) == Some(&'/') {
            break;
        }
        if !*in_stmt {
            let rest: String = chars[i..].iter().collect();
            if !starts_declaration(&rest) {
                return !strict;
            }
            if strict && starts_mod(&rest) && rest.contains('{') {
                return false; // brace-form module body, not a bare `mod x;`
            }
            *in_stmt = true;
            *depth = 0;
        }
        let mut terminated = false;
        while i < n {
            let c = chars[i];
            if strict && !is_safe_decl_char(c) {
                return false;
            }
            match c {
                '{' => *depth += 1,
                '}' => *depth -= 1,
                ';' if *depth <= 0 => {
                    *in_stmt = false;
                    *depth = 0;
                    i += 1;
                    terminated = true;
                    break;
                }
                _ => {}
            }
            i += 1;
        }
        if !terminated {
            break; // line ended mid-statement; span continues on next line
        }
    }
    true
}

/// True iff every changed line in every hunk is declaration-only. Spans are
/// reconstructed separately for the old side (context + removed) and new side
/// (context + added), so an opener appearing only as context still licenses
/// added/removed continuation lines inside it. Port of `hunks_decl_only`.
///
/// Native adaptation: the hunk lines come from the structured `HunkDiff` (context =
/// [`DiffLineTag::Context`], removed = `Delete`, added = `Insert`); the Python's
/// `(tag, body)` pairs map onto these one-for-one, and the `content` field is the
/// line WITHOUT the diff prefix, exactly like the Python's `body = raw[1:]`.
fn hunks_decl_only(hunks: &[HunkDiff]) -> bool {
    for hunk in hunks {
        for keep in [DiffLineTag::Delete, DiffLineTag::Insert] {
            let mut in_stmt = false;
            let mut depth = 0i64;
            for line in &hunk.lines {
                if line.tag != DiffLineTag::Context && line.tag != keep {
                    continue;
                }
                let changed = line.tag == keep;
                let stripped = line.content.trim();
                if stripped.is_empty()
                    || stripped.starts_with("//")
                    || stripped.starts_with("#[")
                    || stripped.starts_with("#![")
                {
                    continue; // blank / comment / attribute; state unchanged
                }
                let code = line.content.split("//").next().unwrap_or("").trim_end();
                let ok = scan_decl_line(code, &mut in_stmt, &mut depth, changed);
                if changed && !ok {
                    return false;
                }
            }
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Postflight entry point
// ---------------------------------------------------------------------------

/// Evaluate the blast-radius postflight for one completed task (R12/R16/AE7).
///
/// * `diff` — the agent patch (post-dependency baseline → post-run tree).
/// * `allow` / `forbid` — the contract's `allowed_changes.paths` /
///   `.forbidden_paths` globs.
/// * `scratch_root` — the post-run scratch workspace, read for (a) default-forbid
///   paths the snapshot dropped and (b) added/modified post-state content scanned
///   for secrets.
/// * `store` / `baseline_ref` — the native object store and the pre-agent baseline
///   content ref (the `a`-side of the run's `diff_native_content_refs(baseline,
///   post)`), used to read a MODIFIED file's baseline blob so the secret-content scan
///   covers only agent-ADDED lines (diff-aware R16).
pub(crate) fn evaluate_blast(
    diff: &TreeDiff,
    allow: &[String],
    forbid: &[String],
    scratch_root: &Path,
    store: &NativeObjectStore,
    baseline_ref: &str,
) -> Result<BlastOutcome> {
    let mut outcome = BlastOutcome::default();
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    // Layer 1+2+3: path classification over the diff (non-excluded changed paths),
    // with the facade decl-only allowance for outside-allowlist facade files.
    for file in &diff.files {
        seen.insert(file.path.clone());
        let Some(kind) = classify_path(&file.path, allow, forbid) else {
            continue;
        };
        if kind == "outside allowlist"
            && DEFAULT_FACADES.contains(&file.path.as_str())
            && hunks_decl_only(&file.hunks)
        {
            outcome.facade_allowed.push(file.path.clone());
            continue;
        }
        outcome.violations.push(BlastViolation {
            path: file.path.clone(),
            class: BlastViolationClass::ForbiddenPath,
            detail: format!("{kind}: {}", file.path),
        });
    }

    // Policy-excluded classes the snapshot dropped from the diff (`.forge/**`,
    // `.env`, keys, `*credentials*`, and the broader secret-risk names like `id_dsa`,
    // `*.p12`, `*secret*`): find them by walking the scratch tree, fail-closed. The
    // detail distinguishes the narrow default-forbid subset from the broader
    // secret-risk-only case so an operator can tell them apart; both are
    // ForbiddenPath-class (exit 3).
    for path in scan_scratch_default_forbid(scratch_root)? {
        if seen.insert(path.clone()) {
            let detail = if is_default_forbidden(&path) {
                format!("default_forbidden: {path}")
            } else {
                format!("policy_excluded: {path}")
            };
            outcome.violations.push(BlastViolation {
                path: path.clone(),
                class: BlastViolationClass::ForbiddenPath,
                detail,
            });
        }
    }

    // Secret-content scan (R16), DIFF-AWARE: for an ADDED file the whole post-state
    // content; for a MODIFIED file only the lines the agent added relative to the
    // pre-agent baseline. Deletions are skipped; excluded files are already forbidden
    // above and their content is never persisted, so they need no separate scan.
    for file in &diff.files {
        if file.status.starts_with('D') {
            continue; // deletion has no post-state content
        }
        if classify_path(&file.path, allow, forbid).is_some() {
            continue; // already a path violation; do not double-report
        }
        if scan_file_for_secret(scratch_root, file, store, baseline_ref)? {
            outcome.violations.push(BlastViolation {
                path: file.path.clone(),
                class: BlastViolationClass::SecretContent,
                detail: format!("secret-content detected in {}", file.path),
            });
        }
    }

    Ok(outcome)
}

/// Scan a changed file's agent-AUTHORED content and return `true` if the shared
/// secret detector fires. Diff-aware (R16): for a MODIFIED file only the lines the
/// agent ADDED relative to the pre-agent baseline are scanned; for an ADDED file the
/// whole post-state content is scanned (unchanged behavior).
///
/// The post-state is read from the scratch workspace; the baseline blob is read from
/// the store via [`read_blob_at_path`]. Both sides are bounded at
/// [`SECRET_SCAN_LIMIT`]; non-UTF-8 (binary) content is skipped (documented choice —
/// the redactor operates on text and a binary blob is not a secret-assignment
/// carrier). NEVER returns or records the content itself.
///
/// Precision note: the added set is a simple LINE-SET difference (post lines not
/// present in the baseline). A pre-existing fixture line that the agent MOVED is
/// therefore not re-flagged (its text still exists in the baseline set — correct),
/// and a pre-existing line the agent DUPLICATED is likewise not re-flagged
/// (acceptable: the content already exists in the signed base tree). Only genuinely
/// new secret-bearing lines trigger. This is deliberately not an LCS diff; erring
/// toward treating a line as pre-existing (never toward hiding a new secret line) is
/// the safe direction because a truly new line can never coincide with a baseline
/// line's exact bytes without the secret already being in the base.
fn scan_file_for_secret(
    scratch_root: &Path,
    file: &FileDiff,
    store: &NativeObjectStore,
    baseline_ref: &str,
) -> Result<bool> {
    let path = scratch_root.join(&file.path);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        // A path in the diff that is unreadable in the scratch tree (e.g. a rename
        // edge) is not a secret carrier; skip rather than fail the whole postflight.
        Err(_) => return Ok(false),
    };
    let window = &bytes[..bytes.len().min(SECRET_SCAN_LIMIT)];
    let Ok(post_text) = std::str::from_utf8(window) else {
        return Ok(false); // binary / non-UTF-8: skipped with intent
    };

    // ADDED file (no baseline content): scan the whole post-state. `status` uses git's
    // name-status letters (`A`/`M`/`D`, `R<score>`); anything that is not a plain
    // modification is treated as fully agent-authored (scan whole), erring toward
    // scanning more.
    let baseline_bytes = if file.status.starts_with('M') {
        read_blob_at_path(store, baseline_ref, &file.path)?
    } else {
        None
    };
    let Some(baseline_bytes) = baseline_bytes else {
        let (_redacted, kinds) = forge_content::redact_evidence_excerpt(post_text);
        return Ok(!kinds.is_empty());
    };

    // MODIFIED file: scan only the lines present in post but not in the baseline. The
    // baseline is bounded and non-UTF-8-skipped exactly like the post side; a
    // non-UTF-8 baseline (binary→text edit) collapses to an empty pre-existing set, so
    // the whole post-state is scanned — the fail-toward-scanning-more direction.
    let baseline_window = &baseline_bytes[..baseline_bytes.len().min(SECRET_SCAN_LIMIT)];
    let baseline_lines: BTreeSet<&str> = match std::str::from_utf8(baseline_window) {
        Ok(text) => text.lines().collect(),
        Err(_) => BTreeSet::new(),
    };
    let added: String = post_text
        .lines()
        .filter(|line| !baseline_lines.contains(line))
        .collect::<Vec<_>>()
        .join("\n");
    if added.is_empty() {
        return Ok(false);
    }
    let (_redacted, kinds) = forge_content::redact_evidence_excerpt(&added);
    Ok(!kinds.is_empty())
}

/// Walk the scratch workspace and return the relative paths (with `/` separators)
/// of EVERY file that is `is_ignored_by_policy` — fail-closed. This is broader than
/// the [`DEFAULT_FORBID`] list on purpose: `is_ignored_by_policy` (via
/// `is_secret_risk_path`) also covers secret-risk names that are NOT default-forbid
/// globs (`id_dsa`, `*.p12`, `*.pfx`, `*secret*`, singular `*credential*`, …). Those
/// paths are dropped from the snapshot before the tree diff, so if we only flagged the
/// narrower default-forbid subset an agent could plant/exfiltrate such a file
/// invisibly (it would be excluded from the snapshot AND never reported).
///
/// Flagging every policy-excluded file found here is safe because the scratch tree is
/// materialized ONLY via `materialize_tree` (forge-content-native), which writes just
/// the base+dependency TREE content and `continue`s on every `is_ignored_by_policy`
/// entry — it never writes `.forge` or the `WORKSPACE_MARKER_FILE`. The base and
/// dependency snapshots that produced that content were themselves taken with
/// `is_ignored_by_policy` paths excluded. Therefore ANY `is_ignored_by_policy` file
/// present in the scratch tree was written by the agent (KTD7) → a real blast
/// violation. The walk skips `.git`/`target`/`node_modules` via [`WALK_SKIP_DIRS`].
///
/// The caller distinguishes the default-forbid subset (`default_forbidden:` detail)
/// from the broader secret-risk-only case (`policy_excluded:` detail) via
/// [`is_default_forbidden`] on each returned path.
fn scan_scratch_default_forbid(scratch_root: &Path) -> Result<Vec<String>> {
    let mut hits = Vec::new();
    let mut stack = vec![scratch_root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let file_type = entry
                .file_type()
                .with_context(|| format!("stat scratch entry {}", entry.path().display()))?;
            if file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            if file_type.is_dir() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if WALK_SKIP_DIRS.contains(&name.as_ref()) {
                    continue;
                }
                stack.push(path);
            } else if file_type.is_file() {
                if let Ok(rel) = path.strip_prefix(scratch_root) {
                    let rel = rel.to_string_lossy().replace('\\', "/");
                    if is_ignored_by_policy(&rel) {
                        hits.push(rel);
                    }
                }
            }
        }
    }
    hits.sort();
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_content::DiffLine;

    fn hunk(lines: &[(DiffLineTag, &str)]) -> HunkDiff {
        HunkDiff {
            old_start: 1,
            old_lines: 1,
            new_start: 1,
            new_lines: 1,
            lines: lines
                .iter()
                .map(|(tag, content)| DiffLine {
                    tag: *tag,
                    content: (*content).to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn default_forbid_covers_forge_env_keys_credentials() {
        assert!(is_default_forbidden(".forge/forge.db"));
        assert!(is_default_forbidden("sub/.forge/x"));
        assert!(is_default_forbidden(".env"));
        assert!(is_default_forbidden("pkg/.env.local"));
        assert!(is_default_forbidden("deploy/server.pem"));
        assert!(is_default_forbidden("secrets/id_rsa"));
        assert!(is_default_forbidden("a/b/my_credentials.txt"));
        assert!(!is_default_forbidden("crates/forge-core/src/lib.rs"));
    }

    #[test]
    fn facade_decl_only_change_is_allowed() {
        // A bare `mod` re-export line change is declaration-only.
        let hunks = vec![hunk(&[
            (DiffLineTag::Context, "pub mod alpha;"),
            (DiffLineTag::Insert, "pub mod beta;"),
        ])];
        assert!(hunks_decl_only(&hunks));
    }

    #[test]
    fn facade_multiline_use_group_is_allowed() {
        let hunks = vec![hunk(&[
            (DiffLineTag::Context, "pub use foo::{"),
            (DiffLineTag::Insert, "    bar,"),
            (DiffLineTag::Context, "};"),
        ])];
        assert!(hunks_decl_only(&hunks));
    }

    #[test]
    fn facade_executable_change_is_rejected() {
        // A fn item smuggled onto a facade line is not declaration-only.
        let hunks = vec![hunk(&[(DiffLineTag::Insert, "fn sneaky() { evil(); }")])];
        assert!(!hunks_decl_only(&hunks));
        // A macro call likewise contains out-of-set characters.
        let macro_hunk = vec![hunk(&[(DiffLineTag::Insert, "include!(\"x.rs\");")])];
        assert!(!hunks_decl_only(&macro_hunk));
    }

    #[test]
    fn classify_precedence_default_forbid_wins() {
        // Even with an allow glob that would cover `.forge/**`, default-forbid wins.
        let allow = vec![".forge/**".to_string(), "**".to_string()];
        assert_eq!(
            classify_path(".forge/x", &allow, &[]),
            Some("default_forbidden")
        );
        assert_eq!(
            classify_path("src/main.rs", &["src/**".to_string()], &[]),
            None
        );
        assert_eq!(
            classify_path("docs/x.md", &["src/**".to_string()], &[]),
            Some("outside allowlist")
        );
    }

    #[test]
    fn scan_flags_secret_risk_name_not_on_default_forbid() {
        // `id_dsa` is `is_ignored_by_policy` (via is_secret_risk_path) but is NOT on the
        // narrower DEFAULT_FORBID list. Fail-closed: an agent-written `id_dsa` in the
        // scratch tree must still be flagged, or it could be planted/exfiltrated
        // invisibly (the snapshot drops it AND the old conjunction never reported it).
        assert!(!is_default_forbidden("id_dsa"));
        assert!(is_ignored_by_policy("id_dsa"));

        let scratch = tempfile::tempdir().unwrap();
        std::fs::write(
            scratch.path().join("id_dsa"),
            b"-----BEGIN PRIVATE KEY-----\n",
        )
        .unwrap();
        // A benign file in the same tree must NOT be flagged.
        std::fs::write(scratch.path().join("main.rs"), b"fn main() {}\n").unwrap();

        let hits = scan_scratch_default_forbid(scratch.path()).unwrap();
        assert_eq!(hits, vec!["id_dsa".to_string()]);
    }

    #[test]
    fn evaluate_blast_reports_secret_risk_scratch_file_as_policy_excluded() {
        // End-to-end: an empty diff plus a scratch tree carrying an agent-written
        // `id_dsa` still produces a ForbiddenPath violation, tagged `policy_excluded:`
        // (not `default_forbidden:`, since `id_dsa` is not on DEFAULT_FORBID).
        let scratch = tempfile::tempdir().unwrap();
        std::fs::write(scratch.path().join("id_dsa"), b"key bytes\n").unwrap();

        let diff = TreeDiff {
            files: Vec::new(),
            dropped_secret_paths: Vec::new(),
            warnings: Vec::new(),
        };
        // The diff is empty, so the diff-aware secret scan never reads the baseline;
        // a placeholder store/ref suffices for this path-only assertion.
        let store = NativeObjectStore::new(scratch.path());
        let outcome = evaluate_blast(&diff, &[], &[], scratch.path(), &store, "").unwrap();

        assert!(outcome.has_violation());
        let violation = outcome
            .violations
            .iter()
            .find(|v| v.path == "id_dsa")
            .expect("id_dsa must be flagged");
        assert_eq!(violation.class, BlastViolationClass::ForbiddenPath);
        assert_eq!(violation.detail, "policy_excluded: id_dsa");
    }
}
