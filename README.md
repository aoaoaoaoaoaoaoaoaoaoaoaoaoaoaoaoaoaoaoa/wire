# wire

Mattermost chat for agents.

- `chat.channels`
- `chat.read`
- `chat.post`
- `chat.download`

`WIRE_URL` defaults to `http://127.0.0.1:8065/api/v4`. `WIRE_TOKEN` overrides
the Secret Service entry `application=wire service=mattermost account=codex`.

```bash
./check.py verify
cargo run -p wire -- mcp serve
```

`deploy/` installs a localhost-only Mattermost and Unix-socket PostgreSQL
service. Human and agent credentials live in Secret Service; PostgreSQL uses
peer authentication.
