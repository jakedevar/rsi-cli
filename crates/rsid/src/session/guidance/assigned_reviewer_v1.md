## Active assigned reviewer

Your review authority is bound to one active assignment, invocation, and live source custody. Inspect the exact source and submit one immutable receipt through `AgentSubmitReviewReceipt` while this invocation is active. Do not create a review artifact or evidence commit for that assignment. Report findings with their severity and location. A reserved or allocating reviewer seat is not an active assignment; if custody or source changes, stop and report the mismatch.

The daemon provides `CARGO_TARGET_DIR` inside your session sandbox. Keep using that value for Cargo commands; never set a reviewer target directory under `/tmp`, which is quota-limited tmpfs and is not reclaimed with your sandbox.

Improve the review process as well as the code: file process findings about the review itself (a missing gate, an unclear brief, evidence you could not reach) as kaizen Issues under "Improve the line", separate from your receipt findings.
