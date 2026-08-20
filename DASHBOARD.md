# TuxBridge Mission Control

TuxBridge ships a zero-build supervision UI at `GET /ui`.

The HTML shell itself is unauthenticated and contains no host data or secrets. All useful data is loaded from authenticated `/v1/*` APIs after the operator supplies the same Bearer API key used by ChatGPT Actions. The browser keeps the key in `sessionStorage`; it is not embedded into the page or persisted to `localStorage`.

## Push-live event stream

Mission Control no longer polls for agent activity. It opens an authenticated streaming `fetch()` to `GET /v1/events/stream` and consumes Server-Sent Events from TuxBridge's typed in-memory event bus. Native `EventSource` is intentionally not used because it cannot attach the required Authorization header.

The same event hub keeps a bounded snapshot at `GET /v1/events` so the UI can recover recent context after reconnecting. Static inventory such as workspaces, LSP readiness, jobs, sessions, and approvals is refreshed occasionally; high-volume activity is pushed immediately.

Representative event kinds include:

- `request.started` / `request.finished`;
- `command.started` / `command.finished`;
- `job.started`, `job.stdout`, `job.stderr`, `job.finished`, and cancellation requests;
- coding-session lifecycle events;
- edit-plan and structural-edit previews/applies;
- repository-map and verification-plan activity;
- approval required/approved/denied events.

Event payloads must remain metadata-oriented. Request bodies and authorization headers are not copied into the generic request/audit feed. Command output is emitted only on explicit job stdout/stderr events because live command observation is the purpose of those events.

## Current panels

- active security profile;
- configured workspaces and enabled capabilities;
- installed/configured LSP readiness;
- live agent timeline over SSE;
- live background-command output;
- background jobs and structured-diagnostic counts;
- coding sessions and hash-safe rollback controls;
- pending approval gates with explicit Approve/Deny controls;
- per-workspace Git diff inspection;
- compact repository-map inspection.

TuxBridge intentionally does not require a frontend build chain, Node, npm, or a separate web service: `web/dashboard.html` is embedded into the Rust binary with `include_str!`.

## Human approval gates

`[approvals].required_paths` can name exact mutation API paths that require operator approval. A protected call without a consumed approval returns HTTP 428 and an `approval_id`. Mission Control can approve or deny that ID. The caller then retries the exact operation with:

```text
X-TuxBridge-Approval-Id: approval-...
```

An approved token is one-shot and path-bound. Approval-management endpoints cannot sensibly be placed behind their own gate.

Useful examples in unrestricted profiles include `/v1/git/push` or `/v1/commands/raw`. Approval gates are optional and empty by default; they complement, rather than replace, workspace capabilities and security profiles.

## Audit feed

`GET /v1/audit/events` remains a bounded request-oriented ring containing request ID, method, path, HTTP status, duration, and timestamp. It is distinct from the richer typed event stream.

Both rings are observability state, not durable security logs. They are lost when TuxBridge restarts. Durable audit logging remains the responsibility of journald/log aggregation or a future explicit persistence backend.

## Why SSE instead of WebSocket

Most Mission Control traffic is daemon-to-browser observation. SSE is simpler, reconnect-friendly, HTTP-native, and sufficient for that direction. TuxBridge should reserve WebSockets for a future feature that genuinely needs full-duplex interaction, such as an interactive PTY. The coding agent does not need a WebSocket merely to receive build progress or activity events.

## Why web first

A browser is better for reviewing diffs, long diagnostics, workspace state, approvals, and multiple concurrent activities than a terminal-only UI. A future TUI should consume the same authenticated APIs and SSE stream rather than growing a second source of truth inside the daemon.
