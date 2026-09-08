2026-09-09 - src/sessions/support.rs - import HashMap for session reference resolution - fixes a compile blocker introduced by the session module split
2026-09-09 - src/sessions/agent.rs - use Option::is_none_or for workspace filtering - avoids a new Clippy warning in the session API
2026-09-09 - src/sessions/persistence.rs - quarantine corrupt manifests and garbage-collect orphan snapshots - keeps healthy sessions recoverable and bounds leaked persistence storage
