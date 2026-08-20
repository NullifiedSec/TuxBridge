# TuxBridge

TuxBridge is a capability-scoped HTTP bridge for exposing explicitly configured Linux host operations to ChatGPT Actions.

The server, not the model-side schema, is the authorization boundary. Workspaces opt into filesystem, command, and Git capabilities independently; separately configured `user_files` mounts expose only read/write-style filesystem operations.

## v0.1 alpha surface

- system metadata and common development-tool availability
- bounded tool-version probing for Git, Rust, Node, Bun, Go, Python, PHP, Docker, and friends
- process metadata inspection without exposing command lines or environment variables
- TCP listener inspection and mounted-filesystem capacity reporting
- doctor diagnostics
- workspace listing, metadata, capabilities, safe path resolution, and bounded tree summaries
- safe workspace-relative directory listing, stat, bounded UTF-8 reads, batch reads, and recursive content search
- full-file SHA-256 checks for optimistic concurrency
- atomic create/replace and targeted exact-match patching
- separate explicitly configured user-files mounts with list/stat/read/hash/write/patch operations
- project manifest/package-manager inspection
- argv-based synchronous command execution
- cancellable background command jobs with timeout, retention, job-count limits, and bounded stdout/stderr
- structured Git status, branches, HEAD metadata, remotes, stash listing, log, and diff
- Git fetch, clean-tree fast-forward-only pull, explicit add, commit, and non-force push
- global request-body and concurrent-request limits
- request IDs, no-store/nosniff response headers, and body-free/header-free audit logging
- OpenAPI schema for ChatGPT Actions

## Configure

```bash
cp tuxbridge.example.toml tuxbridge.toml
export TUXBRIDGE_API_KEY="$(openssl rand -hex 32)"
```

Edit `tuxbridge.toml` and replace the example roots. Capability flags default to `false`.

The API key is read only from the environment variable named by `auth.api_key_env`. TuxBridge rejects keys shorter than 32 characters, keys with surrounding whitespace, and keys containing control characters. `tuxbridge.toml` is gitignored.

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

Every response is assigned an `X-TuxBridge-Request-Id`. The service logs request ID, method, path, response status, and duration; it deliberately does not log authorization headers or request bodies.

## Bonus inspection endpoints

The bonus host-inspection tools are intentionally read-only and bounded. Process inspection returns PID/name/state/UID but not argv or environment variables, because those frequently contain secrets. TCP listener inspection uses `ss -lnt` and disk reporting uses `df -P -B1`; `doctor` reports when those helpers are missing.

Workspace tree summaries do not follow symlinks and enforce depth/entry caps. Git bonus reads expose current HEAD/upstream, configured remote URLs, and stash metadata without mutating repository state.

## ChatGPT Action

`openapi.yaml` contains the Action schema. Replace its placeholder server URL with the HTTPS URL that fronts your TuxBridge instance, then configure the GPT Action to use Bearer API-key authentication with the same key supplied through `TUXBRIDGE_API_KEY`.

TuxBridge should normally listen only on loopback and sit behind a TLS reverse proxy. See `deploy/` for hardened systemd and Caddy examples.

## Security model

### Filesystem

Workspace and user-files operations reject absolute client paths and parent traversal. Existing paths are canonicalized and checked against the configured root. Mutations reject symlink targets, require canonical parents, and persist through same-directory temporary files. Replace/patch operations require a current full-file SHA-256 so stale agent reads fail with `409 Conflict` instead of overwriting newer work.

Workspace and user-files mutations are currently limited to 8 MiB per file. Search does not follow symlinks.

### Command execution

`commands = true` grants process execution using the privileges of the OS account running TuxBridge. The workspace controls the process working directory; it is **not an OS filesystem sandbox**.

Commands are argv arrays, not implicit shell strings. They receive null stdin, bounded stdout/stderr capture, configurable timeout limits, and `kill_on_drop`. Background jobs are capped and completed jobs expire from the in-memory job store.

Run TuxBridge as a dedicated low-privilege service user and give that account access only to the paths it should touch.

### Git

Git push intentionally has no force option. Pull uses `--ff-only` and refuses to run on a dirty worktree. Mutation commands disable Git hooks and fsmonitor and set `GIT_TERMINAL_PROMPT=0`.

Git remains execution-adjacent: repository filters, credential helpers, SSH, and remote helpers can execute programs as part of normal Git behavior. Treat `git_write` and especially `git_network` as privileged capabilities and expose only repositories you trust to the TuxBridge service account.

### Network exposure

Do not expose the Rust listener directly to the public Internet. Bind it to `127.0.0.1`, put Caddy/nginx/etc. in front for TLS, firewall the host, and keep the API key out of config files and shell history where practical.

See `SECURITY.md` for the threat model and deployment assumptions.

## Verification

CI is configured to run:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
cargo build --release
```

Until CI or a local Rust toolchain has successfully run on the current commit, treat repository state as provisionally rather than compiler-verified green.
