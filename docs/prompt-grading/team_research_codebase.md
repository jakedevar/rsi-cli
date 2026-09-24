# Grade: team_research_codebase.md

**Final grade: C**  |  **Score: 40/70**

## Rubric
| Dimension | Score | Comment |
|---|---|---|
| Role & Task Clarity | 8/10 | Master-worker role is clearly stated, when-to-use vs when-not-to-use is explicit, documentarian framing is unambiguous. Best part of the file. |
| Structural Scaffolding | 5/10 | Numbered steps + a worker prompt template. No XML tags, no typed fields. The "RESEARCH REPORT" block is a flat ASCII schema — parseable by eyeballs, not by regex. |
| Output Contract | 4/10 | Report schema exists (L124-137), but every field is "1-2 sentences" with no numeric cap. No per-worker token budget. Master output format (L253-264) is also uncapped. |
| Context Discipline | 3/10 | L42 and L116 both mandate "read FULLY — never use limit/offset." L121 opens the code-snippet loophole. Workers routinely return 500-line "short illustrative snippets." Violates #5 and #6. |
| Stop Conditions & Escape Hatches | 4/10 | `Status: COMPLETE | PARTIAL | BLOCKED` is the only stop signal. No attempt cap, no worker timeout, no `<blocking_question>` schema for true ambiguity. "Gaps" field is the closest thing. |
| Negative Space | 6/10 | Strong explicit list on L23-27 (no critique / no refactor / no improvements) and L119-122 forbidding narration/dumps. But no forbidden-content list for the master's synthesis document. |
| Composition & DRY | 4/10 | Five-Expert (L30-36), commit-push (L224-232), and original-question (L281-290) blocks are duplicated from sibling commands. Worker template is inlined rather than stored as a shared artifact. Pipeline override (L268-277) is good factoring, though. |

## Margin comments (teacher annotations)
- **L36** — "Vim Language Designer" listed as #7 under a header that says "Five-Expert lenses" and enumerates 7 items — *arithmetic is off, and CLAUDE.md calls it the Seven-Expert Framework. Inconsistent naming with canon.*
- **L42** — "read them FULLY now in the main context — never use limit/offset" — *same disease as implement_plan L40. Master context bloats before workers even dispatch. Violates #5. The master's job is to dispatch, not to ingest. Fix: "Grep for the question keywords; read only cited ranges."*
- **L94** — "Spawn all domain workers in a single message using the Agent tool with run_in_background: true" — *good — satisfies #12 parallel-spawn affordance explicitly. Keep.*
- **L116** — "If a file is mentioned, read it FULLY — never use limit/offset" — *propagates the context-bloat mandate into every worker. A worker hitting `crates/rsid/src/session.rs` (~2000 lines) will blow its own window on one file. Violates #5 and #6.*
- **L121** — "Short illustrative code snippets (3-5 lines max) are allowed only when essential" — *the loophole. Workers interpret "essential" generously and return ten 5-line snippets each. Fix: zero snippets in the return message; snippets go to `thoughts/drafts/research-<worker>.md` on disk, master receives only the path. Violates #5 (Disk-is-the-channel) hard.*
- **L129-132** — "[Finding 1 — 1-2 sentences with file:line reference]" — *"1-2 sentences" is a vibe, not a cap. Workers return paragraphs. Replace with `max_words: 40` per finding. Violates #3 and #4.*
- **L128** — "Key files: [file:line, file:line, ...]" — *no max_items. Workers return 30+ file references. Violates #4 (structured schema with max_items).* 
- **L98-137** — no per-worker token budget anywhere in the template — *the master has no way to refuse an over-long report. Budget should be explicit: "Entire return message ≤ 400 tokens." Violates #3.*
- **L136** — "[EXTRA FIELDS — master appends 1-2 lines here for targeted domains, leave blank otherwise]" — *clever in theory, but no enforcement on the master side. No example of a populated EXTRA FIELDS block at dispatch time — violates #11 (Format anchoring).*
- **L157** — "spawn one targeted follow-up worker (synchronous, not background)" — *good — explicit serial mode for gap-filling. Keep.*
- **L219** — "Open Questions" in the doc — *fine, but document itself has no size cap. A synthesis doc for a cross-domain question can exceed 5k words. Add an upper bound.*
- **L224-232** — Commit and Push block — *duplicated verbatim from implement_plan. Violates #14. Also `git add` entire `thoughts/` dir with no forbidden-path list — same risk as the sibling.*
- **L281-290** — Original Question Restatement — *same duplication issue. Pipeline plumbing belongs in a shared include.*
- **L46-51** — fallback response when no question is provided — *good UX touch, but the block itself is prose that will be echoed verbatim; wrap in a `<no_input_response>` tag for parseability. Violates #2.*
- **L268-277** — Pipeline Mode Override — *the strongest section in the file. Clear contract: "terminal message MUST contain ONLY the PIPELINE HANDOFF block." This is exactly the discipline every other section lacks. Export it as a template.*

## Principles missing (with one-sentence fix)
- **#2 XML scaffolding** — wrap the worker return in `<research_report><domain/><status/><findings/></research_report>` so the master can parse with a regex, not an LLM.
- **#3 Output contracts** — numeric caps: worker return ≤ 400 tokens, max 8 findings, max 12 key files, synthesis doc ≤ 2500 words.
- **#4 Structured return schemas** — typed fields with `max_words` per entry and `max_items` per list, not "1-2 sentences."
- **#5 Disk-is-the-channel** — workers write drafts to `thoughts/drafts/research-<domain>.md`; return message carries `{path, status, finding_count, blocker?}` only. Kill the snippet loophole on L121.
- **#6 Discard-after-extract** — master extracts fields and drops raw worker text before synthesis.
- **#7 Forbidden-content lists** — explicit per-worker forbidden list: no full file contents, no narration, no chain-of-thought, no improvement suggestions (currently only the last is forbidden).
- **#8 Stop conditions** — worker must halt after N failed Grep/Read attempts and emit BLOCKED status.
- **#9 Escape hatches** — a `<blocking_question>` tag for true ambiguity, not just a `Gaps` field.
- **#14 DRY preambles** — extract Five-Expert, commit-push, original-question into a shared include.

## Overall verdict

Structurally this is the stronger of the two prompts. The master-worker separation is clean, the parallel-spawn directive on L94 is explicit and correct, the documentarian framing on L23-27 is exactly the right negative-space discipline, and the Pipeline Mode Override on L268-277 is genuinely good prompt engineering — a terminal-output contract stated crisply in four bullets. If the rest of the file were written with that same discipline, this would be a B+.

The rot is in the worker contract. L121's "short illustrative code snippets (3-5 lines max) are allowed" is the kind of loophole that sounds reasonable to a human reviewer and is immediately abused by an LLM. Combined with L129-132's "1-2 sentences" (uncapped in tokens) and the absence of any per-worker budget, the master reliably receives 2-3k tokens of prose per worker × 4-6 workers = the master burns 10-18k tokens on raw worker text before synthesis even starts. This is the exact failure mode principles #5 and #6 exist to prevent, and the file ignores both.

Fix path, in priority order: (1) kill L121 snippets entirely — drafts go to disk, (2) add numeric caps to every field in the worker schema (max_words, max_items), (3) add a total token budget to each worker's return message, (4) wrap the report in XML tags for deterministic parsing, (5) extract the duplicated blocks (Five-Expert / commit-push / original-question) into a shared include. Steps 1-3 alone move this from C to B+. The bones are good; the contract is loose.
