# Multi-GPT roles

TuxBridge supports multiple GPT occupations behind one daemon and one HTTPS endpoint. The split is both an OpenAPI ergonomics boundary and a server-side authorization boundary.

## Shipped occupations

### Eris Dev

Schema: `openapi-dev.yaml`

Credential: `TUXBRIDGE_DEV_API_KEY`

Role: `developer`

Exactly 30 GPT operations. It is optimized for the normal software-engineering loop: repository orientation, focused code reads, Tree-sitter structure, LSP semantics, hash-guarded edits, coding sessions/rollback, verification commands, diff/history, stage/commit/push.

The deliberately omitted heuristic symbol-outline endpoint is redundant with Tree-sitter and LSP and would consume one of the GPT Actions 30-operation slots.

### Eris Review

Schema: `openapi-review.yaml`

Credential: `TUXBRIDGE_REVIEW_API_KEY`

Role: `reviewer`

Read-only investigation/review surface. It can inspect projects, code context and structure, LSP semantics/diagnostics, and Git history/diffs/metadata. The server rejects file mutations, semantic edits, commands, commits and pushes for this role even if somebody manually calls those endpoints outside the GPT UI.

### Eris Ops

Schema: `openapi-ops.yaml`

Credential: `TUXBRIDGE_OPS_API_KEY`

Role: `operator`

Exactly 30 GPT operations. It is optimized for host supervision and operations: Mission Control activity/approvals, system/process/listener/disk diagnostics, workspace file maintenance, foreground/background commands and jobs, plus operational Git status/branch/fetch/pull flows.

The operator role also receives a small number of **support routes** that do not appear as GPT Actions and therefore do not consume operation slots. These are read-only endpoints needed by Mission Control, such as the SSE event stream, LSP/session inventory, audit reads, and Git diff viewing.

## Administrator credential

`TUXBRIDGE_API_KEY` remains the backwards-compatible administrator credential. It is implicitly assigned the `admin` role and can call the complete TuxBridge API. Existing installations therefore do not lose access when upgrading to role-scoped principals.

Use the administrator key for emergency/manual access. Prefer occupation keys in GPT Actions so a compromised or misdirected GPT has a smaller server-side blast radius.

## One authorization source of truth

`openapi.yaml` is the canonical complete Action API description.

`openapi-profiles.json` contains two concepts per occupation:

- `operations` — canonical `operationId` values exposed to that GPT; these are limited to 30;
- `support_routes` — explicit method/path permissions for non-Action clients such as Mission Control; these do not count toward the GPT operation limit.

TuxBridge embeds both the canonical OpenAPI document and profile manifest into the binary at compile time and constructs its role policy from them during startup. Startup fails if a profile references a missing operation, duplicates an operation, uses an unknown profile, or exceeds 30 GPT operations.

This means the GPT schema split is not cosmetic. The same occupation manifest that defines the Action surface controls which authenticated role may invoke which HTTP method/path.

Dynamic OpenAPI paths such as `/v1/jobs/{id}` are matched as exactly one path segment at runtime.

## Schema generation and validation

Run:

```bash
python3 scripts/generate_openapi_profiles.py
```

to regenerate standalone occupation schemas by filtering canonical `openapi.yaml` through the `operations` arrays in `openapi-profiles.json`.

CI runs:

```bash
python3 scripts/generate_openapi_profiles.py --check
```

which verifies that each committed occupation schema exposes exactly the manifest operation set, every route/method matches canonical OpenAPI, and no profile exceeds the 30-operation limit. The checker accepts both the hand-readable YAML form and the JSON-compatible YAML emitted by the generator.

## Configuration

A manual configuration can define principals like this:

```toml
[auth]
api_key_env = "TUXBRIDGE_API_KEY"

[auth.principals.eris_dev]
api_key_env = "TUXBRIDGE_DEV_API_KEY"
roles = ["developer"]

[auth.principals.eris_review]
api_key_env = "TUXBRIDGE_REVIEW_API_KEY"
roles = ["reviewer"]

[auth.principals.eris_ops]
api_key_env = "TUXBRIDGE_OPS_API_KEY"
roles = ["operator"]
```

A principal may have multiple roles; permissions are the union of those roles. Duplicate role declarations, duplicate resolved keys, empty role lists, and malformed environment-variable names are rejected during startup.

## Installer behavior

`install.sh` preserves existing credentials and adds missing role keys on upgrade:

```text
TUXBRIDGE_API_KEY
TUXBRIDGE_DEV_API_KEY
TUXBRIDGE_REVIEW_API_KEY
TUXBRIDGE_OPS_API_KEY
```

Retrieve them with:

```bash
sudo cat /etc/tuxbridge/tuxbridge.env
```

Use each occupation key with its matching OpenAPI schema in the GPT Builder.

## Security-profile interaction

Role authorization and the global TuxBridge security profile are separate layers.

For example, `developer` is authorized for `runCommand`, but the global `default` profile still blocks `/v1/commands/run`. A Dev GPT that needs builds/tests through that endpoint therefore requires a suitable workspace plus `loose` or `i_want_to_nuke_my_server` profile. Role authorization never upgrades workspace capabilities or OS permissions.

Approval gates are evaluated only after successful authentication and role authorization. Unauthenticated traffic cannot create pending approval records.
