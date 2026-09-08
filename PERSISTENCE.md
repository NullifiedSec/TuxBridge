# Durable Session State

TuxBridge sessions are durable control-plane state. They must survive GPT replacement, browser loss, daemon restart, and host reboot without writing bookkeeping files into user workspaces.

## State location

TuxBridge resolves its state root in this order:

1. `TUXBRIDGE_STATE_DIR` when explicitly configured.
2. `/var/lib/tuxbridge` when that installed service state directory exists.
3. `$XDG_STATE_HOME/tuxbridge` for a user-mode development run.
4. `$HOME/.local/state/tuxbridge` when `XDG_STATE_HOME` is unset.
5. `/var/lib/tuxbridge` as the final fallback.

The installer already creates `/var/lib/tuxbridge` owned by the `tuxbridge` service user, and the default systemd sandbox allows writes there.

## Layout

Each session owns one directory beneath the state root:

```text
<state-root>/
└── sessions/
    └── <readable-session-ref>/
        ├── session.json
        └── snapshots/
            └── <sha256>.bin
```

`session.json` contains session metadata only: workspace binding, title, summary, plan, result summary, status, Git baseline, todos, timestamps, and rollback snapshot metadata.

Rollback `before` bytes are stored separately as content-addressed blobs. The blob filename is the SHA-256 of its content. Snapshot bytes are not loaded into memory during daemon startup; they are read and hash-verified only when rollback needs them.

## Write guarantees

Session manifests are serialized to a same-directory temporary file, flushed, `fsync`ed, and atomically renamed over `session.json`. Snapshot blobs are write-once and verified against their expected SHA-256 before they become part of the durable session manifest.

For in-memory mutations, TuxBridge keeps the previous `SessionRecord` until the new manifest has been persisted. If persistence fails, the in-memory mutation is restored and the API call fails rather than reporting state that will disappear after restart.

## Startup recovery

`AppState` opens the durable session store before serving requests. TuxBridge scans `sessions/*/session.json`, validates the persistence schema and snapshot metadata, and rebuilds the session index using the original readable session references.

Repository state is intentionally not trusted from the manifest on resume. Existing session views still recompute current file hashes and current Git HEAD, so `chooseSession` can warn when external work moved after the persisted session state was written.

A missing snapshot blob does not erase the session. The session remains resumable and reports a resume warning; rollback will fail rather than fabricate or overwrite missing state.

## Schema versioning

Every `session.json` includes `schema_version`. The current version is `1`. Unknown versions fail startup explicitly instead of being interpreted with incompatible assumptions.

Future durable operations, approvals, and control-plane events should reuse the same TuxBridge state root. They do not need to use the exact session manifest format.
