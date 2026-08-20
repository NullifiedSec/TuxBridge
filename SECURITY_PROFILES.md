# TuxBridge security profiles

TuxBridge has three explicit runtime profiles. The selected profile is enforced by the server and should also match the installation policy for the `tuxbridge` Unix account.

## `default`

- TuxBridge runs as the dedicated `tuxbridge` user.
- No sudo privileges are granted.
- Structured tools still require their normal workspace capability flags.
- Raw command execution accepts only simple whitespace-separated commands whose executable is in `security.default_command_allowlist`.
- Shell metacharacters, chaining, substitutions, redirection, globs, and multiline scripts are rejected.

This is the recommended profile.

## `loose`

- TuxBridge runs as the dedicated `tuxbridge` user.
- No sudo privileges are granted by the installer.
- Raw commands execute through `/bin/bash -lc`.
- The AI can therefore do anything the `tuxbridge` Unix account itself can do, including accessing network resources and files allowed by normal Unix permissions.

This is intentionally equivalent to handing the API client an interactive shell as the `tuxbridge` account, except stdin is not interactive and command execution remains timeout/output bounded.

## `i_want_to_nuke_my_server`

- Same runtime behavior as `loose`.
- The installer creates `/etc/sudoers.d/tuxbridge` containing passwordless `sudo` permission for all commands.
- The installer requires the administrator to type `NUKE` before enabling it.

This profile is effectively remote root command execution for anyone who possesses the TuxBridge API key. Treat the API key as a root credential, require HTTPS, restrict network exposure, and use this profile only when that is explicitly the intended trust model.

## Unix account model

The installer creates a normal dedicated account with:

- username `tuxbridge`;
- home directory `/home/tuxbridge`;
- shell `/bin/bash`;
- no sudo/admin group membership added by TuxBridge;
- service execution under `User=tuxbridge` and `Group=tuxbridge`.

The account remains subject to ordinary Unix ownership, mode bits, ACLs, groups, firewall rules, and any MAC policy such as AppArmor or SELinux.
