# Coding environment

TuxBridge exposes editor-grade primitives so an AI can inspect and modify a workspace without falling back to broad shell text manipulation.

## Coding endpoints

- `POST /v1/code/context` — read a numbered code window around a line/range. Returns the full-file SHA-256 so a later edit can prove it is based on the same file version.
- `POST /v1/code/symbols` — lightweight language-aware outline for common Rust, Go, Python, JavaScript/TypeScript, PHP, Java, and Kotlin declarations. This is intentionally heuristic, not advertised as compiler/LSP truth.
- `POST /v1/code/references` — bounded identifier-boundary search across a workspace or subtree, skipping generated/vendor directories and symlinks.
- `POST /v1/code/edit-plan` — multi-file editor transaction preflight. Each file carries an expected SHA-256 and one or more exact/range/insert edits. `dry_run=true` returns hashes and compact diff previews without writing. All hashes are validated before the first write; each final file replacement is same-directory atomic. Cross-file writes are not falsely claimed to be filesystem-transaction atomic.
- `POST /v1/code/tasks` — discover likely check/test/lint/build/format tasks from project manifests without executing them.

## Supported edit operations

`edit-plan` supports:

- `replace_exact` — replace text only when it occurs exactly once;
- `replace_lines` — replace a 1-based inclusive line range;
- `insert_before` — insert before a 1-based line;
- `insert_after` — insert after a 1-based line.

Every target must provide the SHA-256 returned by a fresh read/context operation. A stale file returns `409 Conflict` before any mutation begins.

## Recommended agent loop

1. Inspect the project and Git status.
2. Read focused numbered context instead of repeatedly loading whole files.
3. Use symbol outline/reference search to understand change impact.
4. Read/hash every file that will be edited.
5. Submit one multi-file edit plan with `dry_run=true`.
6. Review the previews and new hashes.
7. Submit the same plan with `dry_run=false` while the hashes are still current.
8. Inspect Git diff.
9. Discover project tasks and run appropriate checks through the permitted command interface.
10. Re-read diagnostics/diff before commit.

## Why this is separate from shell access

Shell commands are useful for builds and project tooling, but they are a poor editing protocol: quoting is fragile, stale reads are easy to overwrite, and `sed`/scripts do not naturally carry optimistic concurrency metadata. The code endpoints make ordinary editing deterministic and auditable even when the security profile permits a full shell.

## Language intelligence roadmap

The current outline/reference layer is deliberately dependency-light. True IDE semantics should be added through a dedicated language-intelligence layer rather than pretending text heuristics are equivalent to a compiler.

Planned adapters can expose:

- LSP initialize/workspace lifecycle;
- go-to-definition;
- hover/type information;
- find references;
- document/workspace symbols;
- compiler/linter diagnostics;
- completion;
- code actions;
- semantic rename with workspace edits;
- formatting;
- call hierarchy.

Those operations should preserve the same TuxBridge rules: workspace scoping, bounded output, capability checks, explicit execution risk, and hash-aware edits before applying language-server workspace changes.
