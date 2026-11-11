# Hub regressions

Moved from cstack on 2026-11-12.

## hub

### R-hub-1: stale binary on PATH made `hub` look dead
- **symptom / scenario:** Plain `hub` exited 0 with no output; the real CLI at
  `~/code/proj/polymux-hub/bin/hub` worked. Cause: `target/release/hub` was built when
  the repo lived at `~/code/hub`, and the rebuild guard in `bin/hub` compares
  mtimes, so moving the directory never triggered a rebuild.
- **guard:** `bin/hub` must rebuild when the resolved repo path changes, not just
  when source mtimes change. `~/.local/bin/hub` is a wrapper that execs the repo
  script (a bare symlink breaks `$0` repo-root resolution).
- **verify:** `hub unread >/dev/null && echo ok` produces output and exit 0; `hub status` reports bridges.
- **status:** verified (2026-10-06)

### R-hub-4: `CARGO_MANIFEST_DIR` baked a stale repo path
- **symptom / scenario:** `hub status|check|update|install` shelled out to a
  non-existent `bin/hub-maintenance` after the repo moved, because the path came
  from `env!("CARGO_MANIFEST_DIR")`, which is frozen at compile time. Cargo sees
  unchanged sources as fresh and does not recompile after a move.
- **guard:** Resolve the repo root at runtime: `HUB_REPO_ROOT` from `bin/hub`,
  else the running executable's location, else the compile-time path. The same
  change makes `skills install` relink its own dangling links (via
  `is_prior_pack_link`) instead of refusing to touch them.
- **verify:** `cargo test` (3 pass, incl. the prior-link test); `hub skills install` reports `relinked`.
- **status:** verified (2026-10-06)

### R-hub-2: bridges are "not built by the hub"
- **symptom / scenario:** `hub check` prints `check failed (not built by the hub)`
  for every platform, and `iMessage: could not read the releases`.
- **guard:** This is the expected state for bridges installed outside the hub's
  own build. `hub update` is a reported no-op for them, not an error. Do not
  rebuild or reinstall bridges without asking.
- **verify:** `hub check` names the reason per platform; no crash.
- **status:** open

### R-hub-3: bridge realtime stream loses messages and does not backfill
- **symptom / scenario:** LinkedIn (and other) bridges drop their realtime stream
  on intermittent DNS failures (`lookup ...: no such host`) during sleep/wake,
  stay connected, and never backfill the gap. Messages only reappear after a
  bridge restart triggers a chat resync.
- **guard:** `hub update` should refresh bridge software; if a bridge is behind,
  restart+resync is the recovery. Surface the DNS/network cause rather than
  blaming the platform.
- **verify:** bridge runtime logs show no fresh `no such host` burst after update; `hub unread` sees recent events.
- **status:** open

---

