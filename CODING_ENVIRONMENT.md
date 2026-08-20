# Coding environment

TuxBridge now exposes four complementary development layers: repository orientation, Tree-sitter syntax structure, authoritative Language Server Protocol semantics, and TuxBridge-owned hash-safe mutation/verification machinery.

The goal is not to imitate an IDE UI. It is to give an AI agent the same useful primitives an IDE-backed coding agent relies on while keeping authorization, mutation safety, rollback, and observability inside TuxBridge.

## Repository orientation

`POST /v1/code/map` gives the agent a bounded compact map before it starts opening files blindly. It reports detected project kinds/manifests, language counts, likely source/test roots, configuration and important files, a bounded source-file sample, and current Git-changed paths when `git_read` is available.

This is intentionally cheaper than recursively reading a project and reduces orientation tool calls on unfamiliar repositories.

## Tree-sitter structural intelligence

TuxBridge bundles Tree-sitter grammars for Rust, Go, Python, JavaScript/JSX, TypeScript, and TSX.

- `POST /v1/code/structure` — parse a file and return exact named syntax nodes with byte and source ranges;
- `POST /v1/code/node-at` — resolve the smallest named syntax node at a 1-based Unicode-scalar source position;
- `POST /v1/code/replace-node` — preview or hash-guardedly replace an exact syntax node, optionally requiring the node kind to still match.

Tree-sitter is used for concrete syntax structure, not type-system claims. It is the right tool for exact function/class/module boundaries and structural editing. LSP remains the authority for semantic references, types, imports, definitions, and refactors.

## Editor primitives

- `POST /v1/code/context` — numbered code window plus full-file SHA-256;
- `POST /v1/code/symbols` — cheap heuristic outline fallback;
- `POST /v1/code/references` — cheap bounded identifier-boundary search fallback;
- `POST /v1/code/edit-plan` — multi-file hash-guarded edit preflight/apply;
- `POST /v1/code/tasks` — discover project-defined verification tasks without executing them.

`edit-plan` supports `replace_exact`, `replace_lines`, `insert_before`, and `insert_after`. All file hashes are preflighted before the first write and each final file replacement is same-directory atomic. Cross-file writes are not falsely claimed to be one filesystem transaction.

An edit plan may include `session_id`. When it does, TuxBridge records each file's original bytes and the hash the agent is about to write as part of the active coding session, so ordinary multi-file edits become rollback-aware without separate bookkeeping.

## Real semantic intelligence

TuxBridge speaks JSON-RPC/LSP over stdio to actual language servers.

- `GET /v1/lsp/servers` — configured/built-in mappings and executable availability;
- `POST /v1/lsp/definition` — go to definition;
- `POST /v1/lsp/references` — semantic references;
- `POST /v1/lsp/hover` — types/signatures/documentation;
- `POST /v1/lsp/document-symbols` — semantic document outline;
- `POST /v1/lsp/workspace-symbols` — indexed workspace search;
- `POST /v1/lsp/diagnostics` — language-server diagnostics;
- `POST /v1/lsp/rename` — semantic rename with preview/apply;
- `POST /v1/lsp/format` — language-server formatting with preview/apply.

Agent-facing source coordinates are 1-based Unicode-scalar positions. TuxBridge converts them to LSP UTF-16 positions and converts returned ranges back to checked UTF-8 byte offsets without splitting surrogate pairs or UTF-8 code points.

Built-in conventional executable mappings exist for rust-analyzer, gopls, TypeScript Language Server, Pyright, Intelephense, JDTLS, and Kotlin Language Server. They are mappings, not bundled executables; `/v1/lsp/servers` reports which are really installed. Additional stdio language servers can be configured in TOML.

Semantic rename and formatting never make the language server the authorization boundary. Returned WorkspaceEdits are validated for in-workspace regular files, symlinks are rejected, UTF-16 ranges are checked, overlapping edits are rejected, all target files are preflighted before writes, and each final replacement is atomic per file. LSP resource create/delete/rename operations are currently rejected instead of being implicitly trusted.

## Coding sessions and rollback

Coding sessions make a coherent agent task observable and recoverable.

- `POST /v1/code/sessions` — create a session;
- `GET /v1/code/sessions` and `GET /v1/code/sessions/{id}` — inspect sessions;
- `POST /v1/code/sessions/{id}/checkpoint` — explicitly snapshot files before a non-session-aware mutation path;
- `POST /v1/code/sessions/{id}/refresh` — update expected after-hashes after external/session-unaware edits;
- `POST /v1/code/sessions/{id}/finalize` — mark successful work complete;
- `POST /v1/code/sessions/{id}/rollback` — restore session before-images safely.

Rollback is deliberately conservative. Before restoring anything, every tracked file must still have the exact hash TuxBridge recorded as the agent's last written version. If a human or another process changed even one tracked file afterward, rollback aborts rather than overwriting newer work.

Sessions are in-memory and bounded: they are a safety/coordination mechanism for a running TuxBridge instance, not a replacement for Git history.

## Verification planning and structured diagnostics

`POST /v1/code/verification-plan` divides recommended checks into `fast` and `full` groups based on project manifests and changed paths. It currently understands useful patterns for Rust, Go, JavaScript/TypeScript package managers, and Python. Planning never executes project code by itself.

Command execution results and background-job snapshots now include normalized diagnostics where common compiler/test output can be parsed into:

```json
{
  "severity": "error",
  "tool": "rustc",
  "path": "src/main.rs",
  "line": 42,
  "column": 9,
  "code": "E0308",
  "message": "mismatched types"
}
```

Text normalization supports common Rust, TypeScript-style, Go/PHP-style, and Python/pytest output. It is a convenience layer over command output; LSP diagnostics remain the semantic source when a language server is available.

Background jobs also emit stdout/stderr chunks through the live event bus, so an agent/operator can see build progress before the command exits.

## Recommended agent loop

1. Read `/v1/code/map` and Git status to orient in the repository.
2. Create a coding session for non-trivial work.
3. Query `/v1/lsp/servers` and use LSP semantics whenever available.
4. Use Tree-sitter structure/node-at for exact syntactic boundaries instead of guessing line ranges.
5. Read focused context and hashes for target files.
6. Dry-run semantic or edit-plan changes.
7. Apply edits with the session ID when using `/v1/code/edit-plan`; checkpoint/refresh around mutation paths that are not session-aware.
8. Inspect Git diff and language-server diagnostics.
9. Ask `/v1/code/verification-plan` for fast checks, execute them through the permitted command API, and consume structured diagnostics/live output.
10. Iterate on failures, then run the appropriate full checks.
11. Finalize the session when successful, or use hash-safe rollback if the change should be abandoned.
12. Commit/push only through configured Git capabilities and optional human approval gates.

## Why the layers stay separate

- **Repository map** reduces blind exploration.
- **Tree-sitter** owns concrete syntax structure.
- **LSP/compiler tooling** owns semantic truth.
- **TuxBridge** owns workspace policy, hashes, writes, rollback, command limits, Git safety, auditability, and human supervision.

Keeping those responsibilities separate makes the system both more accurate and easier to reason about than building one enormous makeshift parser/editor endpoint.
