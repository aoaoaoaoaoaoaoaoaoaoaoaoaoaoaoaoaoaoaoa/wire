#!/usr/bin/env bash
set -euo pipefail

readonly release=${1:?usage: install-server.sh RELEASE_TREE}
readonly version=${2:?usage: install-server.sh RELEASE_TREE VERSION}
readonly here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
readonly pg_data=/var/lib/postgres/data
readonly legacy_pg=/var/lib/postgres/data-pg12-legacy-202008
readonly release_root=/opt/mattermost/releases
readonly installed="$release_root/$version"

[[ $EUID == 0 ]] || { printf 'root required\n' >&2; exit 1; }
[[ -x $release/bin/mattermost && -x $release/bin/mmctl ]] || {
    printf 'invalid Mattermost release tree: %s\n' "$release" >&2
    exit 1
}

systemctl stop mattermost.service postgresql.service 2>/dev/null || true

if [[ -e /etc/systemd/system/postgresql.service.d/10-change-pgroot.conf ]]; then
    grep -qx 'Group=data' /etc/systemd/system/postgresql.service.d/10-change-pgroot.conf || {
        printf 'refusing to remove an unknown PostgreSQL override\n' >&2
        exit 1
    }
    rm /etc/systemd/system/postgresql.service.d/10-change-pgroot.conf
    rmdir --ignore-fail-on-non-empty /etc/systemd/system/postgresql.service.d
fi

if [[ -f $pg_data/PG_VERSION && $(<"$pg_data/PG_VERSION") != 18 ]]; then
    [[ ! -e $legacy_pg ]] || {
        printf 'legacy PostgreSQL destination already exists: %s\n' "$legacy_pg" >&2
        exit 1
    }
    mv "$pg_data" "$legacy_pg"
fi

if [[ ! -f $pg_data/PG_VERSION ]]; then
    install -d -o postgres -g postgres -m 0700 "$pg_data"
    runuser -u postgres -- initdb \
        --pgdata="$pg_data" \
        --encoding=UTF8 \
        --locale=C.UTF-8 \
        --auth-local=peer \
        --auth-host=scram-sha-256
fi
[[ $(<"$pg_data/PG_VERSION") == 18 ]] || {
    printf 'PostgreSQL 18 data directory required\n' >&2
    exit 1
}

install -o postgres -g postgres -m 0600 "$here/pg_hba.conf" "$pg_data/pg_hba.conf"
install -o postgres -g postgres -m 0600 \
    "$here/postgresql.auto.conf" \
    "$pg_data/postgresql.auto.conf"
install -d -o root -g root -m 0755 "$release_root"

if [[ ! -d $installed ]]; then
    readonly incoming="$release_root/.${version}.incoming"
    [[ ! -e $incoming ]] || {
        printf 'stale Mattermost installation transaction: %s\n' "$incoming" >&2
        exit 1
    }
    cp -a -- "$release" "$incoming"
    rm -rf -- "$incoming/prepackaged_plugins" "$incoming/logs"
    chown -R root:root "$incoming"
    chmod -R a-w "$incoming"
    mv -- "$incoming" "$installed"
fi

ln -sfn "releases/$version" /opt/mattermost/.current
mv -Tf /opt/mattermost/.current /opt/mattermost/current

getent group mattermost >/dev/null || groupadd --system mattermost
id mattermost >/dev/null 2>&1 || useradd \
    --system \
    --gid mattermost \
    --home-dir /var/lib/mattermost \
    --shell /usr/bin/nologin \
    mattermost

install -d -o mattermost -g mattermost -m 0700 \
    /var/lib/mattermost/config \
    /var/lib/mattermost/files \
    /var/lib/mattermost/export \
    /var/lib/mattermost/import \
    /var/lib/mattermost/plugins \
    /var/lib/mattermost/client/plugins

if [[ ! -f /var/lib/mattermost/client/root.html ]]; then
    install -o mattermost -g mattermost -m 0600 \
        "$release/client/root.html" \
        /var/lib/mattermost/client/root.html
fi

config_source="$release/config/config.json"
[[ ! -f /var/lib/mattermost/config/config.json ]] || \
    config_source=/var/lib/mattermost/config/config.json
readonly config_source
readonly config_tmp=$(mktemp /var/lib/mattermost/config/.config.XXXXXX)
trap 'rm -f -- "$config_tmp"' EXIT
jq '
        .ServiceSettings.SiteURL = "http://127.0.0.1:8065"
        | .ServiceSettings.ListenAddress = "127.0.0.1:8065"
        | .ServiceSettings.EnableOAuthServiceProvider = false
        | .ServiceSettings.EnableIncomingWebhooks = false
        | .ServiceSettings.EnableOutgoingWebhooks = false
        | .ServiceSettings.EnableCommands = false
        | .ServiceSettings.EnableUserAccessTokens = true
        | .ServiceSettings.EnableBotAccountCreation = true
        | .ServiceSettings.EnableGifPicker = false
        | .ServiceSettings.EnableCustomEmoji = false
        | .ServiceSettings.EnableTutorial = false
        | .ServiceSettings.EnableOnboardingFlow = false
        | .ServiceSettings.EnableLocalMode = false
        | .ServiceSettings.LocalModeSocketLocation = "/var/tmp/mattermost_local.socket"
        | .SqlSettings.DriverName = "postgres"
        | .SqlSettings.DataSource = "postgres://mattermost@/mattermost?host=/run/postgresql&sslmode=disable&connect_timeout=10&binary_parameters=yes"
        | .SqlSettings.MaxIdleConns = 4
        | .SqlSettings.MaxOpenConns = 16
        | .FileSettings.DriverName = "local"
        | .FileSettings.Directory = "/var/lib/mattermost/files"
        | .FileSettings.ExportDirectory = "/var/lib/mattermost/files"
        | .PluginSettings.Enable = false
        | .PluginSettings.EnableMarketplace = false
        | .PluginSettings.EnableRemoteMarketplace = false
        | .PluginSettings.AutomaticPrepackagedPlugins = false
        | .PluginSettings.Directory = "/var/lib/mattermost/plugins"
        | .PluginSettings.ClientDirectory = "/var/lib/mattermost/client/plugins"
        | .LogSettings.EnableConsole = true
        | .LogSettings.ConsoleJson = false
        | .LogSettings.EnableFile = false
        | .LogSettings.EnableDiagnostics = false
        | .LogSettings.EnableSentry = false
        | .EmailSettings.SendEmailNotifications = false
        | .EmailSettings.SendPushNotifications = false
        | .MetricsSettings.Enable = false
        | .MetricsSettings.EnableClientMetrics = false
        | .MetricsSettings.EnableNotificationMetrics = false
        | .TeamSettings.SiteName = "Mattermost"
        | .TeamSettings.EnableUserCreation = false
        | .TeamSettings.EnableOpenServer = false
        | .TeamSettings.EnableJoinLeaveMessageByDefault = false
        | .TeamSettings.EnableCustomUserStatuses = false
        | .PrivacySettings.ShowEmailAddress = false
        | .ExportSettings.Directory = "/var/lib/mattermost/export"
        | .ImportSettings.Directory = "/var/lib/mattermost/import"
    ' "$config_source" >"$config_tmp"
chown mattermost:mattermost "$config_tmp"
chmod 0600 "$config_tmp"
mv "$config_tmp" /var/lib/mattermost/config/config.json
trap - EXIT

install -o root -g root -m 0644 "$here/mattermost.service" /etc/systemd/system/mattermost.service

systemctl daemon-reload
systemctl enable --now postgresql.service

runuser -u postgres -- psql --dbname=postgres --set=ON_ERROR_STOP=1 <<'SQL'
SELECT 'CREATE ROLE mattermost LOGIN'
WHERE NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'mattermost')\gexec
SELECT 'CREATE DATABASE mattermost OWNER mattermost'
WHERE NOT EXISTS (SELECT FROM pg_database WHERE datname = 'mattermost')\gexec
SQL

systemctl enable --now mattermost.service
