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
- profile-aware raw command execution
- argv-based synchronous command execution and cancellable background jobs in unrestricted profiles
- structured Git status, branches, HEAD metadata, remotes, stash listing, log, and diff
- Git fetch, clean-tree fast-forward-only pull, explicit add, commit, and non-force push
- global request-body and concurrent-request limits
- request IDs, no-store/nosniff response headers, and body-free/header-free audit logging
- OpenAPI schema for ChatGPT Actions

## Install and onboarding

Build the release binary, then run the installer as root:

```bash
cargo build --release
sudo ./install.sh
```

The installer creates or safely reuses a dedicated normal Unix account named `tuxbridge`, with home `/home/tuxbridge`, and runs the systemd service as that user. It refuses to reuse an account with an unexpected home directory, UID 0, or supplementary groups.

Onboarding asks for one of three profiles:

1. **Default** — recommended. The service gets additional systemd sandboxing, only `/home/tuxbridge` and `/var/lib/tuxbridge` are writable through that sandbox, raw commands are restricted to a small non-shell-escape allowlist, and the general argv/background command APIs are blocked. Git write/network capabilities are disabled in the generated home workspace.
2. **Loose** — TuxBridge behaves like the `tuxbridge` Unix user itself. Raw commands run through Bash, the general command APIs are enabled, and no sudo privileges are added by TuxBridge.
3. **I_want_to_nuke_my_server** — Loose plus `/etc/sudoers.d/tuxbridge` granting `NOPASSWD: ALL`. Onboarding requires typing `NUKE`. Anyone holding the API key should be treated as having remote root-equivalent access.

The installer preserves the existing API key when switching profiles, rewrites the runtime config/systemd policy to match the selected profile, and validates the sudoers fragment with `visudo` before enabling the nuke profile.

See `SECURITY_PROFILES.md` for the detailed trust model.

## Configure manually

```bash
cp tuxbridge.example.toml tuxbridge.toml
export TUXBRIDGE_API_KEY="$(openssl rand -hex 32)"
```

Edit `tuxbridge.toml` and replace the example roots. Capability flags default to `false`; `security.profile` defaults to `default`.

The API key is read only from the environment variable named by `auth.api_key_env`. TuxBridge rejects keys shorter than 32 characters, keys with surrounding whitespace, and keys containing control characters. `tuxbridge.toml` is gitignored.

The default configuration path is `./tuxbridge.toml`. Override it with `TUXBRIDGE_CONFIG`.

## Run manually

```bash
cargo run --release
```

Then inspect the service:

```bash
curl http://127.0.0.1:8787/health
curl -H "Authorization: Bearer $TUXBRIDGE_API_KEY" http://127.0.0.1:8787/v1/doctor
```

Every response is assigned an `X-TuxBridge-Request-Id`. The service logs request ID, method, path, response status, and duration; it deliberately does not log authorization headers or request bodies.

## Command profiles

`POST /v1/commands/raw` is the profile-aware command endpoint. In Default, TuxBridge rejects shell grammar and executes only simple commands whose executable appears in `security.default_command_allowlist`. The shipped allowlist is intentionally boring: `pwd`, `ls`, `cat`, `head`, `tail`, `wc`, `grep`, `stat`, `du`, `df`, `uname`, `id`, and `whoami`.

Default blocks `/v1/commands/run` and `/v1/commands/start` at the HTTP policy boundary so they cannot bypass the raw-command allowlist. Loose and Nuke enable those APIs and execute raw command strings through `/bin/bash --noprofile --norc -lc` with null stdin, configured timeouts, and bounded output capture.

The command workspace controls the initial working directory, not containment. Loose and Nuke commands can access anything the `tuxbridge` OS account can access; Nuke can additionally use passwordless `sudo`.

## Bonus inspection endpoints

The bonus host-inspection tools are intentionally read-only and bounded. Process inspection returns PID/name/state/UID but not argv or environment variables, because those frequently contain secrets. TCP listener inspection uses `ss -lnt` and disk reporting uses `df -P -B1`; `doctor` reports when those helpers are missing.

Workspace tree summaries do not follow symlinks and enforce depth/entry caps. Git bonus reads expose current HEAD/upstream, configured remote URLs, and stash metadata without mutating repository state.

## ChatGPT Action

`openapi.yaml` contains the Action schema, including profile-aware raw command execution. Replace its placeholder server URL with the HTTPS URL that fronts your TuxBridge instance, then configure the GPT Action to use Bearer API-key authentication with the same key supplied through `TUXBRIDGE_API_KEY`.

TuxBridge should normally listen only on loopback and sit behind a TLS reverse proxy. See `deploy/` for Caddy and systemd examples; `install.sh` generates the profile-appropriate service unit during normal installation.

## Security model

### Filesystem

Workspace and user-files operations reject absolute client paths and parent traversal. Existing paths are canonicalized and checked against the configured root. Mutations reject symlink targets, require canonical parents, and persist through same-directory temporary files. Replace/patch operations require a current full-file SHA-256 so stale agent reads fail with `409 Conflict` instead of overwriting newer work.

Workspace and user-files mutations are currently limited to 8 MiB per file. Search does not follow symlinks.

### Command execution

Command execution always uses the privileges of the OS account running TuxBridge. It is **not an application-level OS sandbox**. Default adds a systemd sandbox during installation; Loose deliberately removes that extra filesystem policy so the agent behaves like its Unix account.

Timeout/cancellation kills the spawned child, but is not yet a cgroup-backed guarantee that all detached grandchildren are terminated.

### Git

Git push intentionally has no force option. Pull uses `--ff-only` and refuses to run on a dirty worktree. Mutation commands disable Git hooks and fsmonitor and set `GIT_TERMINAL_PROMPT=0`.

Git remains execution-adjacent: repository filters, credential helpers, SSH, and remote helpers can execute programs as part of normal Git behavior. Treat `git_write` and especially `git_network` as privileged capabilities and expose only repositories you trust to the TuxBridge service account.

### Network exposure

Do not expose the Rust listener directly to the public Internet. Bind it to `127.0.0.1`, put Caddy/nginx/etc. in front for TLS, firewall the host, and keep the API key out of config files and shell history where practical.

See `SECURITY.md` and `SECURITY_PROFILES.md` for the threat model and deployment assumptions.

## Verification

CI is configured to run:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
cargo build --release
```

Until CI or a local Rust toolchain has successfully run on the current commit, treat repository state as provisionally rather than compiler-verified green.
