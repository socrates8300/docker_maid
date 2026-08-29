# docker_maid

Policy-driven cleanup for Docker resources that coding agents leave behind.

docker_maid inventories one Docker host, classifies each object against a
declarative TOML policy, and removes only what a rule selected and an
authorization confirmed. Production workloads that share the daemon stay
untouched unless you write a rule that names them.

> [!IMPORTANT]
> **Alpha (`0.1.0-alpha.1`).** The CLI, daemon, TUI, and versioned JSON
> interface work from source. Nothing is deleted unless you pass `--apply`
> or confirm a plan in the TUI.

```sh
# See what the current policy would remove. Changes nothing.
docker_maid plan

# Remove only those targets, after a fresh re-check.
docker_maid clean --apply
```

## Install

You need [Rust 1.91+](https://rustup.rs/) and a running Docker Engine
(Linux, or macOS via OrbStack / Colima / Docker Desktop).

```sh
git clone https://github.com/socrates8300/docker_maid.git
cd docker_maid
cargo build --release
cargo install --path .
```

The binary talks to the Docker API over the socket. There is no Python
runtime, no sidecar container, and no extra daemon besides Docker itself.

### OrbStack (macOS)

OrbStack is a drop-in Docker Desktop replacement and needs no special
support: docker_maid speaks the standard Engine API and follows the socket.
Setup notes recorded when this operator migrated (evidence in
docs/evidence/2026-08-29--docker-debris-capture.md):

- `orbctl docker migrate` copies containers, volumes, and image records —
  not compose-created networks. Recreate those networks with their
  compose-project labels before starting migrated containers.
- The system socket is not handed over automatically while Docker Desktop's
  privileged helper lives. After removing Docker Desktop:
  `sudo ln -sfn $HOME/.orbstack/run/docker.sock /var/run/docker.sock`.
- Put `~/.orbstack/bin` before other docker installations in `PATH`; IDEs
  hardcode `/usr/local/bin/docker`, so symlink it to
  `~/.orbstack/bin/docker`.
- `docker_maid status` prints the daemon identity (`Daemon:` line) and the
  machine document's `daemon` block reports it. docker_maid uses bollard and
  ignores docker CLI contexts, so if the CLI and docker_maid disagree about
  which engine is live, the status line names the one docker_maid is
  actually talking to.

On macOS with Colima, point the client at Colima's socket if `DOCKER_HOST`
is unset:

```sh
export DOCKER_HOST=unix://$HOME/.colima/default/docker.sock
```

Otherwise docker_maid uses `DOCKER_HOST` when set, or Docker's local
default socket. If a named Docker context uses another endpoint, export
that endpoint first.

## Quick start

### 1. Let the TUI write the first policy

```sh
docker_maid tui
```

If no configuration exists, the TUI opens **Setup**. It reads Docker,
shows exact ownership evidence (agent labels, Compose projects, prefixes
you type), and writes a reviewed proposal. You do not have to hand-write
TOML first.

The TUI needs a real terminal. Piped or redirected stdio exits `4`.

### 2. Preview, then apply

```sh
docker_maid plan              # dry run; exit 1 means removals are pending
docker_maid clean             # same dry-run boundary as plan
docker_maid clean --apply     # one authorized pass; never prompts
```

### 3. Keep a shared host tidy

```sh
# Watch. Docker events wake a pass; the interval is the quiet-host backstop.
docker_maid daemon

# Same loop, with deletion authorized.
docker_maid daemon --apply
```

### 4. Pin anything that must survive

```sh
docker_maid protect container '^postgres-prod$'
docker_maid protect label com.docker.compose.project=immich
docker_maid status
```

## Why this exists

Coding agents create short-lived containers, images, volumes, networks,
and build cache. Interrupted sessions leave that work behind. Over time
`docker ps -a` becomes unreadable, disk fills, and the default bridge
pool runs out of addresses.

`docker system prune` is too broad when real services and agent sandboxes
share one daemon. docker_maid acts only through:

- an explicit ownership rule, or
- an intentionally enabled unscoped policy (build cache, and `scope = "all"`
  with `allow_unscoped = true`).

## Safety model

The deletion contract is the same in the TUI, the tables, and `--json`.

- Non-interactive commands are dry-run unless `--apply` is present.
- The TUI applies only a policy-generated, immutable plan after one
  confirmation. There is no per-object delete key.
- There is no `--yes` flag and no direct-delete shortcut.
- The protected set is the union of `[protect]` in the config file and
  typed runtime entries from `protect` / `unprotect`.
- Protection always wins over a cleanup rule.
- Before every delete, the executor reloads the exact config file, rejects
  a changed config, re-inventories Docker, and requires the same ID, rule,
  disposition, and removal decision.
- Revalidation can drop targets from a plan. It never adds them.
- Unowned resources require `scope = "all"` and `allow_unscoped = true`.
- Image, volume, and network age floors measure **continuous
  observed-unreferenced time**, not creation age. The first observation
  starts the clock at zero and never deletes.
- Docker events only wake the planner. They do not delete, infer
  ownership, or inspect liveness.

## Commands

| Command | What it does |
|---|---|
| `tui` | Review what a policy would remove, protect what it should not, set it up |
| `config default` | Print a commented starter with no active cleanup rule |
| `config check` / `print` | Validate or normalize a file |
| `config survey` | Discover exact ownership evidence (read-only) |
| `config propose` | Build a reviewable proposal; writes nothing |
| `config write` | Compare-and-swap a reviewed proposal into the config path |
| `plan` | Inventory + pending removals; never mutates Docker |
| `clean` | Dry-run plan (exit `1` if anything is pending) |
| `clean --apply` | Execute that plan after revalidation |
| `daemon` | Startup pass, then event-woken / interval backstop |
| `daemon --apply` | Same loop with deletion authorized |
| `status` | Disposition counts, disk, last completed pass |
| `protect` / `unprotect` | Typed runtime protection (not the config file) |
| `labels` | Canonical ownership label keys this build understands |
| `stamp` | Emit those labels for the caller to apply at create time |
| `spawn` | Create one stamped sandbox and return; no attach, no wait |
| `init --agents` | Install the portable skills that teach an agent the CLI |

Global flags: `--config <path>`, `--format table|json`, `--json`.

### Interactive TUI

```sh
docker_maid tui
```

Three views share the same inventory, plan, protection store, executor,
and activity journal as the CLI:

| View | Key | What it answers |
|---|---|---|
| **Review** | `1` | What would be removed, and why. The default. |
| **Keeping** | `2` | What is safe, and why. Where you protect things. |
| **Setup** | `3` | Guided configuration from real ownership evidence. |

**Review** is the screen a Docker GUI cannot give you. Each pending
removal gets three lines: what it is, how old and how big, and the
policy reason that claimed it:

```text
WOULD REMOVE  (2)
▶ container    agent-box
       3d old · 120.0 MiB
       why:  matched agent label ai-agent.owner=ci
             state age 3d meets 2h
```

**Keeping** is one compact line per resource and expands the selected
row, because it routinely holds sixty or more.

Keys, in every view: `1` `2` `3` switch views, `↑` `↓` or `j` `k` move,
`space` protects or releases the selected object, `P` does the same for
its whole label family, `enter` opens the details pane, `/` narrows the
list, `c` approves a name prefix, `a` reviews and applies the removal
set, `l` opens the activity log, `r` refreshes, `?` help, `q` quit. In
**Setup**, `←` `→` change the profile and `[` `]` then `e` edit a value,
`v` previews, `s` saves.

The footer is generated from the same table the key handler reads, so it
cannot advertise a key that does nothing. A test enforces both
directions: every advertised key resolves and moves the interface, and
every bound key appears in the footer or in `?`.

Colour never carries meaning alone. Red is only "would be removed",
green is only "protected", yellow is only "needs your decision", and
every coloured row also sits under a heading that says the same thing in
words. Selection uses reverse video, not a background colour.

A filter narrows what you read, never what you confirm. The confirmation
modal always lists the full removal set and says how many rows the
filter is hiding.

Below 60x20 the interface draws one message giving the current and
required size instead of a layout that cannot hold its own content.

`space` and `P` write typed runtime state. Neither edits your
configuration file. Build cache always opens a dedicated modal, because
it carries no ownership evidence at all.

### Guided configuration without the TUI

```sh
# Read-only discovery. Copy candidate IDs. Pass the same policy flags you
# will propose with so the Compose warnings match.
docker_maid config survey --profile workstation --volume-ttl 72h

# Reviewable artifact. Does not write config.
docker_maid config propose \
  --profile workstation \
  --candidate compose/my-project-ab12cd34 \
  --volume-ttl 72h \
  --format json > proposal.json

# Compare-and-swap into the default XDG config path.
docker_maid config write --proposal proposal.json

docker_maid plan
```

The configurator discovers:

- known coding-agent labels
- exact `com.docker.compose.project` families
- name prefixes you explicitly enter
- build cache as a separate authorized-unscoped choice

It does not infer ownership from arbitrary names. Human views show agent
labels first, Compose families second, prefixes next, and build cache
last. Machine JSON keeps the candidate vector stable so IDs and TUI
indexes do not shift.

Compose candidates carry a warning computed from the rules the proposal
will generate, against the current inventory. A running or referenced
stack can preview zero removals now; a family that already has eligible
members states its pending count. The warning also states what those
same rules can remove after `docker compose down`.

New files go to `$XDG_CONFIG_HOME/docker_maid/config.toml`, or
`~/.config/docker_maid/config.toml`. Existing explicit or loaded paths
stay in place. Writes use a sibling process lock, source and inventory
hash checks, a `.bak` copy, same-directory atomic replacement, `fsync`
of the file and parent directory, mode `0700` directories, and mode
`0600` files on Unix.

The configurator owns only its marked region and rule IDs under
`docker-maid.configure/`. Manual rules, comments, and ordering stay
untouched. Overlapping selections and generated rules that an earlier
manual rule would shadow are refused.

Lookup order for every command: `--config <path>`, `./docker_maid.toml`,
then `$XDG_CONFIG_HOME/docker_maid/config.toml`. Unknown keys and broken
safety invariants are errors. Name regular expressions and label globs
are validated before Docker is contacted. Configuration failures exit
`3`.

Low-level helpers:

```sh
docker_maid config default    # commented starter; no active rule
docker_maid config check
docker_maid config print
```

### Plan

`plan` inventories containers, images, volumes, networks, and build
cache through the Docker API. It applies the first matching rule, checks
effective protection first, and prints only pending removals.

```sh
docker_maid plan
```

Sort order is resource type, name, then immutable Docker ID. Container
image, volume, and network references are resolved before orphan or
unused policies. Built-in Docker networks are implicitly protected.

Docker exposes no last-used or detach timestamp for images, volumes, or
networks. Their floors (`unused_for`, `orphan_for`) therefore measure how
long docker_maid has continuously *observed* the resource unreferenced,
in `$XDG_STATE_HOME/docker_maid/observation.toml`. A volume that existed
for months and detached a minute ago is one minute old by this clock.
Attaching it again clears the record. A later detach starts the clock
over. A host that cannot persist that file never accumulates time, so
nothing there becomes eligible.

A rule with no age floor removes nothing and says so. Container floors
still use Docker's own state timestamps.

Build-cache records expose no ownership metadata. Their single rule
requires `allow_unscoped = true` plus `older_than`, `max_bytes`, or both.
Cache age uses Docker's last-used timestamp, with creation time as a
fallback. `max_bytes` selects oldest inactive records until the cache is
within budget. Records in use or shared with an image are kept. Records
with no usable age are not selected. Every configured cache pass emits
an authorized-unscoped warning.

### Clean

`clean` without `--apply` is the same dry-run as `plan`. `--apply` is
the complete non-interactive authorization.

```sh
docker_maid clean
docker_maid clean --apply
```

Target IDs come only from the initial policy plan. A target that
disappears, becomes protected, gains a reference, or otherwise becomes
ineligible is skipped. The pass continues and exits `2` after any skip
or deletion failure; successful deletions remain reported.

Container deletion does not remove anonymous volumes. Image deletion
disables parent-image pruning. Image and volume deletion are not forced;
Docker's own reference checks are the last barrier after revalidation.

Build-cache deletes use Docker's prune endpoint with one exact cache ID
per request. Graph children are processed before parents. An empty prune
response is a skip. An unexpected cache ID returned by Docker is a
failure.

### Daemon

`daemon` runs a pass immediately, then waits.

- Docker events wake **one** planner pass after 500 ms of quiet.
- A burst is one wake, not one pass per event.
- The configured interval is the backstop when Docker is quiet or events
  never stop.
- Every wake uses the same plan, protection lock, revalidation, and
  `--apply` gate as `clean`.
- Event payloads are discarded. They do not infer ownership or inspect
  liveness. Events have no delete path.

```sh
docker_maid daemon
docker_maid daemon --apply
docker_maid daemon --apply --interval 30s
```

On macOS and Linux, `SIGHUP` starts an immediate pass with the latest
configuration. `SIGTERM` and `SIGINT` wait for the current pass to
finish, then exit `0`. Applied daemon passes record `source = "daemon"`
in the activity journal. With `--format json`, the process emits one
NDJSON lifecycle, plan, action, summary, or recoverable-error event per
line.

Docker, configuration, or state failures are reported and retried at the
next interval. The process does not busy-loop.

### Protection and activity

Runtime protection is typed and non-interactive:

```sh
docker_maid protect container '^agent-session-important$'
docker_maid protect image agent-base:latest
docker_maid protect volume workspace-data
docker_maid protect network shared-services
docker_maid protect label com.docker.compose.project=immich

docker_maid unprotect network shared-services
```

A `label` entry is one exact `key=value` pair. It protects every
container, image, volume, and network carrying that pair, so one entry
covers a whole Compose project or agent family. Matching is
byte-for-byte on both halves: `project=immich` never protects
`project=immich-staging`. Build cache records expose no Docker labels,
so a label entry never matches them. This is narrower than configuration
`protect.labels`, which are globs matched against the key or the whole
pair.

Entries persist in `$XDG_STATE_HOME/docker_maid/protection.toml`, or
`~/.local/state/docker_maid/protection.toml` when `XDG_STATE_HOME` is
unset. That file is `schema_version = 2`. Version 1 files are read
unchanged and rewritten at version 2 by the next protection change.
An older build reading a version 2 file stops with exit `6` rather than
silently ignoring label entries it cannot represent.

Concurrent writers use one exclusive lock and an atomic, durable file
replacement. The state directory is mode `0700` and its files are mode
`0600` on Unix. Repeated `protect` and `unprotect` are idempotent.

Configuration `[protect]` and runtime entries form one set. `unprotect`
cannot remove a matching configuration-sourced entry; the diagnostic
names the field you must edit. A name is checked against
`protect.names`, and a label pair against `protect.labels`, so a
catch-all name regex is never blamed for a label. Before each delete,
the executor reloads runtime state under a shared inter-process lock and
holds that lock through the Docker request.

Every `clean --apply` or `daemon --apply` pass appends schema-versioned,
correlated events to `activity.jsonl`. Complete records are serialized
across processes. History is bounded to 10,000 events and 5 MiB.
`status` reports current disposition counts and the most recent completed
pass after a process restart.

Protection or activity state failures stop the command with exit `6`.

## Configuration

Policy lives in TOML. A rule match **is** the ownership statement. There
is no flag that switches adoption on, and no third state between owned
and unowned. `scope = "all"` with `allow_unscoped = true` is the one
loud escape hatch for unowned objects.

An older file that still carries `adopt` is refused. The error names the
retired key and tells you to delete the line; removing it changes no
decision.

```toml
[[rules.containers]]
name = "agent-sandboxes"
description = "Reap stopped coding-agent containers"
select.labels = ["ai-agent.*", "devcontainer.local_folder=*"]
stopped_ttl = "2h"
```

```toml
[rules.build_cache]
older_than = "7d"
max_bytes = 10737418240
allow_unscoped = true
```

The three named profiles are editable starting values for the
configurator, not hidden defaults the engine applies on its own:

| Profile | Stopped containers | Images | Volumes | Build cache after opt-in |
|---|---:|---:|---:|---:|
| Shared Host | 24h | 7d | 14d | 30d / 20 GiB |
| Workstation | 2h | 24h | 48h | 7d / 10 GiB |
| Ephemeral CI | 15m | 1h | 6h | 24h / 5 GiB |

Human-authored protection stays in the configuration file:

```toml
[protect]
names = ["^postgres-prod$"]
labels = ["com.example.prod=true"]
```

Runtime protection and activity history live separately under
`$XDG_STATE_HOME/docker_maid/` with locked, durable writes. Automation
never edits the manual `[protect]` table.

## Agents

Coding agents are first-class users of this CLI. Drive the binary. Do
not reimplement it, and do not wrap it in your own sandbox launcher.

### Stamp at creation

Docker fixes labels when a resource is created. Nothing can relabel a
container, image, volume, or network afterwards. Apply the ownership
stamp at create time or the resource stays anonymous.

```sh
docker_maid labels
docker_maid --json labels

docker_maid stamp
docker_maid --json stamp --owner my-agent
docker_maid stamp --owner my-agent --docker-args
```

`labels` needs no daemon and no configuration. It is the only list that
counts: a key it does not advertise is not ownership evidence.

The flag line is meant to be interpolated. `--owner` accepts letters,
digits, dot, dash, and underscore. Anything else is refused rather than
quoted, because a space would split into two Docker arguments.

```sh
docker run -d $(docker_maid stamp --owner my-agent --docker-args) alpine sleep 600
docker volume create $(docker_maid stamp --owner my-agent --docker-args)
docker network create $(docker_maid stamp --owner my-agent --docker-args) my-net
```

`stamp` reads no configuration, contacts no daemon, and changes nothing.
Adoption by rule stays the first route; stamping only makes a new
resource obvious to `config survey`.

### Spawn a stamped sandbox

```sh
docker_maid spawn --image my-sandbox:latest --owner my-agent \
  --workspace /absolute/path/to/project -- npm test
```

The host directory is bound at `/workspace`, which is also the working
directory unless `--workdir` says otherwise.

Two properties matter more than the flags:

- **It does not parent the agent.** The sandbox is always detached and is
  never removed automatically. It outlives this command. Nothing attaches
  to its streams. No cleanup is tied to the command exiting. An exited
  sandbox is still there to be inventoried, which is what lets a rule
  adopt and later reclaim it.
- **It does not proxy Docker.** There is no route for ports, networks,
  environment, users, capabilities, or limits, and it never pulls an
  image. For any of those, run Docker yourself and interpolate `stamp`.

An image that is not present locally is an error naming the
`docker pull` to run. The maid never reaches the network on your behalf.

### Teach an agent the CLI

`docker_maid init --agents` installs two portable skills compiled into
the binary. Installing them needs no network, and neither one reads or
edits your configuration.

| Skill | Teaches |
|---|---|
| `docker-maid` | Creating stamped resources and reclaiming them |
| `docker-maid-config` | Writing, applying, and proving a policy file |

Each lands at `<skills>/<skill-name>/SKILL.md`.

```sh
docker_maid init --agents --target claude
docker_maid init --agents --target codex
docker_maid init --agents --target generic --dest /path/to/skills
docker_maid init --agents --target claude --skill docker-maid-config
```

The target is required rather than guessed. Reinstalling reports
`unchanged`. A skill you have edited yourself is kept until you pass
`--force`, and a refusal on one skill installs none of them. `--skill`
selects a subset; the default is all of them.

The first skill sends an agent to `spawn` and `stamp` rather than
reimplementing them. The second teaches the strict configuration schema,
the selector syntax, and why a brand new policy removes nothing on its
first pass.

### Machine output

Every non-interactive command accepts `--format json` (`--json` is the
alias). One-shot commands emit one schema-versioned document. Fatal
errors leave stdout empty and emit one schema-versioned error document
on stderr. Exit codes do not change between table and JSON formats.

```sh
docker_maid status --format json
docker_maid clean --apply --format json
docker_maid daemon --apply --format json
```

JSON output contains no ANSI escapes, spinners, progress bars, or
prompts. Schema version 1 is additive-only and documented in
[docs/schema.md](docs/schema.md).

| Code | Meaning |
|---:|---|
| `0` | Success |
| `1` | `plan` / dry-run `clean` found pending removals |
| `2` | An applied pass skipped or failed at least one target |
| `3` | Configuration missing, unreadable, or invalid |
| `4` | `tui` does not have terminal stdin and stdout |
| `5` | Docker is unavailable or incompatible |
| `6` | Protection, observation, activity, or skill-install state failed |
| `7` | Output or an internal invariant failed |
| `64` | Invalid command invocation |

Exit `1` is information: a dry run found pending removals. Treat exit
`2` as a real problem and read the outcomes in the result document.

## What this tool will not do

- Proxy Docker, pull images, or supervise a container's lifecycle.
- Infer ownership from a name you did not select.
- Hunt process liveness or parent a coding agent.
- Auto-edit the manual `[protect]` table.
- Run `docker system prune` or add targets after the plan is sealed.
- Manage Kubernetes, Podman, or more than one Docker host.

## Developers

Minimum supported Rust version: **1.91**.

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --offline
```

Live tests talk to the Docker daemon on the machine that runs them.
They are gated and off by default:

```sh
DOCKER_MAID_LIVE_TEST=1 cargo test
```

Do not run the live suite against a host whose leftover fixtures you
cannot afford to confuse with the test prefix. The tests create and
remove disposable objects; they are not a cleanup of your daemon.

### Layout

| Path | Role |
|---|---|
| `src/config.rs` | Strict TOML schema, retired-key diagnostics |
| `src/configurator.rs` | Survey, propose, write |
| `src/inventory.rs` | Docker inventory via bollard |
| `src/plan.rs` | Inventory → disposition → immutable plan |
| `src/executor.rs` | Revalidate, lock protection, delete |
| `src/state.rs` | Runtime protection store |
| `src/observation.rs` | Observed-unreferenced clocks |
| `src/activity.rs` | Durable JSONL journal |
| `src/machine.rs` | Versioned JSON / NDJSON |
| `src/tui.rs` | ratatui / crossterm frontend |
| `src/wakeup.rs` | Event debounce and interval backstop |
| `src/labels.rs` | Sole ownership-key table |
| `src/stamp.rs` / `spawn.rs` | Creation-time stamp and thin sandbox |
| `src/agent_skill.rs` | Embedded portable skills |
| `docs/schema.md` | Machine schema v1 |
| `assets/agent-skill/SKILL.md` | Resource skill compiled into the binary |
| `assets/agent-skill-config/SKILL.md` | Policy-authoring skill compiled into the binary |

The safety-critical core is a pure inventory-to-disposition pipeline. It
produces immutable plans for a separate executor. Frontends contain no
policy logic.

Dependencies of note: `clap`, `serde`, `toml` / `toml_edit`, `bollard`,
`tokio`, `ratatui`, `crossterm`, `fs2`. `unsafe_code` is forbidden.

### Product notes

- [PRD](PRD.md) is the longer product contract, including non-goals and
  milestone history.
- Open an [issue](https://github.com/socrates8300/docker_maid/issues)
  for design or implementation feedback.

## License

[MIT](LICENSE).
