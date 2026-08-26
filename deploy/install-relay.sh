#!/usr/bin/env bash
set -euo pipefail

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
readonly here
readonly config_home=${XDG_CONFIG_HOME:-${HOME}/.config}
readonly unit_dir="$config_home/systemd/user"

install -d -m 0700 "$HOME/.local/libexec" "$unit_dir"
install -m 0700 "$here/wire-current" "$HOME/.local/libexec/wire-current"
for unit in wire-relay.service wire-relay-refresh.path wire-relay-refresh.service; do
    install -m 0600 "$here/$unit" "$unit_dir/$unit"
done

systemctl --user daemon-reload
systemctl --user enable wire-relay.service wire-relay-refresh.path
systemctl --user restart wire-relay.service wire-relay-refresh.path
