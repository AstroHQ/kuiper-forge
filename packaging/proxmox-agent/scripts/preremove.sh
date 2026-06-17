#!/bin/sh
# Stop and disable the service when the package is being removed (not upgraded).
set -e

# dpkg passes "remove"/"upgrade"; rpm passes 0 (remove) / 1 (upgrade).
case "$1" in
    remove | purge | 0)
        if command -v systemctl >/dev/null 2>&1; then
            systemctl disable --now kuiper-proxmox-agent.service || true
        fi
        ;;
esac
