# Coding environment

TuxBridge exposes two complementary coding layers: deterministic editor primitives owned by TuxBridge, and authoritative semantic operations delegated to real Language Server Protocol implementations.

## Editor primitives

- `POST /v1/code/context` — read a numbered code window around a line/range. Returns the full-file SHA-256 so later edits can prove they are based on the same file version.
- `POST /v1/code/symbols` — lightweight language-aware outline. This remains intentionally heuristic and is useful as a cheap first pass, not compiler truth.
- `POST /v1/code/references` — bounded identifier-boundary search across a workspace or subtree, skipping generated/vendor directories and symlinks.
- `POST /v1/code/edit-plan` — multi-file edit preflight. Every file carries an expected SHA-256. `dry_run=true` returns hashes and compact diff previews without writing. All hashes are validated before the first write; each final replacement is same-directory atomic.
- `POST /v1/code/tasks` — discover likely check/test/lint/build/format tasks from project manifests without executing them.

`edit-plan` supports `replace_exact`, `replace_lines`, `insert_before`, and `insert_after`.

## Real semantic intelligence

TuxBridge now speaks JSON-RPC/LSP over stdio to actual language servers rather than trying to reproduce compiler behavior itself.

Semantic endpoints:

- `GET /v1/lsp/servers` — report configured and built-in language-server mappings plus executable availability;
- `POST /v1/lsp/definition` — go to definition;
- `POST /v1/lsp/references` — semantic references including declarations;
- `POST /v1/lsp/hover` — types, signatures, and language-server documentation;
- `POST /v1/lsp/document-symbols` — compiler/language-server document outline;
- `POST /v1/lsp/workspace-symbols` — indexed workspace symbol search;
- `POST /v1/lsp/diagnostics` — collect published diagnostics for an opened document;
- `POST /v1/lsp/rename` — semantic rename with preview/apply mode;
- `POST /v1/lsp/format` — language-server formatting with preview/apply mode.

Agent-facing source coordinates are 1-based Unicode-scalar line/column positions. TuxBridge converts them to LSP UTF-16 positions and converts returned UTF-16 ranges back to exact UTF-8 byte offsets without splitting surrogate pairs or UTF-8 code points.

### Built-in language-server executable mappings

When no TOML override matches an extension, TuxBridge knows the conventional stdio commands for:

- Rust — `rust-analyzer`;
- Go — `gopls serve`;
- TypeScript/JavaScript — `typescript-language-server --stdio`;
- Python — `pyright-langserver --stdio`;
- PHP — `intelephense --stdio`;
- Java — `jdtls`;
- Kotlin — `kotlin-language-server`.

These are mappings, not bundled executables. `/v1/lsp/servers` tells the agent which ones are actually available on the host.

Any other stdio language server can be configured through `[lsp.servers.<name>]` with custom argv, extensions, language ID, and initialization options. This is the intended integration point for tools such as clangd, Vue language tools, basedpyright, custom internal language servers, or future protocol adapters.

## Safe LSP workspace edits

Semantic rename and formatting do not give a language server unrestricted write access.

TuxBridge receives the LSP `WorkspaceEdit`, then:

1. rejects non-file URIs;
2. rejects edits escaping the configured workspace root;
3. rejects symlink targets;
4. currently rejects LSP resource create/delete/rename operations rather than applying destructive filesystem operations implicitly;
5. converts every UTF-16 range to checked UTF-8 byte boundaries;
6. rejects overlapping edits;
7. builds every changed file in memory;
8. records old/new SHA-256 values;
9. when applying, re-hashes every target before the first write so a changed file aborts the operation;
10. persists each changed file through same-directory atomic replacement while preserving permissions.

Cross-file writes are preflighted together but are not falsely described as a filesystem-wide atomic transaction.

## Recommended agent loop

1. Inspect project metadata and Git status.
2. Call `/v1/lsp/servers` to discover semantic engines available for the workspace.
3. Use cheap code context/outline tools to orient quickly.
4. Prefer LSP definition, hover, references, document symbols, and diagnostics whenever a real server is available.
5. Use semantic rename/format with `dry_run=true` first.
6. Review affected files and hashes.
7. Apply with `dry_run=false` while the workspace is still current.
8. Use the general hash-guarded edit plan for changes that are not representable as an LSP operation.
9. Inspect Git diff.
10. Discover and run appropriate project checks through the permitted command interface.
11. Re-read diagnostics and diff before commit.

## Why the layers stay separate

A language server is excellent at semantic questions but should not become TuxBridge's authorization boundary. TuxBridge owns workspace scoping, path validation, output limits, hashes, mutation policy, and auditability. The language server owns language semantics.

The lightweight text/outline layer also remains useful when a project has no language server installed, while LSP is the authoritative path when one exists.

## Future parser/index layer

A future fast syntax/index layer can use Tree-sitter or SCIP as a cache and navigation accelerator for huge repositories. It should complement rather than replace LSP: Tree-sitter is excellent for concrete syntax trees and exact syntactic ranges, while language servers/compiler indexes remain authoritative for type resolution, imports, overloads, references, and refactors.
