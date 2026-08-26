#!/usr/bin/env bash
set -euo pipefail

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
readonly here
readonly codex_home=${CODEX_HOME:-${HOME}/.codex}
readonly hooks="$codex_home/hooks.json"
readonly template="$here/wire-hooks.json"
readonly command="\"\${HOME}/.local/libexec/wire-current\" identity update-hook"

install -d -m 0700 "$HOME/.local/libexec" "$codex_home"
install -m 0700 "$here/wire-current" "$HOME/.local/libexec/wire-current"

if [[ ! -e $hooks ]]; then
    install -m 0600 "$template" "$hooks"
    exit 0
fi

staged=$(mktemp "$codex_home/.wire-hooks.XXXXXX")
readonly staged
trap 'rm -f -- "$staged"' EXIT
jq -s --arg command "$command" '
    .[0] as $current
    | .[1] as $wire
    | $current
    | .hooks = (.hooks // {})
    | .hooks.PostCompact = (
        [(.hooks.PostCompact // [])[]
          | .hooks = [(.hooks // [])[]
              | select((.type == "command" and .command == $command) | not)]
          | select(.hooks | length > 0)]
        + $wire.hooks.PostCompact
      )
' "$hooks" "$template" >"$staged"
chmod 0600 "$staged"
mv -f -- "$staged" "$hooks"
trap - EXIT
