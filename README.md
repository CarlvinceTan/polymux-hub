# hub

The messaging hub as a stateless CLI plus a skill. Each command talks to the
private Matrix homeserver on localhost and exits; an MCP adapter is kept for
clients that have no shell.

```
bin/hub                the Rust CLI: builds on first use, then execs target/release/hub
bin/hub-maintenance    bridge maintenance backend (status, check, update, install)
bin/hub-mcp            optional MCP entry point (kept, not wired into OpenCode)
bin/hub-log-cap        trims a bridge runtime.log that has grown past the cap
Cargo.toml             Rust CLI crate
rust/main.rs           the CLI: commands, JSON output, skill install, maintenance forwarding
rust/hub.rs            bridge catalogue, status rows, provisioning
rust/matrix.rs         Matrix client: Keychain token and HTTP
rust/ops.rs            the six messaging operations as structured data
src/mcp.ts             MCP stdio transport, ~150 lines, no dependencies
src/tools.ts           the six messaging tools, adapted to the MCP text surface
src/ops.ts             the six messaging operations, shared by the MCP adapter
src/hub.ts             bridges, provisioning, Matrix client helpers
src/cookies.ts         Chromium CDP cookie harvest, used by the bridge login flows
src/install.ts         native bridge build, launchd unit, Tuwunel restart
src/skills.ts          agent skill install: one symlink per detected agent
skills/hub/SKILL.md    when and how the agent should use the CLI
```

## Run

```sh
./bin/hub connections          # platform and account status
./bin/hub unread               # unread conversations, without marking them read
./bin/hub rooms [query]        # joined conversations
./bin/hub messages <room_id>   # recent window, newest last
./bin/hub status               # bridge install/health
./bin/hub skills status        # where the skill is linked, read-only
```

Every command is one short-lived process over the Matrix homeserver on
localhost, so no CLI server has to stay running; only Tuwunel and the bridges
do. Add `--json` to a read command for structured output. `hub status`,
`hub check`, `hub update` and `hub install` are forwarded to
`bin/hub-maintenance`, which is still the TypeScript bridge tooling.

OpenCode does not need an MCP entry: the `hub` skill teaches the CLI and the
agent runs it through the shell. `bin/hub-mcp` is still there for a client with
no shell, but it is not wired into the OpenCode configuration.

## Log rotation

Each bridge's launchd unit sends its stdout and stderr straight to
`runtime.log`, and launchd never rotates that file, so a bridge stuck in a
reconnect loop writes until the disk is full: the Instagram bridge reached
16 GB. Installing a bridge also installs a `com.carlvince.matrix-hub.logcap`
launchd timer, which runs `bin/hub-log-cap` every minute and keeps only a tail
of any bridge log over the cap. launchd opens the redirection with `O_APPEND`,
so trimming the file in place is safe: the bridge's descriptor stays valid and
its next write lands at the new end of file. The cap is 64 MiB with an 8 MiB
tail, overridable per run with `HUB_LOG_MAX_BYTES` and `HUB_LOG_KEEP_BYTES`.

Only `bridges/*/runtime.log` is covered. Tuwunel's own log and the WeChat
bridge's logs are managed separately.

## Skill install

The skill is linked into each agent rather than declared in config, the same
shape as `cua-driver skills`, so the pack stays with the repo it belongs to:

```sh
./bin/hub skills install      # symlink skills/hub into each detected agent
./bin/hub skills status       # per-agent link state, read-only
./bin/hub skills uninstall    # remove the links this tool made
```

`install` covers every agent present on the machine (Claude Code, Codex, Prime
Agent, OpenClaw, OpenCode, Antigravity, Hermes) and skips the ones that are not
installed. It never replaces a link it did not make: a link pointing elsewhere,
or a real file in the way, is reported and left alone. `skills update` is a
synonym for `install`, since the pack is this repo and there is nothing to fetch.

The CLI is the agent-facing surface now; `hub_unread`, `hub_send` and so on
remain available through the optional MCP adapter.

## State

- Homeserver and bridges live under `~/Library/Application Support/matrix-hub`.
- The Matrix access token is in the Keychain, service `Matrix Hub Codex Token`.
- Email is handled separately by the Himalaya CLI skill, outside hub.

## Not ported from the plugin

The `/hub` menu, the session panel, the terminal QR renderer, and the
interactive bridge connect/disconnect flows all lived in the OpenCode TUI. They
are not here. `src/hub.ts` still contains them (`startLogin`, `disconnect`,
`cookiesLogin`) as dead code that expects a plugin context, so porting them to a
CLI later is a wiring job rather than a rewrite. Connecting a bridge today is the
bridge's own login flow; `bin/hub` forwards install, status and update to
`bin/hub-maintenance`.
