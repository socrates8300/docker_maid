# docker_maid — Product Requirements Document

| | |
|---|---|
| **Status** | Draft v0.3 — review fixes: deletion contract, protection persistence, agent-mode flags |
| **Owner** | James Ray |
| **Repo** | `docker_maid` |
| **License** | MIT |

---

## 1. Problem

Coding agents (Claude Code, Codex, Aider, custom scripts) increasingly use Docker as a sandbox: they spin up ephemeral containers, build throwaway images, mount volumes, and create networks. These resources are frequently abandoned when an agent finishes, crashes, or is interrupted mid-task. The result is **Docker sprawl**:

- Hundreds of stopped containers named `agent-sandbox-a1b2c3…`
- Multi-GB layers of dangling/intermediate images from repeated builds
- Orphaned anonymous volumes holding stale workspace copies
- Leaked bridge networks exhausting the default address pool (`no available IPv4 addresses on network pool` — the failure that finally gets noticed)
- Host disk filling up silently until Docker (or the whole machine) falls over

Existing tools (`docker system prune`) are all-or-nothing, blunt, age-blind to *ownership*, and dangerous to run when real workloads share the same Docker daemon. Nothing on the market is designed around the agent-workflow lifecycle.

## 2. Vision

> **docker_maid is a small, fast Rust daemon/CLI/TUI that reads a declarative policy file and continuously reclaims Docker resources left behind by coding agents — without ever touching anything outside an explicit ownership rule or an explicitly authorized unscoped policy.**

One sentence: *Declare what agent sprawl looks like; docker_maid keeps it at zero.*

One product, **three faces**: `tui` for humans at a terminal, plain tables for humans in scripts, and a stable versioned JSON interface for coding agents. Every capability is reachable through all three — no feature ever lives only behind the interactive UI.

## 3. Goals & Non-Goals

### Goals

1. **Declarative policy, explicit runtime state** — cleanup policy lives in one versionable TOML config file; machine-managed protection and activity state lives separately under `$XDG_STATE_HOME` and is always auditable.
2. **Safe by default** — non-interactive commands are dry-run unless `--apply`; TUI mutation requires confirmation of an immutable plan. Ownership is explicit, and unowned resources are never deleted unless a rule explicitly opts into the authorized-unscoped escape hatch.
3. **Two modes of operation** — one-shot `clean` (cron/CI-friendly) and a long-running `daemon` (watch + reap on interval).
4. **Full resource coverage** — containers, images, volumes, networks, and build cache.
5. **Observable** — every action logged and reportable; users always know what the maid did (or *would* do).
6. **Zero runtime deps** — single static Rust binary talking to the Docker API over the socket; no Python, no sidecar containers.
7. **Effortless interactive UX** — a ratatui-based TUI (`docker_maid tui`): at-a-glance dashboard, browsable inventory, one-confirmation plan application. (The TUI applies policy-generated plans only — no side-door deletions.)
8. **Agent-first headless parity** — every TUI capability has a non-interactive equivalent with stable, documented JSON output; the TUI never launches when not attached to a TTY. (The agents causing the sprawl are also our users.)

### Non-Goals (v1)

- ❌ Managing Kubernetes / Podman / containerd directly (Podman socket compat may fall out for free; not a target).
- ❌ Inside-container process management (no `exec` babysitting).
- ❌ Being a Docker Compose replacement or supervising agent lifecycles (reap, don't parent).
- ❌ Cloud/multi-host fleet management. One daemon per host.
- ❌ Garbage-collecting *files* on the host (only Docker-managed objects).
- ❌ Web UI / full GUI (Electron dashboards, etc.). The ratatui TUI is the only interactive surface in v1; everything else is headless.

## 4. Users & Personas

| Persona | Description | Need |
|---|---|---|
| **Agent-power-user (primary)** | Developer running one or more coding agents daily on a workstation or homelab box | Host doesn't die; disk doesn't fill; nothing important is deleted |
| **CI/CD pipeline author** | Runs agents inside ephemeral runners with a shared or self-hosted Docker daemon | Deterministic cleanup step with trustworthy exit codes |
| **Homelab / shared-server admin** | Hosts real services *and* experiments on the same daemon | Guarantee that production containers are invisible to the maid |
| **The coding agent itself** | The very bot that spawns sandboxes — often the one running cleanup | Deterministic, parseable output; no TTY assumptions; never blocks on a prompt |

## 5. User Stories

1. *As an agent user*, I run `docker_maid clean --apply` and every container my abandoned agent sessions left is removed, so `docker ps -a` is readable again.
2. *As a developer*, I configure a TTL of 2h for anything labeled `devcontainer`, leave my workspace running over lunch, and it's still there when I return — but the crashed one from yesterday is gone.
3. *As an admin*, I add my production containers to a protected list and I am confident the maid can never remove them, even if my rules are wrong.
4. *As a CI author*, I run `docker_maid clean --apply` as the last pipeline step and check its exit code; `0` means clean, non-zero surfaces what failed.
5. *As a power user*, I run `docker_maid daemon --apply` and it continuously watches and reaps, keeping me at zero sprawl with no cron jobs.
6. *As a cautious user*, I run `docker_maid clean` with no flags first, see the exact plan (what would be deleted and why), then opt in with `--apply`.
7. *As a human*, I run `docker_maid tui`, see sprawl at a glance (disk gauge, counts, worst offenders), review the pending plan, and apply it with one confirmation — no Docker CLI flags to remember.
8. *As a coding agent*, I run `docker_maid status --format json` and `docker_maid clean --apply --format json` in a pipeline and parse a documented, versioned schema — no interactive prompt ever blocks me, no TTY is ever assumed.
9. *As a script author*, when I pipe docker_maid output it detects the non-TTY and emits plain text or JSON — never ANSI escapes unless I explicitly set `CLICOLOR_FORCE=1`, and never an alternate screen.

## 6. Functional Requirements

Priorities: **P0** = required for the v0.1 release (delivered across M0–M2, §10), **P1** = fast-follow, **P2** = later.

### F1. Configuration file — P0

- Format: **TOML** (idiomatic Rust: `serde` + `toml`), loaded from `./docker_maid.toml`, `$XDG_CONFIG_HOME/docker_maid/config.toml`, or `--config <path>` (first match wins; explicit flag overrides).
- `docker_maid config check` (or `clean --check-config`) validates the file: unknown keys are **errors** (not silently ignored), every rule must reference at least one selector (**sole exception:** build-cache rules, which cannot select and must instead set `allow_unscoped = true`, F4.4), TTL/age values must parse.
- Config supports: global defaults, per-resource-type rule sets, ownership selectors, protection lists, schedule/interval, log/report settings. Full sketch in §8.
- Config is hot-reloadable in daemon mode on file change (P1) and via `SIGHUP` (P0).

### F2. Inventory & classification — P0

- Enumerate via Docker API (no shelling out): containers (incl. stopped), images, volumes, networks, build cache.
- For each object, compute a **disposition**: `protected` / `owned (rule R)` / `authorized-unscoped (rule R)` / `unowned`. Never act on `unowned`. An object becomes `authorized-unscoped` only through the two-key escape hatch in F4.4.
- Classification inputs: labels (exact + glob), name patterns (regex), compose project labels, age (`Created`/`FinishedAt`), state, image ancestry (dangling, intermediate build layers).

### F3. Policies (the rule set) — P0

| Policy | Default | Description |
|---|---|---|
| `stopped_ttl` | — | Remove owned containers that exited more than N ago |
| `running_ttl` | — | Remove owned containers *running* longer than N (kills agent zombies; off by default) |
| `dangling_images` | on | Remove untagged, unreferenced images owned per selectors |
| `image_tag_patterns` | — | Remove images matching patterns once unreferenced (e.g. `agent-*:latest`) |
| `orphan_volumes` | — | Remove owned volumes not attached to any container (with age floor) |
| `orphan_networks` | — | Remove user-defined networks with zero containers, matching selectors |
| `build_cache` | — | Prune builder cache older than N / over budget. **Always unscoped**: build-cache records carry no labels/names, so this rule type requires `allow_unscoped = true` and logs the WARN banner (F4.4) |
| `disk_budget` | P1 | Aggressive mode: when Docker data root exceeds X GB, reap oldest-first until under budget |

- Every removal policy carries a rationale in logs/reports: which rule matched, which selector matched (or why the rule is authorized-unscoped), and how old the object was.

### F4. Ownership & safety model — P0

This is the product's core differentiator. Rules, in order of precedence:

1. **Protected set always wins.** Nothing in the protected set can ever be removed by any rule. Protection is re-checked immediately before every delete. The set is the union of two persisted sources:
   - **Config `[protect]`** (§8) — human-authored, version-controlled, removable only by editing the config.
   - **State file `protection.toml`** at `$XDG_STATE_HOME/docker_maid/` (default `~/.local/state/docker_maid/`) — machine-managed, written by `docker_maid protect`/`unprotect` and the TUI's `p` key. Entries are **typed** (`container <id|name-pattern>`, `volume <id>`, `image <ref>`, `network <name>`) and survive restarts. Writers take an exclusive inter-process lock on `protection.lock` across the full read-modify-write transaction, then write a temporary file, `fsync`, rename, and `fsync` the parent directory. The state directory is mode `0700` and files are `0600` where the platform supports Unix permissions. The executor takes a shared lock, reloads the effective protected set, and holds that lock through each Docker delete request; a completed protection update therefore cannot be lost or race a deletion. `unprotect` removes only state-file entries; entries that came from config are refused with a pointer to the config line.
2. **Owned by label.** The maid stamps resources it manages with `dev.docker-maid.managed=true` when it or the agent creates them (P1: `spawn` subcommand); rules may also *adopt* existing resources via label/name selectors — adoption is visible in reports.
3. **Ownership by well-known agent labels.** First-class selector presets for common sandboxes (devcontainer's `devcontainer.local_folder`, generic `ai-agent.*` labels) so users write `policy = "agent-sandboxes"` instead of regex.
4. **Explicit unowned targeting is possible but must be loud.** A rule with `scope = "all"` and `allow_unscoped = true` may classify matching objects as `authorized-unscoped`; deletion still requires the normal `--apply` or confirmed-TUI-plan authorization. Each plan and applied pass logs a WARN summary. This is the escape hatch for "just prune everything dangling," made deliberately awkward. **Build cache is unscoped by nature** — Docker exposes no ownership metadata on cache records — so `rules.build_cache` *must* carry `allow_unscoped = true`; the WARN banner makes each such pass visible.
5. **Dry-run default.** No mutating flag ⇒ plan-only. `--apply` required for `clean` and `daemon`.

### F5. Execution modes — P0

- **`clean`** — single pass: inventory → classify → plan → (apply | print). **`--apply` is the complete non-interactive authorization**: with it, no confirmation is ever prompted; without it, nothing mutates. Cron- and agent-friendly.
- **`daemon`** — same authorization as `clean`: `--apply` required to mutate; without it the daemon is a read-only monitor that logs the plans it *would* run. Reaps every `interval` (default 5m), SIGHUP/config-watch reload, SIGTERM drains gracefully (never kills mid-delete; deletes are individually atomic anyway).
- **`tui`** (P0) — interactive ratatui frontend over the same engine: dashboard, inventory browser, plan review, activity log, rules viewer (§F9). Requires **both stdin and stdout** to be TTYs; refuses to start (exit `4`) otherwise.
- **`status`** (P0) — one-shot snapshot of inventory + dispositions, last-pass actions, and per-rule match counts with `previous_match_count` from the activity journal (§F7) — rule-health regression is surfaced **here, as data** (`health: "regressed"`), not via exit codes; `--format json` for agents, `--follow` NDJSON streaming (P1).
- **`plan`** (P0) — same as dry-run `clean`; exit `1` has exactly one meaning: **removals are pending**. (Rule-health regression is reported by `status`, above.)
- **`protect` / `unprotect`** (P0) — typed, non-interactive management of the protection state file: `docker_maid protect <container|volume|image|network> <id|name-pattern>` (mirrors the TUI's `p` key; persistence and precedence in F4.1).

**Stable exit codes for all commands**

| Code | Meaning |
|---|---|
| `0` | Command completed successfully |
| `1` | `plan` found pending removals; no other command uses this code |
| `2` | A deletion pass ran, but at least one target failed or became ineligible; successful deletions are reported |
| `3` | Configuration or schema is invalid or unreadable |
| `4` | `tui` lacks a TTY on stdin or stdout; no other command uses this code |
| `5` | Docker endpoint is unavailable, incompatible, or failed before a deletion pass could run |
| `6` | Local protection or activity state could not be read or written safely |
| `7` | Unclassified internal failure |
| `64` | Invalid CLI invocation or unsupported option combination |

### F6. CLI UX — P0

```
# --format table|json is a GLOBAL option on every non-interactive subcommand.
# (clean, daemon, status, plan, protect/unprotect, config, version).
# --json is a documented alias for --format json. `tui` accepts neither.
# One-shot commands emit one JSON document; daemon emits NDJSON events.

docker_maid tui  [--config PATH] [--attach]          # interactive UI (needs stdin+stdout TTY)
docker_maid clean [--apply] [--config PATH] [--quiet]
docker_maid daemon [--apply] [--config PATH] [--interval 5m]
docker_maid status [--follow]                        # --follow NDJSON (P1)
docker_maid plan                                     # exit 1 when removals are pending
docker_maid protect|unprotect <container|volume|image|network> <id|name-pattern>…
docker_maid config check|print|default
docker_maid version
```

- Output defaults to `table` (human); `--format json` (alias `--json`) switches to the stable agent schema (§F10).
- `daemon --format json` emits one schema-versioned NDJSON event per lifecycle, plan, action, and pass-summary event. This stream is P0 and describes that daemon process only. The P1 `status --follow` command provides a separate client attachment interface.
- Color precedence, in order: (1) JSON output is **never** colored, regardless of env; (2) `NO_COLOR` disables color; (3) `CLICOLOR_FORCE=1` forces color even when piped; (4) otherwise color iff stdout is a TTY. Spinners/progress bars render only on a TTY.
- `--apply` is the single non-interactive authorization; there is no `--yes`. Interactive confirmation exists only inside the TUI's modals (§F9).
- Every TUI action has a one-command equivalent (parity table in §F10) — the TUI is a view, not a privilege.
- `config default` prints a fully-commented starter config.

### F7. Reporting & logging — P0 (basic) / P1 (full)

- **Activity journal (P0).** JSONL at `$XDG_STATE_HOME/docker_maid/activity.jsonl` — machine-written history that survives process restarts. Every event carries `schema_version`, a globally unique `pass_id`, `source` (`clean|daemon|tui`), `sequence`, `timestamp`, and `config_hash`. Event kinds are `pass_started`, `action` (action, object id/name, matched rule, age, freed bytes), and `pass_summary` (completion timestamp, per-rule match counts, removed count, reclaimed bytes, failures). Readers define “last pass” as the most recent completed `pass_summary` and join its actions by `pass_id`; interleaved processes cannot corrupt that view. Writers take an exclusive `activity.lock` for each complete record append and for rotation; readers take a shared lock while loading a snapshot. The active segment is append-only. Rotation occurs when either limit is reached and retains the newest 10,000 events subject to a 5 MB total cap across active and rotated segments. The journal is the data source for `status` last-pass data, rule-health baselines, and the TUI Activity view + sparkline — no P0 view depends on P1 storage.
- End-of-run summary: counts + reclaimed disk space (`docker system df` delta).
- Per-run JSON report files (config: `report.path`, keep-last N) — P1 (a richer superset of the journal).
- No telemetry/phone-home. Ever.

### F8. Environment & integrations — P0 (Unix socket) / P1 (remote)

- Connect via `DOCKER_HOST`, `DOCKER_CONTEXT`, default Unix socket; TCP + TLS + SSH contexts (P1).
- Rootless Docker and Docker Desktop supported.
- Compatible with `sysbox`/`gVisor` sandboxes (they're just containers to us).
- Works alongside Portainer/Watchtower without conflicts (maid never *starts* anything, and respects the same labels others use).

### F9. TUI (`docker_maid tui`) — P0

A ratatui-based interactive frontend (crossterm backend). The TUI is a **pure view** over the same disposition snapshot the CLI consumes — frontends contain no policy logic.

**Layout & navigation**

```
┌ docker_maid ─────────────────────────────────── [daemon ● next pass 3m] ┐
│ [1] Dashboard  [2] Inventory  [3] Plan  [4] Activity  [5] Rules         │
├───────────────────────────────────────────────┬─────────────────────────┤
│                                               │  Detail pane            │
│         (view-specific content)               │  (selected object:      │
│                                               │   labels, mounts,       │
│                                               │   why-owned, age)       │
└───────────────────────────────────────────────┴─────────────────────────┘
  ↑↓/jk move · / filter · p protect · a apply plan · ? help · q quit
```

**Views**

1. **Dashboard** (default) — `Gauge` widgets for Docker disk usage (and budget, when `disk_budget` is set); disposition counts (protected / owned / authorized-unscoped / unowned) per resource type; last-pass summary (removed, freed bytes); sparkline of reclaimed bytes over recent passes (from the journal's `pass_summary` events); daemon status line.
2. **Inventory** — `Tabs` for containers / images / volumes / networks / build cache; `Table` with columns Name · State · Age · Size · Disposition · Matched rule; `/` opens a fuzzy filter; `Enter` focuses the detail pane; `p` toggles protection (writes the typed state file, F4.1). **No per-object delete key in v1** — deletion flows exclusively through policy-generated plans.
3. **Plan** — the fixed target set proposed for the next apply operation, grouped by rule with match counts and a freed-space estimate; `y` opens its confirmation modal.
4. **Activity** — scrolling log of past actions (timestamp, action, object, rule, freed bytes), read from the activity journal (§F7) so history survives TUI restarts; live events append while running.
5. **Rules** — read-only view of the loaded config with live per-rule match counters and hot-reload status.

**Interaction & safety**

- **The TUI can only ever apply a policy-generated plan.** There is no direct "delete this one object" action in v1, so a single deletion contract holds everywhere: every removal requires (a) a currently matched rule, (b) not protected, and (c) explicit authorization — a confirmed TUI plan or `--apply`, never both at once. (Typed single-object removal — `docker_maid rm <type> <id>` plus a TUI equivalent under the same contract — is a P2 candidate only if users ask.)
- Every plan carries a `plan_id`, `config_hash`, creation timestamp, and immutable target-ID set. Plan application (`a` in Inventory, `y` in Plan) opens a modal that lists that target set; `Enter` authorizes that exact plan and `Esc` cancels. The executor may skip targets that fail delete-time revalidation, but it never adds targets. If the config hash changes, the plan is stale and must be regenerated and confirmed again.
- Protecting an object is one keystroke (`p`) and instantly reflected in the disposition column.
- Vi-style navigation, `/` search, `?` help overlay, `1–5` view switching, `r` force refresh.
- Event-driven rendering: redraw only on input or data change; background re-inventory every `[tui] refresh` interval. No busy loop; idle CPU ~0.
- Crash-safe: alternate screen + raw mode with a panic hook and signal handlers that restore the terminal on normal exit, handled termination signals, and unwinding panics; `SIGKILL` and process aborts cannot be recovered. Verified over ssh, tmux, and screen.
- Mouse support (scroll) — P2.

**Attach vs. standalone**

- Standalone (default): the TUI runs its own inventory passes against the Docker socket.
- `--attach` (P1): connects to a running daemon's local IPC socket for shared live state and a tailed activity log.

### F10. Machine interface (agent mode) — P0

> **Principle: "The TUI is a view, not the product."** Coding agents cannot drive interactive screens — so docker_maid must be fully operable *and* fully introspectable without one.

| Face | Audience | Contract |
|---|---|---|
| `tui` | humans at a terminal | ratatui, keybindings above |
| table output (default) | humans in scripts & pipes | plain text; zero ANSI when piped unless `CLICOLOR_FORCE=1` |
| `--format json` | agents & CI | stable, versioned, documented schema |

1. **Stable JSON schema.** Every non-interactive command accepts the global `--format json` (alias `--json`; `tui` accepts neither). One-shot payloads and every daemon NDJSON record carry `"schema_version": 1`; within a major version, changes are additive-only. Schema documented at `docs/schema.md` and validated in CI against fixture files. JSON mode never emits ANSI codes, spinners, or progress bars. Stdout is always machine-parseable; stderr follows the selected output format.
2. **Machine-readable errors, too.** In JSON mode, fatal failures leave stdout empty and emit one JSON error envelope on **stderr** — `{"schema_version":1,"error":{"kind":"config_invalid|docker_unreachable|state_io|partial_failure|internal|…","message":"…","details":[…]}}`. A partial deletion returns its result payload, including successful and failed targets, on stdout and emits a summary error envelope on stderr. Exit codes follow the global table in F5 in both output formats.
3. **TTY detection covers input *and* output.** `docker_maid tui` starts only when **both stdin and stdout are TTYs**; otherwise it exits `4` immediately with a one-line stderr hint — it never hangs waiting for input. Non-interactive commands never inspect the TTY for control flow, only for color (precedence in §F6).
4. **No blocking prompts, anywhere.** `clean`/`daemon` mutate only under `--apply` and then never prompt (no `--yes` flag exists); interactive confirmation exists solely inside the TUI's modal flow.
5. **Full parity, enforced.** Every TUI capability is a CLI verb (`status`, `plan`, `clean`, `protect`/`unprotect`, `config`). The docs contain a parity table, and a CI check keeps it true — the TUI can never quietly gain a monopoly on a feature.
6. **One-call introspection.** `docker_maid status --format json` returns the whole world in one document — config summary, inventory with dispositions, last-pass actions, disk usage — so an agent never has to shell out to `docker` and scrape output.
7. **Streaming.** `daemon --format json` emits P0 NDJSON for its own events. `status --follow --format json` is P1 and attaches to a running daemon to stream those events to another process.
8. **MCP server (P2).** `docker_maid mcp` exposing plan/clean/status as native agent tools. Gated on real demand; the JSON CLI remains the compatibility contract.

## 7. Non-Functional Requirements

| Category | Requirement |
|---|---|
| **Performance** | Full inventory + plan on a daemon with 1,000 containers < 2s; steady-state daemon RSS < 20 MB; idle CPU ~0 (interval-driven, not poll-hammering) |
| **Safety** | No delete without (a) explicit authorization (`--apply` or a confirmed immutable TUI plan), (b) a rule that still matches at delete time, (c) a fresh protected-set check, and (d) re-verified object state. Delete-time revalidation may shrink a plan but never expand it; a config revision change invalidates the plan. |
| **Reliability** | Individual delete failures never abort the pass; partial success is reported honestly via exit code 2 |
| **TUI** | Event-driven rendering (redraw on change only, idle ~0 CPU); frame render < 16 ms; works over ssh/tmux/screen; terminal state restored on normal exit, handled termination signals, and unwinding panic; TUI-mode RSS < 30 MB |
| **Machine I/O** | JSON schema (payloads + error envelopes) validated against fixtures in CI; zero ANSI escapes in JSON mode; TTY detection (both streams, incl. `tui` refusal) covered by integration tests |
| **Portability** | Linux + macOS first-class; Windows via Docker Desktop/WSL best-effort. Static musl builds published via cargo-dist / GitHub Releases |
| **Security** | Read-only use of Docker API except authorized deletes; never runs containers itself (P1 `spawn` is opt-in); no telemetry or unsolicited network egress. Network traffic is limited to the explicitly configured Docker endpoint. |
| **Maintainability** | 2021-edition Rust, MSRV policy documented, `clippy -D warnings` clean, unit tests for every disposition rule, integration tests against docker-in-docker in CI |
| **Size** | Release binary < 10 MB; startup < 50 ms |

## 8. Configuration Spec (v0 sketch)

```toml
# docker_maid.toml — full annotated template via `docker_maid config default`

[defaults]
interval        = "5m"          # daemon pass cadence
log_level       = "info"

[protect]                          # human-authored, version-controlled
names   = ["^postgres-prod$"]      # regex list — hard immune
labels  = ["com.example.prod=true"]
# Runtime additions (`docker_maid protect` / TUI `p`) live in a separate
# machine-managed state file: $XDG_STATE_HOME/docker_maid/protection.toml
# (typed entries, locked atomic transactions). Effective set = both sources,
# unioned (F4.1).

# ---- rule sets: first matching rule wins per object ----

[[rules.containers]]
name        = "agent-sandboxes"
description = "Reap coding-agent containers"
select.labels = ["ai-agent.*", "devcontainer.local_folder=*"]  # glob
stopped_ttl   = "2h"        # exited > 2h ago → remove
running_ttl   = "24h"       # (optional) running > 24h → stop+remove
adopt          = true       # treat matches as managed without relabeling

[[rules.containers]]
name        = "my-builders"
select.names = ["^buildkit-session-"]
stopped_ttl  = "30m"

[[rules.images]]
dangling          = true
unused_for        = "1h"       # no container references it
select.name_parts = ["agent-", "build-"]

[[rules.volumes]]
select.labels = ["ai-agent.workspace=true"]
orphan_for    = "48h"          # detached from all containers for 48h

[[rules.networks]]
select.names  = ["^agent-net-"]
orphan        = true           # zero connected containers

[rules.build_cache]
# Build-cache records carry NO labels/names — they cannot be owned.
# This rule type is always unscoped and therefore REQUIRES the escape
# hatch; every pass logs the WARN banner (F4.4).
older_than     = "7d"
allow_unscoped = true

# Escape hatch — requires explicit config opt-in and logs a warning banner;
# deletion also requires --apply or confirmation of an immutable TUI plan:
# [[rules.images]]
# name = "everything-dangling"
# scope = "all"
# allow_unscoped = true
# dangling = true

[report]
enabled  = true
path     = "~/.local/state/docker_maid/report.json"
keep     = 30

[tui]
refresh = "5m"     # TUI background re-inventory cadence
mouse   = false    # P2
```

## 9. Architecture Overview

```
┌─────────────┐     ┌──────────────┐     ┌──────────────────┐
│ config.toml │────▶│ PolicyEngine │────▶│  Dispositions    │
└─────────────┘     └──────────────┘     └────────┬─────────┘
                             ▲                     │ plan
                    reload (SIGHUP/watch)          ▼
┌────────────────────────┐  ┌──────────────┐  ┌──────────────────┐
│ Frontends              │  │  Executor    │─▶│  Docker API      │
│  • tui (ratatui)       │◀─│ (verify→act) │  │  (bollard crate) │
│  • table (humans)      │  └──────┬───────┘  └──────────────────┘
│  • json (agents, v1)   │         │
└────────────────────────┘         ▼
                          ┌──────────────────┐
                          │ Reporter/Logger  │
                          └──────────────────┘
```

- **Crate choices:** `bollard` (async Docker API client), `tokio` (runtime), `clap` (CLI), `ratatui` + `crossterm` (TUI frontend), `serde` + `toml` (config), `tracing` (logging), `notify` (config hot-reload, P1), `humantime-serde` (TTL parsing), `regex`/`globset` (selectors).
- **Pure core:** the classification logic (inventory → disposition) is a pure function over data, making it exhaustively unit-testable — the safety-critical part of the codebase has no I/O. The TUI and JSON outputs are thin projections of that snapshot; no policy logic lives in any frontend.
- **Executor re-verifies** each fixed plan target immediately before deletion: the config hash is unchanged, the resource still exists, the same removal rule still matches its current labels/ownership/state/age, and the resource is absent from a freshly loaded protected set. Any failed check records `skipped_revalidation`; it never causes the executor to add a replacement target. The executor holds the shared protection lock through the Docker delete request.
- **State store** lives under `$XDG_STATE_HOME/docker_maid/`: `protection.toml` (typed protect entries, F4.1) and `activity.jsonl` plus rotated segments (journal, §F7). `protection.lock` serializes protection updates against delete-time checks; `activity.lock` serializes journal appends, snapshots, and rotation. CLI and TUI state writers use the same locking and durability implementation.

## 10. Milestones

| Milestone | Scope | Exit criteria |
|---|---|---|
| **M0 — Walking skeleton** (week 1–2) | CLI skeleton, config load/validate, inventory via bollard, dry-run plan output | `docker_maid clean` prints a correct plan on a messy daemon |
| **M1 — Core engine** (week 3–5) | Container/image/volume/network rules, `--apply`, protect list (config + typed state file), daemon mode + SIGHUP, exit codes, activity journal, summary logs | All §5 stories 1–6 pass manually; dind integration tests plus concurrent-protection, journal-interleaving, stale-plan, and delete-time-revalidation tests pass in CI |
| **M2 — Faces: TUI + agent mode** (week 6–9) | ratatui TUI (dashboard, inventory, plan review, activity, rules), agent JSON schema v1 + `docs/schema.md` (payloads + error envelopes + daemon NDJSON), `protect`/`unprotect` verbs, unscoped build-cache rule, reports, `config default`, config hot-reload, release binaries | v0.1 published (all P0 shipped); README quickstart includes a 60-second TUI tour **and** a zero-TTY agent example; one-shot JSON, daemon NDJSON, error-envelope, and exit-code fixtures are green in CI |
| **M3 — Differentiators** (later) | `disk_budget` aggressive mode, agent-label presets, `spawn` (labeled sandbox creation so agents inherit ownership), `tui --attach` + `status --follow` NDJSON, mouse support, MCP server (if demanded), Homebrew/nix install | Community feedback loop running |

## 11. Success Metrics

- **Sprawl reduction:** on a busy agent workstation, `docker ps -a` count and `docker system df` reclaimable bytes trend to ~0 within one interval of abandonment.
- **Safety record (hard gate):** zero reports of protected-resource deletion or unowned-resource deletion without an `authorized-unscoped` rule — any such bug is release-blocking (P0).
- **Adoption signals:** GitHub stars/issues, brew installs, and repeat CI usage (proxy: download counts per version).
- **Trust signal:** > 90% of users never need `allow_unscoped` (measured only via voluntary issue feedback — no telemetry).
- **Agent usability (hard gate):** every documented workflow runs without a pseudo-TTY; JSON schema stays additive-only across minor versions.

## 12. Risks & Mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| Deleting something a user needed | Product-killing | Protect-first architecture, locked protection updates, dry-run default, immutable plan/apply split, current-rule revalidation before delete, adoption visibility in reports |
| Docker API drift (build cache, rootless quirks) | Broken inventory | Bollard pinned + capability-probed; degrade gracefully (skip unknown object types with WARN, never guess) |
| Agent tools change their label conventions | Rules silently stop matching | Rule-health baselines in the activity journal: `status` marks a rule whose match count drops to zero as `health: "regressed"` + WARN log (structured data — `plan` exit 1 means only "removals pending"); docs for writing custom selectors |
| Windows/WSL edge cases | Support burden | Explicitly best-effort in v1; CI matrix Linux/macOS first |
| Scope creep toward "Docker manager" | Lost focus | Non-Goals §3 enforced at PR review; the maid reaps, it does not parent |
| TUI terminal incompatibilities (ssh/tmux/Windows Terminal quirks) | Broken UI erodes trust | ratatui+crossterm (battle-tested combo), panic-hook restore, CI smoke test inside tmux; TUI is optional — headless CLI always works |
| JSON schema drift breaks agent integrations | Agents are our loudest users | `schema_version` pinning, fixture tests in CI, additive-only policy, deprecation notices in release notes |
| Concurrent processes lose state or interleave history | Protection failure or misleading status | Inter-process locks, durable atomic protection writes, correlated `pass_id` journal events, serialized rotation, concurrency tests in CI |

## 13. Open Questions

1. Should `running_ttl` also support **stop-grace** (SIGTERM, wait, then SIGKILL) rather than straight remove? (Leaning yes — agents may write logs/state on SIGTERM.)
2. Do we need per-rule `notify` hooks (e.g., desktop notification when a long-running agent sandbox is reaped), or is the report enough for v1?
3. Multi-config layering (system + user + repo-local `.docker_maid.toml` with merge semantics) — power feature or YAGNI until asked for?
4. Should `spawn` (P1) integrate with devcontainer CLI conventions directly, or stay label-agnostic?
5. Should the TUI ever become the default for interactive TTYs (lazygit-style), or remain an explicit subcommand?
6. Is the MCP server worth shipping in v0.1 to meet agents where they are, or does the JSON CLI suffice until users ask?

---

*End of PRD v0.3 — feedback welcome; this document lives and versions with the repo.*
