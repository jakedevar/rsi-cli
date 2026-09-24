//! `rsi-contract-validate` — CLI for the master-side contract validator
//! (RSI-021).
//!
//! Reads a worker reply from stdin, validates the leading
//! `PIPELINE HANDOFF — <STAGE>:` block (or, with `--worker-report`, the
//! sibling `WORKER REPORT:` block from `team_implement` workers), and emits
//! a machine-readable result.
//!
//! Usage:
//!   rsi-contract-validate <ticket>                   # parse pipeline handoff
//!   rsi-contract-validate <ticket> --first-line-only # only run the marker check
//!   rsi-contract-validate <ticket> --manifest <path> # + cross-stage VERIFY gate
//!   rsi-contract-validate <ticket> --strict-v2       # closed no-idle handoff
//!   rsi-contract-validate --worker-report            # parse WORKER REPORT block
//!   rsi-contract-validate --orchestration-outcome    # strict program final
//!
//! Reply text is read from stdin in all modes.
//!
//! ## Cross-stage VERIFY coverage gate (S6/D1)
//!
//! For a `PIPELINE HANDOFF — VERIFY:` reply that *declares* plan/research
//! linkage keys (a `satisfies:` / `covers:` / `linkage:` / `requires:` line),
//! the validator also runs [`cross_stage_verify_coverage`]: every declared key
//! MUST be covered by some verification-manifest item's `satisfies`/`covers`.
//! An uncovered key is silent research→plan→impl drift and exits **2**.
//!
//! The manifest is resolved in this order:
//!   1. an explicit `--manifest <path>` argument, then
//!   2. the handoff's own `Manifest path:` field.
//!
//! The gate stays dormant for non-VERIFY handoffs and handoffs without linkage.
//! For linked VERIFY handoffs, an explicit `--manifest` or `--strict-v2` requires
//! a readable, parseable manifest; admission failure exits **2**. Strict mode
//! without `--manifest` requires the handoff's own manifest. Legacy mode without
//! either flag preserves a visible skip when the implicit manifest is unresolved,
//! unreadable, or unparseable.
//!
//! ## Stage-contract block gate (S7/SP2)
//!
//! For every full-parse reply (pipeline handoff or `--worker-report`), the
//! validator also runs [`scan_contract_block`] over the raw reply. The S7
//! stage-contract block (`## Stage contract` → `### Inputs/Process/Outputs/
//! Verify`) is documented in the worker preambles; this arms it as a gate.
//!
//! A reply with NO `## Stage contract`
//! block ([`ContractBlock::Absent`] — every historical handoff) or a
//! well-formed one ([`ContractBlock::Valid`]) is dormant/pass (exit unchanged
//! from pre-SP2). Only a present-but-malformed block
//! ([`ContractBlock::Malformed`]) is a contract violation → exit **2**. The
//! `--first-line-only` marker checks are unaffected.
//!
//! Exit codes:
//!   0 — contract satisfied; stdout has the parsed structure as JSON.
//!   1 — I/O or argument error.
//!   2 — contract violation; stderr explains the failure. Stdout may already
//!       contain the parsed handoff, optionally followed by a serialized error.
//!       Callers must inspect the final exit status even when JSON was emitted.
//!
//! The master shells out to this binary at every parse site in
//! `master_implement.md` (VERIFY parse at Step 3.5) and `team_implement.md`.
//! On exit 2 the master `SendMessage`s a single corrective directive, then
//! halts on a second failure.

use std::io::Read as _;
use std::process::ExitCode;

use rsi_common::agent_contract::{
    PipelineHandoff, Stage, cross_stage_verify_coverage, parse_closure_pipeline_handoff_v1,
    parse_closure_review_handoff_v1, parse_orchestration_outcome_v1, parse_pipeline_handoff,
    parse_pipeline_handoff_v2, parse_worker_report, validate_first_line,
    validate_worker_report_first_line,
};
use rsi_common::handoff_schema::{ContractBlock, ContractProblem, scan_contract_block};
use rsi_common::verification_manifest::parse as parse_manifest;

fn print_usage() {
    eprintln!(
        "usage: rsi-contract-validate <ticket> [--first-line-only] [--manifest <path>]\n\
         usage: rsi-contract-validate <ticket> --strict-v2 [--manifest <path>]\n\
         usage: rsi-contract-validate <ticket> --closure-review\n\
         usage: rsi-contract-validate --worker-report [--first-line-only]\n\
         usage: rsi-contract-validate --orchestration-outcome\n\
         \n\
         Reply text is read from stdin in all modes.\n\
         Exit codes: 0=valid, 1=I/O or arg error, 2=contract violation."
    );
}

fn main() -> ExitCode {
    let mut ticket: Option<String> = None;
    let mut first_line_only = false;
    let mut worker_report = false;
    let mut closure_review = false;
    let mut strict_v2 = false;
    let mut orchestration_outcome = false;
    let mut manifest_arg: Option<String> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--first-line-only" => first_line_only = true,
            "--worker-report" => worker_report = true,
            "--closure-review" => closure_review = true,
            "--strict-v2" => strict_v2 = true,
            "--orchestration-outcome" => orchestration_outcome = true,
            "--manifest" => {
                let Some(path) = args.next() else {
                    eprintln!("error: --manifest requires a <path> argument");
                    print_usage();
                    return ExitCode::from(1);
                };
                manifest_arg = Some(path);
            }
            "-h" | "--help" => {
                print_usage();
                return ExitCode::from(0);
            }
            other if other.starts_with("--") => {
                eprintln!("error: unknown flag `{other}`");
                print_usage();
                return ExitCode::from(1);
            }
            other => {
                if ticket.is_some() {
                    eprintln!("error: only one positional <ticket> argument is allowed");
                    return ExitCode::from(1);
                }
                ticket = Some(other.to_string());
            }
        }
    }

    let mut reply = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut reply) {
        eprintln!("I/O error reading stdin: {e}");
        return ExitCode::from(1);
    }

    if strict_v2 && (worker_report || closure_review || first_line_only) {
        eprintln!("error: --strict-v2 requires a full pipeline handoff parse");
        return ExitCode::from(1);
    }

    if orchestration_outcome {
        if worker_report
            || closure_review
            || strict_v2
            || first_line_only
            || ticket.is_some()
            || manifest_arg.is_some()
        {
            eprintln!("error: --orchestration-outcome is a standalone mode");
            return ExitCode::from(1);
        }
        match parse_orchestration_outcome_v1(&reply) {
            Ok(outcome) => {
                emit_json(&outcome);
                ExitCode::from(0)
            }
            Err(error) => {
                emit_json(&error);
                eprintln!("contract error: {error}");
                ExitCode::from(2)
            }
        }
    } else if worker_report {
        if first_line_only {
            return emit_first_line_result(validate_worker_report_first_line(&reply));
        }
        match parse_worker_report(&reply) {
            Ok(report) => {
                emit_json(&report);
                // Stage-contract block gate (S7/SP2). Dormant unless the reply
                // carries a present-but-malformed `## Stage contract` block.
                run_contract_block_gate(&reply).unwrap_or_else(|| ExitCode::from(0))
            }
            Err(e) => {
                emit_json(&e);
                eprintln!("contract error: {e}");
                ExitCode::from(2)
            }
        }
    } else if closure_review {
        if first_line_only {
            return emit_first_line_result(validate_first_line(&reply));
        }
        match parse_closure_review_handoff_v1(&reply) {
            Ok(handoff) => {
                emit_json(&handoff);
                ExitCode::from(0)
            }
            Err(error) => {
                emit_json(&error);
                eprintln!("contract error: {error}");
                ExitCode::from(2)
            }
        }
    } else {
        let Some(ticket) = ticket else {
            eprintln!("error: missing <ticket> argument (or pass --worker-report)");
            print_usage();
            return ExitCode::from(1);
        };
        if first_line_only {
            return emit_first_line_result(validate_first_line(&reply));
        }
        if reply
            .lines()
            .find(|line| !line.trim().is_empty())
            .is_some_and(|line| line.trim() == "PIPELINE HANDOFF — CLOSURE:")
        {
            return match parse_closure_pipeline_handoff_v1(&reply) {
                Ok(handoff) => {
                    emit_json(&handoff);
                    ExitCode::from(0)
                }
                Err(error) => {
                    emit_json(&error);
                    eprintln!("contract error: {error}");
                    ExitCode::from(2)
                }
            };
        }
        let parsed = if strict_v2 {
            parse_pipeline_handoff_v2(&reply, &ticket)
                .map(|strict| (strict.handoff.clone(), serde_json::to_value(strict)))
        } else {
            parse_pipeline_handoff(&reply, &ticket)
                .map(|handoff| (handoff.clone(), serde_json::to_value(handoff)))
        };
        match parsed {
            Ok((handoff, serialized)) => {
                match serialized {
                    Ok(value) => emit_json(&value),
                    Err(error) => {
                        eprintln!("internal: failed to serialize result: {error}");
                        return ExitCode::from(1);
                    }
                }
                // Stage-contract block gate (S7/SP2) — dormant unless the reply
                // carries a present-but-malformed `## Stage contract` block.
                // Checked before the S6 gate; either violation exits 2.
                if let Some(code) = run_contract_block_gate(&reply) {
                    return code;
                }
                // Cross-stage VERIFY coverage gate (S6/D1). Linked VERIFY
                // handoffs require manifest admission when explicit or strict.
                run_cross_stage_gate(&handoff, manifest_arg.as_deref(), strict_v2)
            }
            Err(e) => {
                emit_json(&e);
                eprintln!("contract error: {e}");
                ExitCode::from(2)
            }
        }
    }
}

/// Run the cross-stage VERIFY coverage gate for an already-parsed, valid
/// handoff. Returns exit 0 when the gate passes or is not applicable, exit 2
/// on required-manifest admission failure or `UncoveredLinkage`.
///
/// Applies only to VERIFY handoffs with linkage. Explicit `--manifest` wins
/// over the handoff's `manifest_path`. Either an explicit path or strict-v2
/// requires admission; only legacy implicit-manifest failures remain skips.
fn run_cross_stage_gate(
    handoff: &PipelineHandoff,
    manifest_arg: Option<&str>,
    strict_v2: bool,
) -> ExitCode {
    // Non-VERIFY or no declared linkage → gate is dormant (backward compat).
    if handoff.stage != Stage::Verify || handoff.linkage.is_empty() {
        return ExitCode::from(0);
    }

    // One admission policy covers absent paths, read errors, and parse errors.
    // Removing --manifest cannot turn a strict failure into a legacy skip.
    let (unavailable_code, unavailable_action) = if manifest_arg.is_some() {
        (2, "required by --manifest; coverage check failed")
    } else if strict_v2 {
        (2, "required by --strict-v2; coverage check failed")
    } else {
        (0, "skipping coverage check")
    };

    // Resolve once: explicit --manifest wins, with no fallback on failure.
    let Some(manifest_path) = manifest_arg.or(handoff.manifest_path.as_deref()) else {
        eprintln!(
            "cross-stage gate: VERIFY handoff declares linkage {:?} but no manifest is \
             resolvable (no --manifest and no `Manifest path:` field); {unavailable_action}",
            handoff.linkage
        );
        return ExitCode::from(unavailable_code);
    };

    // Preserve legacy skip diagnostics while making required admission fail
    // with the path, cause, and policy that requires the manifest on stderr.
    let manifest_src = match std::fs::read_to_string(manifest_path) {
        Ok(src) => src,
        Err(e) => {
            eprintln!(
                "cross-stage gate: manifest `{manifest_path}` is unreadable ({e}); \
                 {unavailable_action}"
            );
            return ExitCode::from(unavailable_code);
        }
    };
    let manifest = match parse_manifest(&manifest_src) {
        Ok(manifest) => manifest,
        Err(e) => {
            eprintln!(
                "cross-stage gate: manifest `{manifest_path}` does not parse ({e:?}); \
                 {unavailable_action}"
            );
            return ExitCode::from(unavailable_code);
        }
    };

    match cross_stage_verify_coverage(handoff, &manifest) {
        Ok(()) => ExitCode::from(0),
        // The only reachable error here is `UncoveredLinkage` — the stage was
        // checked above, so `NotVerifyStage` cannot occur. Any error is a
        // contract violation → exit 2.
        Err(e) => {
            emit_json(&e);
            eprintln!("contract error: {e}");
            ExitCode::from(2)
        }
    }
}

/// Run the S7 stage-contract block gate over the raw reply text.
///
/// Returns `Some(ExitCode::from(2))` (after emitting a structured diagnostic)
/// ONLY when the reply carries a present-but-malformed `## Stage contract`
/// block. Returns `None` — dormant/pass, behavior identical to pre-SP2 — when
/// the block is absent ([`ContractBlock::Absent`], every historical handoff)
/// or well-formed ([`ContractBlock::Valid`]). This is the strict-superset
/// property: no NEW failure mode for replies that carry no block.
fn run_contract_block_gate(reply: &str) -> Option<ExitCode> {
    match scan_contract_block(reply) {
        ContractBlock::Absent | ContractBlock::Valid => None,
        ContractBlock::Malformed { problems } => {
            let labels: Vec<String> = problems.iter().map(describe_contract_problem).collect();
            emit_json(&serde_json::json!({
                "error": "malformed_contract_block",
                "problems": labels,
            }));
            eprintln!("contract error: malformed stage-contract block: {labels:?}");
            Some(ExitCode::from(2))
        }
    }
}

/// Render a single [`ContractProblem`] as a stable machine-friendly label for
/// the JSON diagnostic.
fn describe_contract_problem(problem: &ContractProblem) -> String {
    match problem {
        ContractProblem::MissingSubsection(name) => format!("missing_subsection:{name}"),
        ContractProblem::InputsDeclareNothing => "inputs_declare_nothing".to_string(),
    }
}

fn emit_json<T: serde::Serialize>(value: &T) {
    match serde_json::to_string_pretty(value) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("internal: failed to serialize result: {e}"),
    }
}

fn emit_first_line_result(r: Result<(), rsi_common::agent_contract::ContractError>) -> ExitCode {
    match r {
        Ok(()) => {
            println!("{{\"valid\": true}}");
            ExitCode::from(0)
        }
        Err(e) => {
            emit_json(&e);
            eprintln!("contract error: {e}");
            ExitCode::from(2)
        }
    }
}
