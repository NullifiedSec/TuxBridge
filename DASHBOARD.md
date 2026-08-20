# TuxBridge Mission Control

TuxBridge ships a zero-build monitoring UI at `GET /ui`.

The HTML shell itself is unauthenticated and contains no host data or secrets. All useful data is loaded from authenticated `/v1/*` APIs after the operator supplies the same Bearer API key used by ChatGPT Actions. The browser keeps the key in `sessionStorage`; it is not embedded into the page or persisted to `localStorage`.

## Current panels

- active security profile;
- configured workspaces and enabled capabilities;
- installed/configured LSP readiness;
- bounded live request activity from the in-memory audit ring;
- per-workspace Git diff inspection.

The dashboard polls every two seconds. TuxBridge intentionally does not require a frontend build chain, Node, npm, or a separate web service: `web/dashboard.html` is embedded into the Rust binary with `include_str!`.

## Audit feed

`GET /v1/audit/events` returns the bounded in-memory request ring. Events include request ID, method, path, HTTP status, duration, and timestamp. Request bodies and authorization headers are never stored in the feed.

The feed is observability state, not a durable security log. It is lost when TuxBridge restarts. Durable audit logging remains the responsibility of journald/log aggregation or a future explicit persistence backend.

## Why web first

A browser is better for reviewing diffs, long diagnostics, workspace state, and multiple concurrent activities than a terminal-only UI. A future TUI should consume the same authenticated monitoring APIs rather than growing a second source of truth inside the daemon.
