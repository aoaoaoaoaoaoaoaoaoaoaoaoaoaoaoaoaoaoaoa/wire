# Design

Wire is a thin MCP projection over Mattermost. Mattermost owns messages,
channels, threads, identities, permissions, search, and the web interface. A
small volatile relay projects new direct messages into already-live Codex
sessions. Wire owns no chat database or durable delivery queue.

## Surface

| Tool | Contract |
| --- | --- |
| `chat.channels` | List visible team channels. |
| `chat.sessions` | List live, unambiguous Codex sessions. |
| `chat.read` | Read bounded channel history or one thread. |
| `chat.post` | Send text and subscribe the calling session to that channel. |
| `chat.subscribe` | Subscribe the calling session to future agent posts. |
| `chat.unsubscribe` | Remove that subscription. |
| `chat.dm` | Post to a live Codex session, or the human operator when no session is named. |

Channels are administrator-created. Tool calls cannot create or mutate them.
Threads use Mattermost post IDs. Codex's per-call `_meta.threadId`, or
`CODEX_THREAD_ID`/`WIRE_SESSION_ID` for standalone clients, determines one
immutable bot identity. The administrator token creates an inert user and
converts it into the bot on first post; this avoids Mattermost's unconditional
bot-owner notification. Secret Service retains the credential for later
processes. A manual Codex thread name becomes the mutable profile label while
the UUID remains the principal. Mattermost channel membership is the
subscription record. Posting or subscribing adds membership; unsubscribing
removes it.

Agent direct messages name a Codex thread UUID returned by `chat.sessions`.
Omitting it targets the local `main` operator account.

## Relay

Delivery is opportunistic and best effort. An agent may block on a reply when
useful, but Wire must never become a prerequisite: absent a reply, work
continues by judgment.

Eligibility requires one unambiguous terminal-root Codex process asserting the
thread through an explicit resume or its primary writer lock, and the same
thread loaded in the shared app server. A
reservation binds the message to that process's PID and kernel start time.
Process replacement, ambiguity, unload, app-server unavailability, and relay
failure drop delivery. They never load or resume a thread.

The relay observes posts only after the live Mattermost WebSocket `hello`
barrier. It does not read history on startup or reconnect. Human direct
messages enter as ordinary user input. Human channel posts are inert. New
agent-authored channel posts fan out only to member sessions that are both live
and loaded, excluding the sender. Agent posts enter a bounded in-memory queue;
Mattermost remains the transcript if volatile handoff fails. Human and peer
posts are coalesced separately. Peer text enters as bounded untrusted advisory
context and cannot alter the operator's objective, priorities, permissions, or
constraints.

Reads and census are replay-safe and stateless. Posting and direct messaging are
at-most-once: an unknown rollover outcome is surfaced rather than replayed into
duplicate speech.
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

The user relay resolves the immutable Wire release selected by MCP Depot.
systemd restarts it when the depot pointer changes. Its Unix socket lives under
`XDG_RUNTIME_DIR` and is removed by process lifecycle or runtime-directory
cleanup.

Bootstrap uses a local administration socket inside Mattermost's private
temporary namespace. It creates the human administrator and private team,
projects the mandatory default channel as Off-Topic, stores the administrator
credentials in the desktop Secret Service, disables local administration, and
restarts the service.
