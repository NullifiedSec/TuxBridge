# TuxBridge

TuxBridge is a capability-scoped HTTP bridge intended to expose explicitly configured Linux host operations to ChatGPT Actions.

The project is intentionally conservative: the server decides what each workspace may do, and model-side tool definitions are never treated as an authorization boundary.

## Current bootstrap

The first implementation slice provides:

- TOML configuration
- bearer-token authentication sourced from an environment variable
- an unauthenticated health endpoint
- authenticated system metadata
- authenticated workspace listing and lookup
- per-workspace capability metadata

Filesystem mutation, command execution, and Git mutation are intentionally not implemented yet.

## Configure

Copy the example configuration and replace the workspace path:

```bash
cp tuxbridge.example.toml tuxbridge.toml
export TUXBRIDGE_API_KEY="$(openssl rand -hex 32)"
```

The default configuration path is `./tuxbridge.toml`. Override it with `TUXBRIDGE_CONFIG`.

## Run

```bash
cargo run
```

Then:

```bash
curl http://127.0.0.1:8787/health
curl -H "Authorization: Bearer $TUXBRIDGE_API_KEY" http://127.0.0.1:8787/v1/system
curl -H "Authorization: Bearer $TUXBRIDGE_API_KEY" http://127.0.0.1:8787/v1/workspaces
```

## Security model

Workspace roots must be absolute paths. Capability flags default to `false`; adding a workspace does not implicitly grant read, write, command, or Git access.

Future filesystem operations must resolve paths relative to a configured workspace and reject traversal and symlink escapes before touching user data.
