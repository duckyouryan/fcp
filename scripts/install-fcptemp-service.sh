#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

if ! sudo test -x /usr/local/bin/fcp; then
    printf 'Error: /usr/local/bin/fcp is not installed. Install fcp before fcptemp.\n' >&2
    exit 1
fi

cargo build --release -p fcptemp
sudo install -Dm755 target/release/fcptemp /usr/local/bin/fcptemp
if ! sudo test -f /etc/fcptemp/config.json; then
    sudo install -Dm644 crates/fcptemp/examples/fcptemp.json /etc/fcptemp/config.json
fi

# Install the matching units together so startup ordering and socket readiness agree.
sudo install -Dm644 deploy/systemd/fcp.service /etc/systemd/system/fcp.service
sudo install -Dm644 deploy/systemd/fcptemp.service /etc/systemd/system/fcptemp.service
sudo systemctl daemon-reload
sudo systemctl enable fcp.service fcptemp.service

# Restart FCP so systemd applies ExecStartPost socket-readiness handling.
sudo systemctl restart fcp.service
sudo systemctl restart fcptemp.service

sudo systemctl --no-pager --full status fcp.service fcptemp.service
printf '\nfcptemp installed. It starts after FCP has bound /run/fcp/control.sock.\n'
printf 'Temperature automation remains controlled by the saved fcptemp state.\n'
printf 'Restart fcp-gui if it was already running and uses an old socket path.\n'
