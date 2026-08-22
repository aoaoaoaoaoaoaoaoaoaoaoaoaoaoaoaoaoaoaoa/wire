#!/usr/bin/env bash
set -euo pipefail

readonly mmctl=/opt/mattermost/current/bin/mmctl
readonly config=/var/lib/mattermost/config/config.json
readonly runtime_uid=$(id -u main)
readonly runtime_dir="/run/user/$runtime_uid"
readonly bus="unix:path=$runtime_dir/bus"

[[ $EUID == 0 ]] || { printf 'root required\n' >&2; exit 1; }

readonly enable_tmp=$(mktemp /var/lib/mattermost/config/.config.XXXXXX)
trap 'rm -f -- "$enable_tmp"' EXIT
jq '
    .ServiceSettings.EnableLocalMode = true
    | .ServiceSettings.LocalModeSocketLocation = "/var/tmp/mattermost_local.socket"
' "$config" >"$enable_tmp"
chown mattermost:mattermost "$enable_tmp"
chmod 0600 "$enable_tmp"
mv "$enable_tmp" "$config"
trap - EXIT
systemctl restart mattermost.service

readonly server_pid=$(systemctl show mattermost.service --property=MainPID --value)
readonly -a local_mmctl=(
    nsenter --target "$server_pid" --mount --
    "$mmctl" --local --json --suppress-warnings
)
nsenter --target "$server_pid" --mount -- \
    test -S /var/tmp/mattermost_local.socket || {
    printf 'Mattermost local administration socket is unavailable\n' >&2
    exit 1
}

secret_lookup() {
    runuser -u main -- env \
        XDG_RUNTIME_DIR="$runtime_dir" \
        DBUS_SESSION_BUS_ADDRESS="$bus" \
        secret-tool lookup "$@"
}

secret_store() {
    local label=$1 value=$2
    shift 2
    printf %s "$value" | runuser -u main -- env \
        XDG_RUNTIME_DIR="$runtime_dir" \
        DBUS_SESSION_BUS_ADDRESS="$bus" \
        secret-tool store --label="$label" "$@"
}

if ! "${local_mmctl[@]}" user search main | \
    jq -e '.. | objects | select(.username? == "main")' >/dev/null; then
    admin_password=$(openssl rand -base64 36)
    "${local_mmctl[@]}" user create \
        --username main \
        --email main@localhost.invalid \
        --password "$admin_password" \
        --system-admin \
        --email-verified \
        --disable-welcome-email >/dev/null
    secret_store \
        'Mattermost administrator' \
        "$admin_password" \
        application mattermost credential admin-password username main
elif ! secret_lookup \
    application mattermost credential admin-password username main >/dev/null; then
    admin_password=$(openssl rand -base64 36)
    "${local_mmctl[@]}" user change-password main \
        --password "$admin_password" >/dev/null
    secret_store \
        'Mattermost administrator' \
        "$admin_password" \
        application mattermost credential admin-password username main
fi

if ! "${local_mmctl[@]}" team search eternalist | \
    jq -e '.. | objects | select(.name? == "eternalist")' >/dev/null; then
    "${local_mmctl[@]}" team create \
        --name eternalist \
        --display-name Eternalist \
        --private >/dev/null
fi

if ! "${local_mmctl[@]}" channel search --team eternalist agents | \
    jq -e '.. | objects | select(.name? == "agents")' >/dev/null; then
    "${local_mmctl[@]}" channel create \
        --team eternalist \
        --name agents \
        --display-name Agents \
        --purpose 'Agent coordination' >/dev/null
fi

admin_password=$(secret_lookup \
    application mattermost credential admin-password username main)
readonly admin_password
readonly cell=$(mktemp --directory /run/mattermost/bootstrap.XXXXXX)
cleanup() {
    if [[ -n ${session_token:-} ]]; then
        curl --silent --output /dev/null \
            --request POST \
            --header "Authorization: Bearer $session_token" \
            http://127.0.0.1:8065/api/v4/users/logout || true
    fi
    rm -rf -- "$cell"
}
trap cleanup EXIT

jq -cn \
    --arg login_id main \
    --arg password "$admin_password" \
    '{login_id: $login_id, password: $password}' | \
    curl --fail --silent --show-error \
        --dump-header "$cell/login.headers" \
        --output /dev/null \
        --request POST \
        --header 'Content-Type: application/json' \
        --data-binary @- \
        http://127.0.0.1:8065/api/v4/users/login
session_token=$(awk \
    'tolower($1) == "token:" {gsub("\\r", "", $2); print $2}' \
    "$cell/login.headers")
[[ -n $session_token ]] || {
    printf 'Mattermost login returned no session token\n' >&2
    exit 1
}

api_get() {
    curl --fail --silent --show-error \
        --header "Authorization: Bearer $session_token" \
        "http://127.0.0.1:8065/api/v4$1"
}

api_post() {
    curl --fail --silent --show-error \
        --request POST \
        --header "Authorization: Bearer $session_token" \
        --header 'Content-Type: application/json' \
        --data-binary @- \
        "http://127.0.0.1:8065/api/v4$1"
}

readonly main_id=$(api_get /users/username/main | jq -er .id)
readonly team_id=$(api_get /teams/name/eternalist | jq -er .id)
readonly channel_id=$(api_get "/teams/$team_id/channels/name/agents" | jq -er .id)

if ! api_get "/teams/$team_id/members/$main_id" >/dev/null 2>&1; then
    jq -cn --arg team_id "$team_id" --arg user_id "$main_id" \
        '{team_id: $team_id, user_id: $user_id}' | \
        api_post "/teams/$team_id/members" >/dev/null
fi
if ! api_get "/channels/$channel_id/members/$main_id" >/dev/null 2>&1; then
    jq -cn --arg channel_id "$channel_id" --arg user_id "$main_id" \
        '{channel_id: $channel_id, user_id: $user_id}' | \
        api_post "/channels/$channel_id/members" >/dev/null
fi

if ! secret_lookup \
    application wire service mattermost account admin >/dev/null; then
    token_json=$(jq -cn '{description: "wire administrator"}' | \
        api_post "/users/$main_id/tokens")
    readonly token_json
    token=$(jq -er 'first(.. | objects | .token? // empty)' <<<"$token_json")
    readonly token
    [[ -n $token ]] || {
        printf 'Mattermost returned no administrator token\n' >&2
        exit 1
    }
    secret_store \
        'Wire Mattermost administrator token' \
        "$token" \
        application wire service mattermost account admin
fi

curl --fail --silent --show-error --output /dev/null \
    --request POST \
    --header "Authorization: Bearer $session_token" \
    http://127.0.0.1:8065/api/v4/users/logout
session_token=
rm -rf -- "$cell"
trap - EXIT

readonly disable_tmp=$(mktemp /var/lib/mattermost/config/.config.XXXXXX)
trap 'rm -f -- "$disable_tmp"' EXIT
jq '.ServiceSettings.EnableLocalMode = false' "$config" >"$disable_tmp"
chown mattermost:mattermost "$disable_tmp"
chmod 0600 "$disable_tmp"
mv "$disable_tmp" "$config"
trap - EXIT

systemctl restart mattermost.service
systemctl is-active --quiet mattermost.service
