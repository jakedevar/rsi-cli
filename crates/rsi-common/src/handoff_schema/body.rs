//! Body section scanner for handoff documents.
//!
//! A handoff body is a sequence of `## Heading` sections; each section runs
//! from its header line up to the next `## ` line at column 0 (or EOF).
//! `scan_sections()` walks the body once and returns a map keyed by the
//! heading text (stripped of the `## ` prefix and trimmed).

use std::collections::BTreeMap;

/// One body section. `body_text` excludes the heading line itself.
#[derive(Debug, Clone)]
pub struct Section {
    pub heading: String,
    pub line_range: (usize, usize),
    pub body_text: String,
}

/// Scan a markdown body for `## Heading` sections (h2 only, column 0).
///
/// Returns a `BTreeMap` keyed by the trimmed heading text. Duplicate
/// headings are coalesced to the first occurrence (later sections with the
/// same heading are silently ignored — handoffs MUST NOT have duplicate
/// headings, and validation surfaces that elsewhere via presence checks
/// against the canonical name).
pub fn scan_sections(body: &str) -> BTreeMap<String, Section> {
    let mut sections: BTreeMap<String, Section> = BTreeMap::new();
    let lines: Vec<&str> = body.lines().collect();
    let mut idx = 0;
    while idx < lines.len() {
        let line = lines[idx];
        if let Some(heading) = strip_h2(line) {
            // Find the end of this section (next ## at col 0 or EOF).
            let start = idx;
            let mut end = lines.len();
            for j in (idx + 1)..lines.len() {
                if strip_h2(lines[j]).is_some() {
                    end = j;
                    break;
                }
            }
            let body_text = if start + 1 >= end {
                String::new()
            } else {
                lines[(start + 1)..end].join("\n")
            };
            sections.entry(heading.to_string()).or_insert(Section {
                heading: heading.to_string(),
                line_range: (start, end.saturating_sub(1)),
                body_text,
            });
            idx = end;
        } else {
            idx += 1;
        }
    }
    sections
}

/// Return the trimmed heading text if `line` is a column-0 `## ` heading.
fn strip_h2(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("## ")?;
    // Reject `### ` and deeper — strip_prefix already handled the column-0
    // check (no leading whitespace permitted).
    Some(rest.trim_end())
}

/// Return the trimmed heading text if `line` is a column-0 `### ` heading.
/// The `## `-prefixed `strip_h2` rejects `### ` lines (they do not start with
/// `## `), so h2 and h3 scanning never collide.
fn strip_h3(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("### ")?;
    Some(rest.trim_end())
}

/// The canonical h2 heading that introduces a stage-contract block.
///
/// The block is an OPTIONAL, additive section documented in the per-kind
/// worker preambles (`.claude/commands/_shared/worker_preamble_{kind}.md`).
/// It is NOT part of the on-disk handoff v1 schema — historical handoffs
/// carry no such section and must keep validating. `validate()` therefore
/// does NOT require this heading; [`scan_contract_block`] is a standalone
/// validator invoked only on contract-bearing documents.
pub const STAGE_CONTRACT_HEADING: &str = "Stage contract";

/// The four required sub-sections of a stage-contract block, in canonical
/// order. Matched against `### ` sub-headings within the `## Stage contract`
/// section body.
pub const CONTRACT_SUBSECTIONS: [&str; 4] = ["Inputs", "Process", "Outputs", "Verify"];

/// Outcome of scanning a stage-contract block. Distinguishes "no block
/// present" (fine — the block is optional) from "block present but
/// malformed" (a set of concrete problems).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContractBlock {
    /// No `## Stage contract` section in the body. Not an error: the block
    /// is additive and absent from every historical handoff.
    Absent,
    /// A `## Stage contract` section with all four sub-sections present and
    /// an Inputs sub-section that declares static inputs or a discovery
    /// budget (or both).
    Valid,
    /// A `## Stage contract` section that is present but violates the shape.
    /// `problems` is the non-empty list of specific failures.
    Malformed { problems: Vec<ContractProblem> },
}

/// A single structural failure of a stage-contract block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContractProblem {
    /// A required sub-section (`### Inputs` / `### Process` / `### Outputs`
    /// / `### Verify`) was absent.
    MissingSubsection(&'static str),
    /// The `### Inputs` sub-section declared NEITHER a static input (a
    /// `path/to/file`-style reference or a bulleted named artifact) NOR an
    /// explicit discovery budget (a "discovery budget" / "code-discovery"
    /// label or a backticked `` `rg` ``/`` `grep` ``/`` `glob` ``/`` `ripgrep` ``
    /// tool token). Coding inputs are discovered, so a discovery-budget-only
    /// declaration is accepted; declaring nothing is the failure.
    InputsDeclareNothing,
}

/// Scan a handoff/preamble body for an OPTIONAL stage-contract block and
/// validate its shape when present.
///
/// The block is a `## Stage contract` h2 section whose body carries four
/// `### `-level sub-sections — `Inputs`, `Process`, `Outputs`, `Verify` —
/// in any order. Its contract:
///
/// 1. All four sub-sections MUST be present.
/// 2. The `### Inputs` sub-section MUST declare EITHER named static inputs
///    (file/artifact references) OR an explicit code-discovery budget
///    (grep/glob allowance) — or both. Declaring only a discovery budget is
///    ACCEPTED (coding inputs are discovered, not over-constrained to a
///    fixed file list).
///
/// Returns [`ContractBlock::Absent`] when no `## Stage contract` section
/// exists — callers treat that as "not a contract-bearing document", NOT a
/// failure. This keeps every pre-existing handoff (which has no such block)
/// valid: `validate()` never calls this, and the on-disk v1 schema does not
/// require the block.
#[must_use]
pub fn scan_contract_block(body: &str) -> ContractBlock {
    let sections = scan_sections(body);
    let Some(block) = sections.get(STAGE_CONTRACT_HEADING) else {
        return ContractBlock::Absent;
    };

    let subs = scan_h3_subsections(&block.body_text);
    let mut problems: Vec<ContractProblem> = Vec::new();

    for required in CONTRACT_SUBSECTIONS {
        if !subs.contains_key(required) {
            problems.push(ContractProblem::MissingSubsection(required));
        }
    }

    // Inputs must declare static inputs OR a discovery budget. Only check
    // when the sub-section exists (a missing Inputs is already flagged
    // above; don't double-report it as "declares nothing").
    if let Some(inputs_body) = subs.get(CONTRACT_SUBSECTIONS[0])
        && !inputs_declares_static(inputs_body)
        && !inputs_declares_discovery_budget(inputs_body)
    {
        problems.push(ContractProblem::InputsDeclareNothing);
    }

    if problems.is_empty() {
        ContractBlock::Valid
    } else {
        ContractBlock::Malformed { problems }
    }
}

/// Scan a section body for `### Heading` sub-sections, returning a map from
/// trimmed heading text to that sub-section's body text (excluding the
/// heading line). Mirrors [`scan_sections`] one level down.
fn scan_h3_subsections(body: &str) -> BTreeMap<String, String> {
    let mut subs: BTreeMap<String, String> = BTreeMap::new();
    let lines: Vec<&str> = body.lines().collect();
    let mut idx = 0;
    while idx < lines.len() {
        if let Some(heading) = strip_h3(lines[idx]) {
            let start = idx;
            let mut end = lines.len();
            for j in (idx + 1)..lines.len() {
                if strip_h3(lines[j]).is_some() {
                    end = j;
                    break;
                }
            }
            let sub_body = if start + 1 >= end {
                String::new()
            } else {
                lines[(start + 1)..end].join("\n")
            };
            subs.entry(heading.to_string()).or_insert(sub_body);
            idx = end;
        } else {
            idx += 1;
        }
    }
    subs
}

/// Does the Inputs sub-section name at least one static input? A static
/// input is a concrete file/artifact reference: a bullet mentioning a path
/// separator (`/`) or a backticked token, or an explicit `static inputs:` /
/// `inputs:` label followed by content. Kept deliberately permissive — the
/// point is to distinguish "declared something concrete" from "declared
/// nothing at all", not to parse a strict grammar.
fn inputs_declares_static(inputs_body: &str) -> bool {
    let lower = inputs_body.to_ascii_lowercase();
    // A path-ish token (contains `/`) or a backticked artifact anywhere in
    // the body signals a named static input.
    if inputs_body.contains('`') {
        return true;
    }
    for line in inputs_body.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        // A bullet or label that references a path separator.
        if t.contains('/') {
            return true;
        }
    }
    // An explicit "static input(s):" label with any trailing content.
    if let Some(rest) = label_value(&lower, "static input")
        && !rest.is_empty()
    {
        return true;
    }
    false
}

/// Does the Inputs sub-section declare an explicit code-discovery budget?
/// Recognizes the `discovery budget` / `code-discovery` labels or a BACKTICKED
/// discovery-tool token (`` `rg` ``, `` `grep` ``, `` `glob` ``, `` `ripgrep` ``)
/// — the grep/glob allowance that lets a coding stage discover its own inputs.
/// This is the "don't over-constrain to a fixed file list" escape hatch.
///
/// Markers are deliberately precise (explicit phrases + backticked tokens) so
/// incidental prose does NOT count: bare words like "large", "global", or
/// "find the seam" contain "rg"/"glob"/"find" as substrings but declare no
/// budget, and must not be accepted on their own.
fn inputs_declares_discovery_budget(inputs_body: &str) -> bool {
    const DISCOVERY_MARKERS: [&str; 6] = [
        "discovery budget",
        "code-discovery",
        "`rg`",
        "`grep`",
        "`glob`",
        "`ripgrep`",
    ];
    let lower = inputs_body.to_ascii_lowercase();
    DISCOVERY_MARKERS.iter().any(|m| lower.contains(m))
}

/// Extract the trailing value after a `label:` occurrence in `haystack`
/// (already lowercased by the caller when case-insensitive). Returns the
/// trimmed remainder of the line following the first `label:` match, or
/// `None` if the label is absent.
fn label_value<'a>(haystack: &'a str, label: &str) -> Option<&'a str> {
    let pos = haystack.find(label)?;
    let after = &haystack[pos + label.len()..];
    // Skip an optional `(s)` plural and the colon.
    let after = after.trim_start_matches("(s)").trim_start();
    let after = after.strip_prefix(':').unwrap_or(after);
    // Value ends at the line boundary.
    let line_end = after.find('\n').unwrap_or(after.len());
    Some(after[..line_end].trim())
}

/// Count unicode words in a string (whitespace-separated, non-empty tokens).
/// Used by the WordCap rule.
pub fn word_count(text: &str) -> usize {
    text.split_whitespace().filter(|t| !t.is_empty()).count()
}

/// Count top-level bullet items (`^- ` lines, ignoring nested bullets).
/// Used by the ItemCap rule.
pub fn bullet_count(text: &str) -> usize {
    text.lines()
        .filter(|line| line.starts_with("- ") || line.starts_with("* "))
        .count()
}

/// Iterate over top-level bullets. Strips the leading `- ` / `* ` marker.
pub fn iter_bullets(text: &str) -> impl Iterator<Item = &str> {
    text.lines()
        .filter_map(|line| line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_sections_finds_all_h2() {
        let body = "## A\n\nalpha\n\n## B\n\nbeta\n\n## C\n\ngamma\n";
        let sections = scan_sections(body);
        assert_eq!(sections.len(), 3);
        assert!(sections.contains_key("A"));
        assert!(sections.contains_key("B"));
        assert!(sections.contains_key("C"));
    }

    #[test]
    fn scan_sections_handles_eof_section() {
        let body = "## Last\n\nfinal text without trailing heading";
        let sections = scan_sections(body);
        assert_eq!(sections.len(), 1);
        let last = sections.get("Last").unwrap();
        assert!(last.body_text.contains("final text"));
    }

    #[test]
    fn scan_sections_ignores_h3() {
        let body = "## Top\n\n### Nested\n\nbody\n\n## Next\n";
        let sections = scan_sections(body);
        assert_eq!(sections.len(), 2, "h3 must not register as a section");
    }

    #[test]
    fn scan_sections_ignores_indented_h2() {
        let body = "## Real\n\n  ## NotASection\n\n## End\n";
        let sections = scan_sections(body);
        assert_eq!(sections.len(), 2);
    }

    #[test]
    fn word_count_basic() {
        assert_eq!(word_count("hello world"), 2);
        assert_eq!(word_count("  one   two\nthree\n"), 3);
        assert_eq!(word_count(""), 0);
    }

    #[test]
    fn bullet_count_basic() {
        let body = "- one\n- two\n  - nested (skip)\n- three\n";
        assert_eq!(bullet_count(body), 3);
    }

    #[test]
    fn iter_bullets_strips_marker() {
        let body = "- foo\n- bar\n* baz\n";
        let items: Vec<&str> = iter_bullets(body).collect();
        assert_eq!(items, vec!["foo", "bar", "baz"]);
    }

    // --- Stage-contract block (S7) ---
    //
    // The four assertions below are the S7 pinning tests:
    //   (a) contract_block_static_plus_discovery_is_valid
    //   (b) contract_block_missing_outputs_is_flagged
    //   (c) contract_block_declaring_nothing_is_flagged
    //   (d) contract_block_discovery_budget_only_is_accepted
    // (d) is the "don't over-constrain to a fixed file list" pin.

    /// A well-formed block: static inputs + a discovery budget + all four
    /// sub-sections. Wrapped in a `## Stage contract` h2, as the preambles
    /// emit it.
    fn contract_block_full() -> &'static str {
        "## Stage contract\n\
         \n\
         ### Inputs\n\
         \n\
         - Static: `crates/rsi-common/src/handoff_schema/body.rs`\n\
         - Discovery budget: grep/glob across `crates/` to locate call sites.\n\
         \n\
         ### Process\n\
         \n\
         - Read the target, make the change, add tests.\n\
         \n\
         ### Outputs\n\
         \n\
         - A committed diff plus green tests.\n\
         \n\
         ### Verify\n\
         \n\
         - cargo test -p rsi-common passes.\n"
    }

    /// (a) PINNING: well-formed block (static inputs + discovery budget)
    /// scans clean.
    #[test]
    fn contract_block_static_plus_discovery_is_valid() {
        let result = scan_contract_block(contract_block_full());
        assert_eq!(
            result,
            ContractBlock::Valid,
            "well-formed contract block must be Valid, got: {result:?}"
        );
    }

    /// (b) PINNING: a block missing the Outputs sub-section is flagged.
    #[test]
    fn contract_block_missing_outputs_is_flagged() {
        let body = "## Stage contract\n\
             \n\
             ### Inputs\n\
             \n\
             - Static: `foo/bar.rs`\n\
             - Discovery budget: grep across the crate.\n\
             \n\
             ### Process\n\
             \n\
             - Do the work.\n\
             \n\
             ### Verify\n\
             \n\
             - Tests pass.\n";
        let result = scan_contract_block(body);
        match result {
            ContractBlock::Malformed { problems } => {
                assert!(
                    problems.contains(&ContractProblem::MissingSubsection("Outputs")),
                    "expected MissingSubsection(Outputs), got: {problems:?}"
                );
            }
            other => panic!("expected Malformed for missing Outputs, got: {other:?}"),
        }
    }

    /// (c) PINNING: a block whose Inputs declares NEITHER static inputs NOR
    /// a discovery budget is flagged.
    #[test]
    fn contract_block_declaring_nothing_is_flagged() {
        let body = "## Stage contract\n\
             \n\
             ### Inputs\n\
             \n\
             - The task, as understood.\n\
             \n\
             ### Process\n\
             \n\
             - Do the work.\n\
             \n\
             ### Outputs\n\
             \n\
             - A diff.\n\
             \n\
             ### Verify\n\
             \n\
             - Tests pass.\n";
        let result = scan_contract_block(body);
        match result {
            ContractBlock::Malformed { problems } => {
                assert!(
                    problems.contains(&ContractProblem::InputsDeclareNothing),
                    "expected InputsDeclareNothing, got: {problems:?}"
                );
            }
            other => panic!("expected Malformed for empty Inputs, got: {other:?}"),
        }
    }

    /// (d) PINNING: a block whose Inputs declares ONLY a discovery budget
    /// (no named static files) is ACCEPTED — coding inputs are discovered,
    /// so this must not be over-constrained to a fixed file list.
    #[test]
    fn contract_block_discovery_budget_only_is_accepted() {
        let body = "## Stage contract\n\
             \n\
             ### Inputs\n\
             \n\
             - Discovery budget: grep and glob across the workspace to find\n\
             the relevant modules; no fixed file list is prescribed.\n\
             \n\
             ### Process\n\
             \n\
             - Discover, then implement.\n\
             \n\
             ### Outputs\n\
             \n\
             - A diff.\n\
             \n\
             ### Verify\n\
             \n\
             - Tests pass.\n";
        let result = scan_contract_block(body);
        assert_eq!(
            result,
            ContractBlock::Valid,
            "discovery-budget-only Inputs must be accepted, got: {result:?}"
        );
    }

    /// A body with no `## Stage contract` section is Absent, never
    /// Malformed — the block is optional and every historical handoff lacks
    /// it. This is the backward-compat pin at the scanner level.
    #[test]
    fn contract_block_absent_when_no_heading() {
        let body = "## Task(s)\n\n- do a thing\n\n## Artifacts\n\n- a file\n";
        assert_eq!(scan_contract_block(body), ContractBlock::Absent);
    }

    /// Sub-section order does not matter; all four present in a scrambled
    /// order still validates.
    #[test]
    fn contract_block_subsection_order_irrelevant() {
        let body = "## Stage contract\n\
             \n\
             ### Verify\n\
             \n\
             - Tests pass.\n\
             \n\
             ### Outputs\n\
             \n\
             - A diff.\n\
             \n\
             ### Process\n\
             \n\
             - Work.\n\
             \n\
             ### Inputs\n\
             \n\
             - `path/to/input.rs` plus a grep budget.\n";
        assert_eq!(scan_contract_block(body), ContractBlock::Valid);
    }

    /// The real per-kind preamble blocks (below) each parse to Valid. This
    /// pins the docs against the validator: if a preamble drifts from the
    /// four-sub-section shape, this fails.
    #[test]
    fn preamble_style_block_with_static_and_discovery_validates() {
        // A block whose Inputs uses the `Static inputs:` / `Discovery
        // budget:` label style the preambles adopt.
        let body = "## Stage contract\n\
             \n\
             ### Inputs\n\
             \n\
             - Static inputs: the failing test and the module under `crates/`.\n\
             - Discovery budget: `rg`/glob to locate the offending call site.\n\
             \n\
             ### Process\n\
             \n\
             - Reproduce, localize, fix, regression-test.\n\
             \n\
             ### Outputs\n\
             \n\
             - Minimal diff + regression test.\n\
             \n\
             ### Verify\n\
             \n\
             - The regression test fails pre-fix and passes post-fix.\n";
        assert_eq!(scan_contract_block(body), ContractBlock::Valid);
    }

    /// FIX 2 pin: incidental English that merely CONTAINS discovery-tool
    /// substrings ("la**rg**e", "**glob**al", "**find** the seam") but names no
    /// static input and declares no real discovery budget must be FLAGGED. The
    /// old bare-substring markers (`"rg "`, `" find "`, `"glob"`) accepted this;
    /// the tightened markers (explicit phrases + backticked tokens) must not.
    #[test]
    fn contract_block_incidental_prose_is_flagged() {
        let body = "## Stage contract\n\
             \n\
             ### Inputs\n\
             \n\
             - Explore the large surface to find the seam in the global scope.\n\
             \n\
             ### Process\n\
             \n\
             - Do the work.\n\
             \n\
             ### Outputs\n\
             \n\
             - A diff.\n\
             \n\
             ### Verify\n\
             \n\
             - Tests pass.\n";
        let result = scan_contract_block(body);
        match result {
            ContractBlock::Malformed { problems } => {
                assert!(
                    problems.contains(&ContractProblem::InputsDeclareNothing),
                    "incidental 'large'/'find the'/'global' prose must not be \
                     read as a discovery budget; expected InputsDeclareNothing, \
                     got: {problems:?}"
                );
            }
            other => panic!("expected Malformed for incidental-prose Inputs, got: {other:?}"),
        }
    }
}

/// FIX 1: file-backed regression against the REAL per-kind worker preambles.
///
/// Each kind preamble ships a well-formed `## Stage contract` block; this
/// module embeds every real file at COMPILE TIME (`include_str!`, so the test
/// is hermetic and CWD-independent) and asserts the scanner reads each one as
/// [`ContractBlock::Valid`]. If a real preamble ever drifts out of the
/// four-sub-section shape — or its `### Inputs` stops declaring a static input
/// or a discovery budget — this fails, closing the plan's "the preamble
/// variants still parse" item and the Done-means "each variant carries a
/// *validated* contract".
///
/// `body.rs` lives at `crates/rsi-common/src/handoff_schema/`; the repo root is
/// therefore four levels up (`../../../../`). The base `worker_preamble.md`
/// documents the shape but does not itself carry a full four-sub-section block,
/// so it is deliberately NOT asserted here.
#[cfg(test)]
mod preamble_corpus {
    use super::{ContractBlock, scan_contract_block};

    /// (kind label, embedded file contents) for each real kind preamble.
    const REAL_PREAMBLES: [(&str, &str); 4] = [
        (
            "bug",
            include_str!("../../../../.claude/commands/_shared/worker_preamble_bug.md"),
        ),
        (
            "feature",
            include_str!("../../../../.claude/commands/_shared/worker_preamble_feature.md"),
        ),
        (
            "refactor",
            include_str!("../../../../.claude/commands/_shared/worker_preamble_refactor.md"),
        ),
        (
            "research",
            include_str!("../../../../.claude/commands/_shared/worker_preamble_research.md"),
        ),
    ];

    #[test]
    fn real_kind_preambles_carry_a_valid_contract_block() {
        for (kind, content) in REAL_PREAMBLES {
            let result = scan_contract_block(content);
            assert_eq!(
                result,
                ContractBlock::Valid,
                "real {kind} preamble must carry a Valid stage-contract block, \
                 got: {result:?}"
            );
        }
    }
}
