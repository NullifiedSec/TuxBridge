#!/usr/bin/env bash
set -euo pipefail

if [[ ${EUID:-$(id -u)} -ne 0 ]]; then
  echo "TuxBridge installer must run as root." >&2
  exit 1
fi

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
PROFILE_GENERATOR="$SCRIPT_DIR/scripts/generate_openapi_profiles.py"
USER_NAME="tuxbridge"
HOME_DIR="/home/${USER_NAME}"
CONFIG_DIR="/etc/tuxbridge"
STATE_DIR="/var/lib/tuxbridge"
BINARY_DST="/usr/local/bin/tuxbridge"
SERVICE_DST="/etc/systemd/system/tuxbridge.service"
SUDOERS_DST="/etc/sudoers.d/tuxbridge"
ENV_FILE="$CONFIG_DIR/tuxbridge.env"
BINARY_SRC="./target/release/tuxbridge"
SUBDOMAIN=""
PUBLIC_ORIGIN=""

usage() {
  cat <<'EOF'
Usage:
  sudo bash ./install.sh [BINARY_PATH] [-subdomain HOSTNAME]

Examples:
  sudo bash ./install.sh
  sudo bash ./install.sh ./target/release/tuxbridge
  sudo bash ./install.sh -subdomain tuxbridge.example.com
  sudo bash ./install.sh ./target/release/tuxbridge -subdomain tuxbridge.example.com

Options:
  -subdomain, --subdomain HOSTNAME
      Configure the public GPT Action origin for onboarding.
      The installer regenerates openapi-dev.yaml, openapi-review.yaml, and
      openapi-ops.yaml with https://HOSTNAME in their servers section, then
      prints a ready-to-paste Caddy reverse-proxy block.
      TuxBridge itself remains bound to 127.0.0.1:8787.

  -h, --help
      Show this help text.
EOF
}

positional_seen=false
while (( $# > 0 )); do
  case "$1" in
    -subdomain|--subdomain)
      if (( $# < 2 )); then
        echo "$1 requires a hostname." >&2
        usage >&2
        exit 2
      fi
      SUBDOMAIN="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    --)
      shift
      if (( $# > 1 )); then
        echo "Only one positional binary path is supported." >&2
        exit 2
      fi
      if (( $# == 1 )); then
        BINARY_SRC="$1"
        positional_seen=true
        shift
      fi
      ;;
    -*)
      echo "Unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
    *)
      if [[ "$positional_seen" == true ]]; then
        echo "Only one positional binary path is supported." >&2
        usage >&2
        exit 2
      fi
      BINARY_SRC="$1"
      positional_seen=true
      shift
      ;;
  esac
done

if [[ -n "$SUBDOMAIN" ]]; then
  # Accept DNS hostnames only. This is deliberately strict because the value is
  # rendered into both generated OpenAPI documents and a Caddyfile example.
  if (( ${#SUBDOMAIN} > 253 )) \
    || [[ "$SUBDOMAIN" == .* || "$SUBDOMAIN" == *. ]] \
    || [[ ! "$SUBDOMAIN" =~ ^([A-Za-z0-9]([A-Za-z0-9-]{0,61}[A-Za-z0-9])?\.)*[A-Za-z0-9]([A-Za-z0-9-]{0,61}[A-Za-z0-9])?$ ]]; then
    echo "Invalid subdomain/hostname: $SUBDOMAIN" >&2
    echo "Expected a DNS hostname such as tuxbridge.example.com" >&2
    exit 2
  fi
  SUBDOMAIN="${SUBDOMAIN,,}"
  PUBLIC_ORIGIN="https://$SUBDOMAIN"

  command -v python3 >/dev/null 2>&1 || {
    echo "python3 is required to generate GPT Action schemas when -subdomain is used." >&2
    exit 1
  }
  if [[ ! -f "$PROFILE_GENERATOR" ]]; then
    echo "OpenAPI profile generator not found: $PROFILE_GENERATOR" >&2
    echo "Run the installer from a complete TuxBridge checkout." >&2
    exit 1
  fi
fi

if [[ ! -x "$BINARY_SRC" ]]; then
  echo "Built binary not found or not executable: $BINARY_SRC" >&2
  echo "Build first with: cargo build --release" >&2
  exit 1
fi

cat <<'EOF'

TuxBridge security profile

  1) Default
     Strongly constrained. TuxBridge runs as its own Unix user, systemd exposes
     only its home/state as writable, and command execution is allowlisted.

  2) Loose
     TuxBridge becomes its Unix user. It receives the ordinary OS permissions of
     that account, with no sudo privileges added by TuxBridge.

  3) I_want_to_nuke_my_server
     Same as Loose, plus passwordless sudo for the tuxbridge user.
     Whoever controls an administrator/operator-capable key is effectively root.

EOF

read -r -p "Choose profile [1-3]: " choice
case "$choice" in
  1) profile="default" ;;
  2) profile="loose" ;;
  3)
    profile="i_want_to_nuke_my_server"
    echo
    echo "WARNING: this grants tuxbridge passwordless sudo for ALL commands."
    echo "Treat privileged TuxBridge API keys as root credentials."
    read -r -p "Type NUKE to continue: " confirm
    [[ "$confirm" == "NUKE" ]] || { echo "Cancelled."; exit 1; }
    ;;
  *) echo "Invalid profile." >&2; exit 1 ;;
esac

if id "$USER_NAME" >/dev/null 2>&1; then
  passwd_line="$(getent passwd "$USER_NAME")"
  existing_uid="$(cut -d: -f3 <<<"$passwd_line")"
  existing_home="$(cut -d: -f6 <<<"$passwd_line")"
  if [[ "$existing_uid" == "0" || "$existing_home" != "$HOME_DIR" ]]; then
    echo "Existing $USER_NAME account does not match the expected dedicated service account." >&2
    exit 1
  fi
  mapfile -t extra_groups < <(id -nG "$USER_NAME" | tr ' ' '\n' | grep -vx "$USER_NAME" || true)
  if (( ${#extra_groups[@]} > 0 )); then
    echo "Existing $USER_NAME account has unexpected supplementary groups: ${extra_groups[*]}" >&2
    echo "Refusing to reuse a more-privileged account." >&2
    exit 1
  fi
else
  useradd --create-home --home-dir "$HOME_DIR" --shell /bin/bash "$USER_NAME"
fi

install -d -o "$USER_NAME" -g "$USER_NAME" -m 0700 "$HOME_DIR"
install -d -o root -g "$USER_NAME" -m 0750 "$CONFIG_DIR"
install -d -o "$USER_NAME" -g "$USER_NAME" -m 0700 "$STATE_DIR"
install -o root -g root -m 0755 "$BINARY_SRC" "$BINARY_DST"

# Preserve all existing credentials on reinstall and add any newly introduced role keys.
touch "$ENV_FILE"
ensure_key() {
  local name="$1"
  if ! grep -q "^${name}=" "$ENV_FILE"; then
    printf '%s=%s\n' "$name" "$(openssl rand -hex 32)" >> "$ENV_FILE"
  fi
}
ensure_key TUXBRIDGE_API_KEY
ensure_key TUXBRIDGE_DEV_API_KEY
ensure_key TUXBRIDGE_REVIEW_API_KEY
ensure_key TUXBRIDGE_OPS_API_KEY
chmod 0640 "$ENV_FILE"
chown root:"$USER_NAME" "$ENV_FILE"

if [[ "$profile" == "default" ]]; then
  git_write=false
  git_network=false
else
  git_write=true
  git_network=true
fi

cat > "$CONFIG_DIR/tuxbridge.toml" <<EOF
[server]
listen = "127.0.0.1:8787"

[auth]
# Backwards-compatible administrator credential with unrestricted API access.
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

[security]
profile = "$profile"
default_command_allowlist = ["pwd", "ls", "cat", "head", "tail", "wc", "grep", "stat", "du", "df", "uname", "id", "whoami"]

[limits]
max_body_bytes = 10485760
max_in_flight = 32
command_timeout_seconds = 120
max_command_timeout_seconds = 900
command_output_bytes = 2097152
max_jobs = 128
job_retention_seconds = 3600

[workspaces.home]
root = "$HOME_DIR"

[workspaces.home.capabilities]
fs_read = true
fs_write = true
commands = true
git_read = true
git_write = $git_write
git_network = $git_network
EOF
chmod 0640 "$CONFIG_DIR/tuxbridge.toml"
chown root:"$USER_NAME" "$CONFIG_DIR/tuxbridge.toml"

cat > "$SERVICE_DST" <<'EOF'
[Unit]
Description=TuxBridge ChatGPT host bridge
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=tuxbridge
Group=tuxbridge
WorkingDirectory=/home/tuxbridge
Environment=HOME=/home/tuxbridge
Environment=USER=tuxbridge
Environment=LOGNAME=tuxbridge
Environment=TUXBRIDGE_CONFIG=/etc/tuxbridge/tuxbridge.toml
EnvironmentFile=/etc/tuxbridge/tuxbridge.env
ExecStart=/usr/local/bin/tuxbridge
Restart=on-failure
RestartSec=3
UMask=0077
EOF

if [[ "$profile" == "default" ]]; then
  cat >> "$SERVICE_DST" <<'EOF'
NoNewPrivileges=yes
PrivateTmp=yes
PrivateDevices=yes
ProtectSystem=strict
ProtectHome=read-only
ReadWritePaths=/home/tuxbridge /var/lib/tuxbridge
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectKernelLogs=yes
ProtectControlGroups=yes
ProtectClock=yes
ProtectHostname=yes
LockPersonality=yes
MemoryDenyWriteExecute=yes
RestrictSUIDSGID=yes
RestrictRealtime=yes
SystemCallArchitectures=native
EOF
fi

cat >> "$SERVICE_DST" <<'EOF'

[Install]
WantedBy=multi-user.target
EOF

if [[ "$profile" == "i_want_to_nuke_my_server" ]]; then
  command -v visudo >/dev/null 2>&1 || { echo "visudo is required for nuke profile" >&2; exit 1; }
  printf '%s ALL=(ALL:ALL) NOPASSWD: ALL\n' "$USER_NAME" > "$SUDOERS_DST"
  chmod 0440 "$SUDOERS_DST"
  visudo -cf "$SUDOERS_DST" >/dev/null
else
  rm -f "$SUDOERS_DST"
fi

if [[ -n "$SUBDOMAIN" ]]; then
  echo "Generating GPT Action schemas for $PUBLIC_ORIGIN ..."
  if [[ -n "${SUDO_USER:-}" && "$SUDO_USER" != "root" ]] && id "$SUDO_USER" >/dev/null 2>&1 && command -v runuser >/dev/null 2>&1; then
    runuser -u "$SUDO_USER" -- python3 "$PROFILE_GENERATOR" --server-url "$PUBLIC_ORIGIN"
  else
    python3 "$PROFILE_GENERATOR" --server-url "$PUBLIC_ORIGIN"
  fi
fi

systemctl daemon-reload
systemctl enable --now tuxbridge.service

echo "Installed TuxBridge with profile: $profile"
echo "Service user: $USER_NAME"
echo "Home workspace: $HOME_DIR"
echo "Config: $CONFIG_DIR/tuxbridge.toml"
echo "Credential file: $ENV_FILE"
echo "Retrieve all GPT credentials with: sudo cat $ENV_FILE"
echo "  TUXBRIDGE_DEV_API_KEY    -> openapi-dev.yaml"
echo "  TUXBRIDGE_REVIEW_API_KEY -> openapi-review.yaml"
echo "  TUXBRIDGE_OPS_API_KEY    -> openapi-ops.yaml"
echo "  TUXBRIDGE_API_KEY        -> administrator / Mission Control fallback"

if [[ -n "$SUBDOMAIN" ]]; then
  cat <<EOF

GPT Action schemas regenerated with:
  $PUBLIC_ORIGIN

Generated files:
  $SCRIPT_DIR/openapi-dev.yaml
  $SCRIPT_DIR/openapi-review.yaml
  $SCRIPT_DIR/openapi-ops.yaml

Caddy reverse-proxy block for $SUBDOMAIN
----------------------------------------
$SUBDOMAIN {
    reverse_proxy 127.0.0.1:8787
}
----------------------------------------

Add that block to your Caddyfile after pointing DNS for $SUBDOMAIN at this host.
Then validate and reload Caddy, for example:
  sudo caddy validate --config /etc/caddy/Caddyfile
  sudo systemctl reload caddy

Public TuxBridge base URL:
  $PUBLIC_ORIGIN

The three generated GPT Action schemas already use this HTTPS origin.
EOF
fi
