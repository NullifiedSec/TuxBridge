#!/usr/bin/env bash
set -euo pipefail

if [[ ${EUID:-$(id -u)} -ne 0 ]]; then
  echo "TuxBridge installer must run as root." >&2
  exit 1
fi

USER_NAME="tuxbridge"
HOME_DIR="/home/${USER_NAME}"
CONFIG_DIR="/etc/tuxbridge"
STATE_DIR="/var/lib/tuxbridge"
BINARY_SRC="${1:-./target/release/tuxbridge}"
BINARY_DST="/usr/local/bin/tuxbridge"
SERVICE_DST="/etc/systemd/system/tuxbridge.service"
SUDOERS_DST="/etc/sudoers.d/tuxbridge"

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
     Whoever controls the API key is effectively root. Yes, really.

EOF

read -r -p "Choose profile [1-3]: " choice
case "$choice" in
  1) profile="default" ;;
  2) profile="loose" ;;
  3)
    profile="i_want_to_nuke_my_server"
    echo
    echo "WARNING: this grants tuxbridge passwordless sudo for ALL commands."
    echo "Treat the TuxBridge API key as a root credential."
    read -r -p "Type NUKE to continue: " confirm
    [[ "$confirm" == "NUKE" ]] || { echo "Cancelled."; exit 1; }
    ;;
  *) echo "Invalid profile." >&2; exit 1 ;;
esac

if id "$USER_NAME" >/dev/null 2>&1; then
  echo "Refusing to reuse existing user $USER_NAME automatically." >&2
  echo "Remove/rename that account or install on a host without a pre-existing tuxbridge user." >&2
  exit 1
fi
useradd --create-home --home-dir "$HOME_DIR" --shell /bin/bash "$USER_NAME"

install -d -o "$USER_NAME" -g "$USER_NAME" -m 0700 "$HOME_DIR"
install -d -o root -g "$USER_NAME" -m 0750 "$CONFIG_DIR"
install -d -o "$USER_NAME" -g "$USER_NAME" -m 0700 "$STATE_DIR"
install -o root -g root -m 0755 "$BINARY_SRC" "$BINARY_DST"

api_key="$(openssl rand -hex 32)"
printf 'TUXBRIDGE_API_KEY=%s\n' "$api_key" > "$CONFIG_DIR/tuxbridge.env"
chmod 0640 "$CONFIG_DIR/tuxbridge.env"
chown root:"$USER_NAME" "$CONFIG_DIR/tuxbridge.env"

cat > "$CONFIG_DIR/tuxbridge.toml" <<EOF
[server]
listen = "127.0.0.1:8787"

[auth]
api_key_env = "TUXBRIDGE_API_KEY"

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
git_write = true
git_network = true
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

systemctl daemon-reload
systemctl enable --now tuxbridge.service

echo "Installed TuxBridge with profile: $profile"
echo "Service user: $USER_NAME"
echo "Home workspace: $HOME_DIR"
echo "Config: $CONFIG_DIR/tuxbridge.toml"
echo "API key file: $CONFIG_DIR/tuxbridge.env"
echo "Retrieve the API key with: sudo cat $CONFIG_DIR/tuxbridge.env"
