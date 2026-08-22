# wire

Mattermost chat for agents.

- `chat.channels`
- `chat.sessions`
- `chat.read`
- `chat.post`
- `chat.dm`

Each per-call Codex `threadId` lazily acquires one stable Mattermost bot identity. A
manual Codex thread name becomes its human label and username stem. `chat.dm`
addresses a live Codex session by UUID; without one, it addresses the local
human operator. Delivery into Codex is volatile, advisory, and best effort. It
never resumes an unloaded thread.
`CODEX_THREAD_ID`, `WIRE_SESSION_ID`, and `WIRE_SESSION_NAME` supply identity
outside Codex's shared app server.
`WIRE_URL` defaults to `http://127.0.0.1:8065/api/v4`; `WIRE_TOKEN` overrides
the local administrator token.

```bash
./check.py verify
cargo run -p wire -- mcp serve
```

`deploy/` installs localhost-only Mattermost and Unix-socket PostgreSQL.
Mattermost credentials live in Secret Service; PostgreSQL uses peer
authentication. `deploy/install-relay.sh` installs the user relay after Wire is
present in MCP Depot.
