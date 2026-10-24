#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

if ! systemctl is-active --quiet fcp.service; then
    printf 'Warning: fcp.service is not active. fcptemp requires the fcp API socket at /run/fcp/control.sock.\n' >&2
fi

cargo build --release -p fcptemp
sudo install -Dm755 target/release/fcptemp /usr/local/bin/fcptemp
if ! sudo test -f /etc/fcptemp/config.json; then
    sudo install -Dm644 crates/fcptemp/examples/fcptemp.json /etc/fcptemp/config.json
fi
sudo install -Dm644 deploy/systemd/fcptemp.service /etc/systemd/system/fcptemp.service
sudo systemctl daemon-reload
sudo systemctl enable --now fcptemp.service
sudo systemctl --no-pager --full status fcptemp.service

printf '\nfcptemp is installed. Automation remains disabled by default.\n'
printf 'Restart fcp-gui if it was already running, so it selects /run/fcptemp/control.sock.\n'
