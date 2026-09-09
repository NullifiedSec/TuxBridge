# Operations and approvals

TuxBridge models every side-effecting agent tool call as a durable operation.
The operation freezes the canonical session, resolved workspace, tool name, and exact JSON arguments before policy is evaluated.

Operation policy values are:

- `automatic`: execute immediately, but still persist the operation and its result.
- `confirm`: persist as `pending_approval`; execute only after the exact frozen operation is approved.
- `blocked`: persist as `blocked` and never execute.

The default is `automatic` for current mutating tools except `gitSync.push`, which defaults to `confirm`.
Policies can be overridden in `[approvals.agent_tools]`.

A pending operation is approved through `POST /v1/operations/{id}/approve` or denied through `POST /v1/operations/{id}/deny`.
Approval changes only the operation state; its tool and arguments cannot be edited.
Agents use `waitForOperation` for bounded long polling. If the operation is still pending/executing when the wait window closes, the same operation id is returned and the caller can retry safely.

Operation status values are `pending_approval`, `executing`, `completed`, `failed`, `denied`, and `blocked`.

Operation records persist under the TuxBridge state directory in `operations/`. A daemon restart reloads pending and terminal operations. An operation found in `executing` state after restart is marked failed/uncertain and is not automatically replayed, avoiding duplicate commands, commits, pushes, or edits.

Read-only agent tools do not create operations; they continue to emit session-scoped tool events.
