# Plan CODE-011 — keep generated marker messages readable

| | |
| --- | --- |
| Finding | September 7 follow-up in `docs/2026-09-02_REPOSITORY_REVIEW.md`, CODE-011 (conversational CODE-002) |
| Severity / priority | MEDIUM / P1 |
| Impact / effort | Medium / Medium |
| Decision required | Accepted in decision 0032 CODE-011 addendum: prevention and documentation only |
| Depends on | No unfinished implementation task |
| Status | Scope accepted; implementation pending |

## Evidence and scope

At `f1794e5`, a 1,048,575-byte source message passes
`validated_commit_message`; `marker::build_message` grows it to 1,048,860 bytes.
The first sync publishes it successfully; the next rejects it above the
1,048,576-byte verification limit. Both directional commit builders use the
same unchecked construction pattern.

## 1. Regression first

Add real-repository tests for source→dest and dest→source with a near-limit
message whose generated form exceeds the limit. Assert refusal before publishing
that invalid generated commit or advancing its local branch. Preserve existing
partial-progress semantics; do not invent whole-run rollback.

Add exact-boundary tests for the complete generated message (at limit accepted,
one byte over refused), including variable branch lengths and body sanitization.
For accepted messages, verify the created commit's marker and run sync again:
it must be recognized, with no duplicate import/export or unexpected refusal.

## 2. Record and enforce the output invariant

Document under decision 0032 that the size limit applies to complete generated
messages as well as input. Validate the final serialized bytes before commit
creation, through a shared fallible construction boundary used by both builders.
Inspect every `build_message` caller, including setup and synthetic resolve state,
so callers cannot publish state the reader will reject.

Do not truncate the original message or relax authentication. Account for the
actual serialized overhead, not an assumed constant or an OID-only estimate.

## 3. Document existing-state limitation

Owner decision: prevent new occurrences and document the limitation; no repair,
migration, relaxed reader limit or additional recovery design is required.
Document that already-published oversized markers can still block sync and that
reruns do not repair them. Preserve affected history for individual operator
investigation. Do not truncate messages, forge state or rewrite round-tripped
history as a generic workaround. This documentation completes the legacy-state
scope; it is not a blocker awaiting another owner decision.

## Verification and acceptance

Run marker, sync and resolve tests, both new directional regressions, the full
suite, MSRV check and repository release gates. Every accepted generated commit
must pass its own marker verifier; no over-limit generated commit may be published.
Update the review/status index and design log when implemented.
