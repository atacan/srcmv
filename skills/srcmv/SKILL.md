---
name: srcmv
description: Safely inspect, semantically select, preview, move, copy, reorder, or split exact bytes already present in workspace files with the srcmv CLI. Use when a coding task asks to select a function, class, or declaration through an installed language server; relocate existing code or data; preserve selected bytes exactly; construct protocol-v1 requests; commit an expected preview plan; diagnose conflicts; or inspect and recover an interrupted transaction.
---

# srcmv

Use srcmv for exact relocation or duplication of bytes that already exist in a workspace. Keep generated content, import changes, formatting, and semantic refactors outside the srcmv operation.

## Enforce the agent safety gate

For every mutation, follow this sequence without shortcuts:

1. Inspect every source and destination.
2. Build the request from the observed digests and coordinates.
3. Preview the request and review the resolved operations, outputs, diff, warnings, and `plan_sha256`.
4. Commit the unchanged request with `--expect-plan` set to that previewed digest.
5. Verify the commit response and resulting workspace.

Never use `--accept-current-plan`. If a digest, file, or plan changed, inspect and preview again. Never reproduce the selected bytes by hand as a fallback after srcmv rejects an operation.

If srcmv returns `TRANSACTION_BUSY`, wait before retrying and never bypass or remove the lock. Retrying an inspect or preview takes a fresh observation. You may retry the unchanged commit request with the same `--expect-plan`; if it then reports a precondition or plan mismatch, return to inspect and preview. Before an unrelated mutation after contention, use `recover --list --json` as the point-in-time workspace status check.

Read [references/workflow.md](references/workflow.md) before executing any mutating task. It contains the commands, review gates, and retry rules.

## Keep the operation in scope

srcmv moves or copies line or byte ranges. Its read-only `select` and `outline`
commands can discover declarations through a trusted, installed language
server — `outline` lists every symbol in one file, `select` resolves one exact
range; selection and editing remain separate invocations. srcmv does not bundle language servers, update imports, format code, normalize line endings, create parent directories, or provide atomic multi-file visibility. Treat follow-up semantic edits as separate work and test the final workspace.

Before use, confirm `srcmv` is available and query `capabilities --json` when version or feature support is uncertain. See [references/cli-protocol.md](references/cli-protocol.md) for command grammar, response fields, protocol version, and error categories.

## Load only the needed detail

- Read [references/request-construction.md](references/request-construction.md) when selecting lines or bytes, choosing anchors, creating a new destination, composing multiple operations, or interpreting a no-op.
- Read [references/semantic-selection.md](references/semantic-selection.md) when asked to select one or several functions, classes, or declarations by name or source position, or when composing selection results into an edit request.
- Read [references/safety-and-exactness.md](references/safety-and-exactness.md) before multi-file work, binary or mixed-line-ending work, metadata-sensitive changes, or when a path, platform, alias, resource, or precondition check fails.
- Read [references/recovery.md](references/recovery.md) only when recovery is requested or a transaction is unfinished, busy, interrupted, or corrupt. Do not choose completion versus rollback without explicit user intent.

## Preserve evidence

Keep the request and preview response available through commit. Report the previewed plan digest, transaction state, files changed, warnings, and any separate follow-up edits. For a no-op, verify that `transaction_id` is `null` and `transaction_state` is `no_op` rather than claiming a mutation occurred.
