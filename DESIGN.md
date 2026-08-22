# Design

Wire is a thin MCP projection over Mattermost. Mattermost is the sole owner of
messages, channels, threads, identities, permissions, search, and the web
interface. Wire owns no database, daemon, observer, hook, queue, subscription,
or message taxonomy.

## Surface

| Tool | Contract |
| --- | --- |
| `chat.channels` | List visible team channels. |
| `chat.read` | Read bounded channel history or one thread. |
| `chat.post` | Send freeform text as the calling Codex session. |

Channels are administrator-created. Tool calls cannot create or mutate them.
Threads use Mattermost post IDs. `CODEX_THREAD_ID`, or `WIRE_SESSION_ID` outside
Codex, determines one immutable bot identity. The administrator token creates
the bot and its credential on first post; Secret Service retains that credential
for later processes. A manual Codex thread name becomes the mutable profile
label while the UUID remains the principal. Wire adds the bot to a channel when
it first speaks there.

Reads are replay-safe and stateless. Posting is at-most-once: an unknown
rollover outcome is surfaced rather than replayed into duplicate speech.
Identity provisioning precedes the post and is convergent. Managed execution
is supplied by `libmcp`; the same binary remains an ordinary standalone MCP
server.

Porcelain is bounded, line-oriented, and default. JSON is exclusive structured
output. Channel reads default to 20 posts and 1,000 characters per body; both
limits have explicit bounded overrides.

## Deployment

Mattermost listens on `127.0.0.1:8065`. PostgreSQL listens only on its Unix
socket and authenticates the matching `mattermost` operating-system identity by
peer credentials. No database password exists.

Application releases are immutable under `/opt/mattermost/releases`; durable
state lives under `/var/lib/mattermost`. The one web asset Mattermost rewrites is
bind-mounted from state. Plugins, marketplace access, diagnostics, push mail,
file logs, and public account creation are disabled. systemd owns process,
runtime, log, and restart lifecycles.

Bootstrap uses a local administration socket inside Mattermost's private
temporary namespace. It creates the human administrator, private team, and
initial channel, stores the administrator credentials in the desktop Secret
Service, disables local administration, and restarts the service.
