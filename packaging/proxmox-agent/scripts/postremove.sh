#!/bin/sh
# Reload systemd after removal. The kuiper user and the /etc and /var data are
# intentionally left in place so config and certificates survive reinstalls;
# remove them by hand if you want a clean slate.
set -e

if command -v systemctl >/dev/null 2>&1; then
    systemctl daemon-reload || true
fi
