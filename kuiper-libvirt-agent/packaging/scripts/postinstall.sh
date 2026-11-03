#!/bin/sh
# Create the kuiper system user/group and the directories the agent uses, add
# the user to the libvirt group, then reload systemd. Runs on install and upgrade.
set -e

# Create the kuiper system group/user. Support both shadow-utils
# (Debian/RHEL: groupadd/useradd) and BusyBox (Alpine: addgroup/adduser).
home=/var/lib/kuiper-libvirt-agent
if ! getent group kuiper >/dev/null 2>&1; then
    if command -v groupadd >/dev/null 2>&1; then
        groupadd --system kuiper
    else
        addgroup -S kuiper
    fi
fi
if ! getent passwd kuiper >/dev/null 2>&1; then
    if command -v useradd >/dev/null 2>&1; then
        nologin="$(command -v nologin || echo /usr/sbin/nologin)"
        useradd --system --gid kuiper --no-create-home \
            --home-dir "$home" --shell "$nologin" kuiper
    else
        nologin="$(command -v nologin || echo /sbin/nologin)"
        adduser -S -D -H -G kuiper -h "$home" -s "$nologin" kuiper
    fi
fi

# the libvirt group comes with the daemon package, which is only a recommends, so
# it may not be there yet
libvirt_group=no
if getent group libvirt >/dev/null 2>&1; then
    libvirt_group=yes
    if ! id -nG kuiper | tr ' ' '\n' | grep -qx libvirt; then
        if command -v usermod >/dev/null 2>&1; then
            usermod -a -G libvirt kuiper
        else
            addgroup kuiper libvirt
        fi
    fi
fi

# Config lives in /etc and is written by the `register` subcommand. Create it
# owned by the service user so registration can run as that user. The state/log
# dirs are normally created by systemd (StateDirectory/LogsDirectory), but make
# them here too so `register` works before the first service start. Same modes
# as the unit, see the note there on why the state dir is 0711.
install -d -o kuiper -g kuiper -m 0750 /etc/kuiper-libvirt-agent
install -d -o kuiper -g kuiper -m 0711 /var/lib/kuiper-libvirt-agent
install -d -o kuiper -g kuiper -m 0750 /var/log/kuiper-libvirt-agent

if command -v systemctl >/dev/null 2>&1; then
    systemctl daemon-reload || true
fi

cat <<'EOF'
kuiper-libvirt-agent installed.

Next steps:
  1. Register with your coordinator. Run as the kuiper user so the config and
     certificates land in /etc and /var/lib with the correct owner:
       sudo -u kuiper kuiper-libvirt-agent --system register kfr1_BUNDLE_TOKEN
  2. Edit /etc/kuiper-libvirt-agent/config.toml (libvirt, VM and SSH settings).
     See /usr/share/doc/kuiper-libvirt-agent/config.example.toml for all options.
     The base images must be readable by the kuiper user (for qemu-img) and by
     libvirt's QEMU user (libvirt-qemu or qemu).
  3. Enable and start the service:
       sudo systemctl enable --now kuiper-libvirt-agent
EOF

if [ "$libvirt_group" = no ]; then
    cat <<'EOF'

Note: there is no libvirt group yet, so the kuiper user can't reach
qemu:///system. Install libvirt, then run:
  sudo usermod -a -G libvirt kuiper
EOF
fi
