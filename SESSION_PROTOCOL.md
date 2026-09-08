# Agent Session Protocol

TuxBridge's agent-facing API is session-first. A session is the durable unit of work that binds an agent task to exactly one configured workspace and gives the control plane a stable place to attach task memory, todos, file changes, jobs, approvals, and activity.

## Core invariants

- Workspace discovery happens before a session exists.
- Selecting a workspace creates exactly one session and returns an immutable readable session reference.
- Agent-facing workspace operations must eventually take `session`, not a caller-supplied workspace name. TuxBridge resolves the bound workspace server-side.
- The bearer API key authenticates the caller. A session reference is an identifier and continuity handle, not a second credential.
- The canonical session reference is readable and stable: `<workspace>--<original-title-slug>--<short-id>`.
- A unique short suffix may be used to resume a session, but every response returns the canonical full reference.
- Session title may become editable later; changing display metadata must never change the session reference.
- Task summary, plan, and todos are explicit session state. They are not reconstructed from audit/event logs.
- File rollback remains hash-guarded: TuxBridge refuses to overwrite a file that changed after the session last wrote it.

## New-task flow

1. `findWorkspaces` discovers candidate configured workspaces.
2. `selectWorkspace` chooses one workspace, supplies a concise session title, and creates the session.
3. The agent immediately records its interpretation of the requested outcome with `setSummary`.
4. The agent investigates the repository using session-bound read/search/semantic tools.
5. Once it understands the implementation, it stores the implementation strategy with `setPlan`.
6. The agent uses `todo` to track mutable execution state while it works.
7. Every subsequent agent-facing operation supplies the returned `session` reference.
8. `completeSession` finalizes the task with an optional result summary.

The ordering of summary and plan is intentional: summary captures what the user wants; plan should be written after enough repository evidence exists to choose a correct implementation.

## Resume flow

A later GPT instance can call `chooseSession` with the canonical session reference or a unique short suffix. TuxBridge returns the current session package: workspace binding, task summary, plan, todos, baseline, touched files, current file hashes, and resume warnings.

The resumed agent should reconcile the returned state with the repository before making new changes. A resume warning is evidence that external state moved; it is not an instruction to discard or overwrite that state.

## Plan vs todo

`plan` is the stable high-level implementation strategy and may be free-form Markdown.

`todo` is the mutable execution checklist. The current todo actions are:

- `list`
- `add`
- `update`
- `remove`
- `reorder`

Todo statuses are `pending`, `in_progress`, `done`, `blocked`, and `skipped`.

## Baseline and provenance

Session creation records safe Git orientation metadata when the workspace allows Git reads: branch, HEAD, and the Git porcelain changes that already existed when the session began. This lets the control plane distinguish session work from pre-existing state and warn a resumed agent when repository HEAD moved.

The existing code-edit session integration still records before/after file hashes and rollback snapshots. The new readable session reference is also the identifier used by those mechanisms.

## Current migration state

This protocol is being introduced alongside the legacy workspace-oriented API so existing clients keep working while agent tools migrate to session-bound wrappers.

The first milestone implements session bootstrap and explicit task memory. Low-level filesystem, code, command, LSP, and Git endpoints still accept workspace arguments directly; those are internal/legacy surfaces until corresponding compact agent tools are added.

Session metadata is currently process-memory state. Persistence across daemon restarts is intentionally a follow-up milestone because the control-plane model also needs durable operations, approvals, and event history; those should share one coherent state store instead of persisting only part of the session model in an ad-hoc format.

## API docs

The new agent-session OpenAPI document is served at `/openapi-agent-v2.yaml` and rendered through Scalar at `/docs`.
