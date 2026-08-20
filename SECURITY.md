# Security model

TuxBridge deliberately exposes privileged host operations. Its safety boundary is the Rust server plus the operating-system account that runs it, not the GPT instructions or OpenAPI document.

## Trust assumptions

- The TuxBridge host administrator controls `tuxbridge.toml` and `TUXBRIDGE_API_KEY`.
- Configured workspace roots and user-files roots are explicit trust decisions.
- The service account is expected to be low privilege and restricted to the files/repositories it should access.
- Public clients must reach TuxBridge through HTTPS.
- Repositories with Git write/network capabilities are trusted enough to allow Git to process their configuration and attributes.

## Filesystem boundary

Client paths are relative to configured roots. Absolute paths and parent traversal are rejected. Existing targets are canonicalized and must remain under the configured root. Mutation targets require a canonical in-root parent and reject symlink targets.

Atomic same-directory replacement and SHA-256 preconditions reduce partial writes and stale-agent overwrites. These controls do not replace ordinary Unix permissions; the service user must still be appropriately restricted.

## Command boundary

The command API does not invoke a shell implicitly and accepts argv arrays only. This prevents shell metacharacters in arguments from becoming syntax by accident.

It is not a sandbox. A permitted executable can access anything available to the TuxBridge OS account and can make network connections permitted by the host. Use a dedicated service account, OS permissions, firewalling, namespaces/containers, or MAC systems such as AppArmor/SELinux if a stronger execution boundary is required.

Timeout/cancellation kills the spawned child. It is not currently a guaranteed process-tree/cgroup kill for arbitrary grandchildren. Do not treat it as containment for hostile programs.

## Git boundary

Force push is not exposed. Pull is fast-forward-only and requires a clean worktree. Git mutation commands disable hooks and fsmonitor and disable interactive Git credential prompting.

Git can still execute configured clean/smudge filters, credential helpers, SSH, remote helpers, and similar integrations. Therefore Git mutation/network capabilities are execution-adjacent and should only be enabled on trusted repositories.

## API boundary

- Bearer API key is required for every `/v1/*` endpoint.
- API keys shorter than 32 characters or containing surrounding whitespace/control characters are rejected at startup.
- Request bodies and concurrent requests are capped.
- Audit logs contain request metadata only, not request bodies or authorization headers.
- Responses include `Cache-Control: no-store` and `X-Content-Type-Options: nosniff`.
- `/health` is intentionally unauthenticated and reveals only service name/version/status.

## Explicit non-goals in v0.1

TuxBridge does not currently provide:

- per-client identities or OAuth authorization scopes;
- an OS sandbox for commands;
- cgroup-backed process-tree lifetime enforcement;
- encrypted storage for secrets;
- multi-tenant isolation;
- arbitrary force-push/reset/clean/stash operations.

Those should not be inferred from the presence of capability flags.
