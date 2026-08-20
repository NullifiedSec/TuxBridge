# TuxBridge

TuxBridge is a capability-scoped HTTP bridge for exposing explicitly configured Linux host operations to ChatGPT Actions.

The server, not the model-side schema, is the authorization boundary. Workspaces opt into filesystem, command, and Git capabilities independently.

## Current API

- system metadata and tool availability
- doctor diagnostics
- workspace listing and capability inspection
- safe workspace-relative directory listing, stat, bounded UTF-8 reads, batch reads, and recursive content search
- full-file SHA-256 checks for optimistic concurrency
- atomic create/replace and targeted exact-match patching
- project manifest/package-manager inspection
- argv-based synchronous command execution
- cancellable background command jobs with timeout and bounded stdout/stderr
- structured Git status, branches, log, and diff
- Git fetch, fast-forward-only pull, add, commit, and non-force push
- OpenAPI schema for ChatGPT Actions

## Configure

```bash
cp tuxbridge.example.toml tuxbridge.toml
export TUXBRIDGE_API_KEY="$(openssl rand -hex 32)"
```

Edit `tuxbridge.toml` and replace the example workspace path. Capability flags default to `false`.

The default configuration path is `./tuxbridge.toml`. Override it with `TUXBRIDGE_CONFIG`.

## Run

```bash
cargo run --release
```

Then inspect the service:

```bash
curl http://127.0.0.1:8787/health
curl -H "Authorization: Bearer $TUXBRIDGE_API_KEY" http://127.0.0.1:8787/v1/doctor
```

## ChatGPT Action

`openapi.yaml` contains the current Action schema. Replace its placeholder server URL with the HTTPS URL that fronts your TuxBridge instance, then configure the GPT Action to use Bearer API-key authentication with the same key supplied through `TUXBRIDGE_API_KEY`.

## Security model

Workspace filesystem operations reject absolute paths and parent traversal. Existing paths are canonicalized, symlink escapes are rejected, recursive search does not follow symlinks, and filesystem replacements/patches use hash guards plus same-directory atomic persistence.

File mutations are limited to 8 MiB. Targeted patches require exactly one occurrence of the old text and a current full-file SHA-256.

Git push intentionally has no force option. Pull uses `--ff-only`.

### Command execution warning

`commands = true` grants process execution using the privileges of the OS account running TuxBridge. The workspace controls the process working directory; it is **not an OS filesystem sandbox**. Run TuxBridge as a dedicated low-privilege service user and expose only the workspaces that account should access.

Command requests are argv arrays rather than implicit shell strings, have a 120-second default timeout (900-second hard maximum), and retain at most 2 MiB each of stdout and stderr while continuing to drain excess output.

## Verification

CI is configured to run formatting checks, Clippy with warnings denied, tests, and a release build. Until that CI or a local Rust toolchain has run successfully, treat repository state as provisionally rather than compiler-verified green.
