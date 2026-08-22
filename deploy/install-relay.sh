#!/usr/bin/env bash
set -euo pipefail

readonly here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
readonly config_home=${XDG_CONFIG_HOME:-${HOME}/.config}
readonly unit_dir="$config_home/systemd/user"

install -d -m 0700 "$HOME/.local/libexec" "$unit_dir"
install -m 0700 "$here/wire-relay" "$HOME/.local/libexec/wire-relay"
for unit in wire-relay.service wire-relay-refresh.path wire-relay-refresh.service; do
    install -m 0600 "$here/$unit" "$unit_dir/$unit"
done

systemctl --user daemon-reload
systemctl --user enable --now wire-relay.service wire-relay-refresh.path
