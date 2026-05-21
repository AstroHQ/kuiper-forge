#!/bin/sh
# Create the kuiper system user/group and the directories the agent uses, then
# reload systemd. Runs on install and upgrade.
set -e

if ! getent group kuiper >/dev/null 2>&1; then
    groupadd --system kuiper
fi
if ! getent passwd kuiper >/dev/null 2>&1; then
    nologin="$(command -v nologin || echo /usr/sbin/nologin)"
    useradd --system --gid kuiper --no-create-home \
        --home-dir /var/lib/kuiper-proxmox-agent \
        --shell "$nologin" kuiper
fi

# Config lives in /etc and is written by the `register` subcommand. Create it
# owned by the service user so registration can run as that user. The state/log
# dirs are normally created by systemd (StateDirectory/LogsDirectory), but make
# them here too so `register` works before the first service start.
install -d -o kuiper -g kuiper -m 0750 /etc/kuiper-proxmox-agent
install -d -o kuiper -g kuiper -m 0750 /var/lib/kuiper-proxmox-agent
install -d -o kuiper -g kuiper -m 0750 /var/log/kuiper-proxmox-agent

if command -v systemctl >/dev/null 2>&1; then
    systemctl daemon-reload || true
fi

cat <<'EOF'
kuiper-proxmox-agent installed.

Next steps:
  1. Register with your coordinator. Run as the kuiper user so the config and
     certificates land in /etc and /var/lib with the correct owner:
       sudo -u kuiper kuiper-proxmox-agent --system register kfr1_BUNDLE_TOKEN
  2. Edit /etc/kuiper-proxmox-agent/config.toml (Proxmox, VM and SSH settings).
     See /usr/share/doc/kuiper-proxmox-agent/config.example.toml for all options.
  3. Enable and start the service:
       sudo systemctl enable --now kuiper-proxmox-agent
EOF
