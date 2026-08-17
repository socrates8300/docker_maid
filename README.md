# docker_maid

> Declare what agent sprawl looks like; docker_maid keeps it at zero.

> [!IMPORTANT]
> **Project status: alpha.** The CLI, daemon, versioned JSON/NDJSON machine
> interface, durable protection and activity state, and interactive TUI work
> from source. Cleanup remains dry-run unless it receives explicit CLI or TUI
> authorization.

docker_maid is an early-stage Rust CLI that will reclaim Docker resources left
behind by coding-agent workflows. It targets one Docker host at a time and puts
deletion policy, protection, and auditability ahead of aggressive cleanup.

The complete product contract is in the [PRD](PRD.md).

Build the executable, then start the guided configurator:

```sh
cargo build --release
cargo run --release -- tui
```

The TUI can create the first configuration. You do not need to hand-write TOML
first. `cargo install --path .` installs `docker_maid` on your command path.

## Why docker_maid?

Coding agents create short-lived containers, images, volumes, networks, and
build cache. Interrupted sessions often leave those resources behind. Over
time, Docker becomes difficult to inspect, disk space disappears, and network
address pools can be exhausted.

`docker system prune` is too broad when production workloads and agent
sandboxes share a daemon. docker_maid is designed to act only through explicit
ownership rules or an intentionally enabled unscoped policy.

## Safety model

The deletion contract is the same in every interface:

- Non-interactive commands are dry-run unless `--apply` is present.
- The TUI can apply only a policy-generated, immutable plan after confirmation.
- The protected set is the union of configuration and typed runtime state.
- Protected resources always win over cleanup rules.
- Every target must still match its rule immediately before deletion.
- Delete-time revalidation can remove targets from a plan, but never add them.
- Unowned resources require `scope = "all"` and `allow_unscoped = true`.

There is no `--yes` flag and no direct-delete shortcut in v1.

## One product, three interfaces

| Interface | Intended user | Contract |
|---|---|---|
| `docker_maid tui` | People at a terminal | Guided configuration, dashboard, inventory, plan review, and activity |
| Table output | People and shell scripts | Readable text with automatic color control |
| `--format json` | Coding agents and CI | Available: versioned JSON, NDJSON streams, and machine-readable errors |

All three interfaces use the same inventory, classification, planning, and
execution core. Frontends contain no policy logic.

## Available now: deterministic configuration

The configurator reads Docker, finds exact ownership evidence, creates a
reviewable proposal, and saves only after stale-state checks. It does not infer
ownership from arbitrary names. It discovers:

- known coding-agent labels;
- exact `com.docker.compose.project` families;
- name prefixes that the operator explicitly enters;
- build cache as a separate authorized-unscoped choice.

Human views show agent-label families first, Compose families second, explicit
name prefixes next, and build cache last. Machine JSON keeps the canonical
candidate vector unchanged so candidate IDs and TUI selection indexes remain
stable.

Compose candidates carry a warning computed from the rules that the proposal
will generate, evaluated against the current inventory. A running or
referenced stack says it can preview zero removals now; a family that already
has eligible members states its current pending count instead. The warning
also states what those same rules can remove after `docker compose down` or
another detach. `config survey` accepts the same `--profile` and TTL override
flags as `config propose`, so the warning you read during discovery is the
warning your proposal will carry.

Volume, image, and network floors measure **continuous observed-unreferenced
time**, not resource creation age. docker_maid records first-seen-unreferenced
in `$XDG_STATE_HOME/docker_maid/observation.toml` on every policy pass. A
volume that has existed for months and detached a minute ago is one minute old
by this clock, so adopting a long-running project cannot reap its data on the
first pass. Attaching a resource again clears its record and a later detach
starts the clock over. A host that cannot persist that file never accumulates
time, so nothing there becomes eligible.

Unselected and unlabeled objects remain unowned. The three profiles provide
editable starting values:

| Profile | Stopped containers | Images | Volumes | Build cache after explicit opt-in |
|---|---:|---:|---:|---:|
| Shared Host | 24h | 7d | 14d | 30d / 20 GiB |
| Workstation | 2h | 24h | 48h | 7d / 10 GiB |
| Ephemeral CI | 15m | 1h | 6h | 24h / 5 GiB |

Use the TUI, or run the same workflow headlessly:

```sh
# Read-only discovery. Copy one or more candidate IDs. Pass the same policy
# flags you will propose with so the warnings match.
docker_maid config survey --profile workstation --volume-ttl 72h

# Create a versioned review artifact. This does not write config.
docker_maid config propose \
  --profile workstation \
  --candidate compose/my-project-ab12cd34 \
  --volume-ttl 72h \
  --format json > proposal.json

# Compare-and-swap the reviewed artifact into the default XDG config path.
docker_maid config write --proposal proposal.json

# Inspect the exact resulting removal plan. No deletion occurs.
docker_maid plan
```

New files go to `$XDG_CONFIG_HOME/docker_maid/config.toml`, or
`$HOME/.config/docker_maid/config.toml`. Existing explicit or loaded config
paths stay in place. Writes use a sibling process lock, source and inventory
hash checks, a `.bak` copy, same-directory atomic replacement, file and parent
directory sync, mode `0700` directories, and mode `0600` files on Unix.

Manual rules, comments, and ordering stay untouched. The configurator owns
only its marked region and rule IDs under `docker-maid.configure/`. It blocks
overlapping candidate selections and generated rules that an earlier manual
rule would shadow.

The low-level config commands remain available:

```sh
# Print a fully commented safe starter with no active cleanup rule.
docker_maid config default

docker_maid config check
docker_maid config print
```

Configuration lookup order is `--config <path>`, `./docker_maid.toml`, then
`$XDG_CONFIG_HOME/docker_maid/config.toml`. Unknown keys and invalid safety
invariants are errors. Name regular expressions and label globs are validated
before Docker is contacted. Configuration failures exit with code `3`.

## Available now: read-only plan

`plan` inventories containers, images, volumes, networks, and build cache
through the Docker API. It applies the first matching rule, checks effective
protection first, and prints only pending removals. It never changes Docker.

```sh
# Uses ./docker_maid.toml. Exit 1 means removals are pending.
cargo run -- plan
```

The planner sorts by resource type, name, and immutable Docker ID. It identifies
container image, volume, and network references before it evaluates orphan or
unused policies. Docker exposes no last-used or detach timestamp, so image
`unused_for`, volume `orphan_for`, and network `orphan_for` measure how long
docker_maid has continuously *observed* the resource unreferenced, recorded in
`$XDG_STATE_HOME/docker_maid/observation.toml`. The first pass that sees a
resource unreferenced starts its clock at zero and can never remove it. An
image, volume, or network rule with no age floor removes nothing and says so:
without a floor there is no measurement to trust.
Container floors still use Docker's own state timestamps. Built-in Docker
networks are implicitly protected.

The client uses `DOCKER_HOST` when it is set. Otherwise, it uses Docker's local
default socket. If a named Docker context uses another socket, export that
context's endpoint first. For example, Colima commonly uses:

```sh
export DOCKER_HOST=unix://$HOME/.colima/default/docker.sock
cargo run -- plan
```

Build-cache records expose no ownership metadata. Their single rule therefore
requires `allow_unscoped = true` plus `older_than`, `max_bytes`, or both. Cache
age uses Docker's last-used timestamp, with creation time as a fallback.
`max_bytes` selects oldest inactive records until the cache is within budget.
Records in use or shared with an image are kept. Records with no usable age are
not selected by an age or oldest-first policy. Every configured cache pass
emits an authorized-unscoped warning.

## Available now: one-shot cleanup

`clean` without `--apply` is the same dry-run boundary as `plan`. `--apply` is
the complete non-interactive authorization and never opens a prompt.

```sh
# Dry run. Exit 1 means removals are pending.
cargo run -- clean

# Run one authorized cleanup pass without prompts.
cargo run -- clean --apply
```

The target IDs come only from the initial policy plan. Before every delete,
the executor reloads the exact configuration file, rejects a changed config,
re-inventories Docker, and requires the same ID, rule, disposition, and removal
decision. A target that disappears, becomes protected, gains a reference, or
otherwise becomes ineligible is skipped. The pass continues and exits `2`
after any skip or deletion failure; successful deletions remain reported.

Container deletion does not remove anonymous volumes. Image deletion disables
parent-image pruning. Image and volume deletion are not forced, and Docker's
reference checks provide the last barrier against state changes after
revalidation.

Build-cache deletes use Docker's prune endpoint with one exact cache ID per
request. Cache graph children are processed before parents. The executor treats
an empty prune response as a skip and reports any unexpected cache ID returned
by Docker as a failure.

## Available now: durable protection and activity

Runtime protection entries are typed and non-interactive:

```sh
docker_maid protect container '^agent-session-important$'
docker_maid protect image agent-base:latest
docker_maid protect volume workspace-data
docker_maid protect network shared-services
docker_maid protect label com.docker.compose.project=immich

docker_maid unprotect network shared-services
```

A `label` entry is one exact `key=value` pair. It protects every container,
image, volume, and network carrying that pair, so one entry covers a whole
Compose project or agent family at once. Matching is byte-for-byte on both the
key and the value: `project=immich` never protects `project=immich-staging`.
Build cache records expose no Docker labels, so a label entry never matches
them. This is deliberately narrower than configuration `protect.labels`, which
are globs matched against the key or the whole pair.

Entries persist in `$XDG_STATE_HOME/docker_maid/protection.toml`, or
`~/.local/state/docker_maid/protection.toml` when `XDG_STATE_HOME` is unset.
That file is `schema_version = 2`. Version 1 files are read unchanged and are
rewritten at version 2 by the next protection change, so an upgrade needs no
migration step. An older build reading a version 2 file stops with exit `6`
rather than silently ignoring the label entries it cannot represent.
Concurrent writers use one exclusive lock and an atomic, durable file
replacement. The state directory is mode `0700` and its files are mode `0600`
on Unix. Repeated `protect` and `unprotect` operations are idempotent.

Configuration `[protect]` entries and runtime entries form one protected set.
`unprotect` cannot remove a matching configuration-sourced entry; its diagnostic
points to the configuration field that must be edited. A name is checked against
`protect.names`, and a label pair against `protect.labels`, so a catch-all name
regex is never blamed for a label. Before each delete, the
executor reloads runtime state under a shared inter-process lock and holds that
lock through the Docker request.

Every `clean --apply` or `daemon --apply` pass appends schema-versioned,
correlated events to `activity.jsonl`. Complete records are serialized across
processes. History is bounded to 10,000 events and 5 MiB. `status` reports
current disposition counts and the most recent completed pass after a process
restart:

```sh
docker_maid status
```

Protection or activity state failures stop the command with exit code `6`.

## Available now: daemon execution

`daemon` runs a pass immediately, then waits for the configured interval. It is
a read-only monitor unless `--apply` is explicit. Every pass reloads the full
configuration and protection state. Docker, configuration, or state failures
are reported and retried at the next interval without busy-looping.

```sh
# Monitor every five minutes without mutation.
docker_maid daemon

# Run continuous authorized cleanup.
docker_maid daemon --apply

# Override `[defaults].interval` for this process.
docker_maid daemon --apply --interval 30s
```

On macOS and Linux, `SIGHUP` starts an immediate pass with the latest
configuration. `SIGTERM` and `SIGINT` wait for the current pass to finish, then
exit successfully. Applied daemon passes use `source = "daemon"` in the durable
activity journal. With `--format json`, the daemon emits one NDJSON lifecycle,
plan, action, summary, or recoverable-error event per line.

## Available now: interactive TUI

```sh
cargo run --release -- tui
```

If no configuration exists, the TUI opens **Configure** automatically. Docker
objects remain unowned until you select exact ownership evidence.

The five views use the same inventory, policy plan, protection store, executor,
and activity journal as the non-interactive commands:

1. In **Configure**, select exact agent-label or Compose families.
2. Change the named profile with `h`/`l`. Select editable values with `[`/`]`
   and press `e` to enter a duration or cache byte budget.
3. Press `v` to create the real before/after plan preview.
4. Press `s` and confirm the config-only write. The TUI refreshes **Plan**.
5. Inspect **Inventory** to see why each object is owned or protected.
6. In **Plan**, press `y`, inspect the confirmation, and press `Enter` to authorize
   that exact plan.
7. Open **Activity** to inspect the resulting actions and reclaimed bytes.

Use `1`–`5` to switch views, `j`/`k` to move, `/` to filter, `c` to approve a
name prefix from the selected inventory object, `p` to toggle runtime
protection for that one object, `P` to toggle one label protection for its whole
ownership family, `r` to refresh, `?` for help, and `q` to quit. Both keys write
typed runtime state; neither edits your configuration file. Build cache always
opens a dedicated unscoped-warning modal. There is no per-object delete key.

The TUI refuses to start unless both stdin and stdout are terminals. It exits
with code `4` and a one-line hint instead of waiting on a pipe. Normal exit,
handled termination signals, and panics restore raw mode and the alternate
screen.

## Configuration

Policy lives in `docker_maid.toml`. This example adopts labeled agent
containers and removes them two hours after they stop:

```toml
[[rules.containers]]
name = "agent-sandboxes"
description = "Reap stopped coding-agent containers"
select.labels = ["ai-agent.*", "devcontainer.local_folder=*"]
stopped_ttl = "2h"
```

A rule match is the adoption. Any resource a rule selects is `owned`, so
there is no key that switches adoption on and no third ownership state
between owned and unowned. `scope = "all"` with `allow_unscoped = true`
remains the one loud escape hatch for unowned objects.

An older file that still carries `adopt` is refused by the strict schema.
The error names the retired key and tells you to delete the line; removing it
changes no decision.

Human-authored protection rules remain in the configuration file. Runtime
protection entries and activity history are stored separately under
`$XDG_STATE_HOME/docker_maid/` with locked, durable writes.

### Canonical ownership labels

`docker_maid labels` prints every Docker label key this build treats as
ownership evidence, with who writes it and why it counts:

```
docker_maid labels
docker_maid --json labels
```

The command needs no Docker daemon and no configuration. It is the single
source of that list: the ownership survey, the family lookup behind the TUI
protect action, and this command all read one table, so a key cannot be
advertised here while the policy engine ignores it.

A resource carrying one of these keys is evidence `config survey` can offer to
adopt. Any other key is ignored. Keys shown with a trailing `*` are namespaces,
matched by prefix, and that is how you write them in a selector.

An agent that stamps one of these keys on what it creates becomes discoverable
without any further configuration.

### Stamping what an agent creates

Docker fixes labels at creation. There is no API to relabel an existing
container, image, volume, or network, so `docker_maid` cannot walk up to a
resource and mark it as its own. `docker_maid stamp` therefore emits the labels
and the caller applies them at creation:

```
docker_maid stamp                                # the pairs, with an example
docker_maid --json stamp --owner my-agent        # for a tool
docker_maid stamp --owner my-agent --docker-args # for a shell
```

The flag line is meant to be interpolated, and no value ever needs quoting:

```sh
docker run -d $(docker_maid stamp --owner my-agent --docker-args) alpine sleep 600
docker volume create $(docker_maid stamp --owner my-agent --docker-args)
```

`--owner` accepts letters, digits, dot, dash, and underscore. Anything else is
refused rather than quoted, because a name holding a space would split into two
Docker arguments once a shell expands the line.

The stamp writes only keys `docker_maid labels` advertises, so `config survey`
offers the stamped resource for adoption on the next pass. `stamp` reads no
configuration, contacts no daemon, and changes nothing. Adoption by rule stays
the first route; stamping only makes a new resource obvious.

### Spawning a stamped sandbox

`docker_maid spawn` creates one container already carrying the stamp, so an
agent inherits ownership without having to remember it:

```
docker_maid spawn --image my-sandbox:latest --owner my-agent \
  --workspace /absolute/path/to/project -- npm test
```

The host directory is bound at `/workspace`, which also becomes the working
directory unless `--workdir` says otherwise.

Two properties matter more than the flags:

- **It does not parent the agent.** The sandbox is always detached and is never
  removed automatically. It outlives this command, nothing attaches to its
  streams, and no cleanup is tied to the command exiting. An exited sandbox is
  still there to be inventoried, which is what lets a rule adopt and later
  reclaim it.
- **It does not proxy Docker.** There is no route here for ports, networks,
  environment variables, users, capabilities, or limits, and it never pulls an
  image. For any of those, run Docker yourself:

```sh
docker run -d --network mynet -e KEY=value \
  $(docker_maid stamp --owner my-agent --docker-args) my-sandbox:latest
```

An image that is not present locally is an error naming the `docker pull` to
run, because the maid never reaches the network on your behalf.

### Teaching an agent to use the CLI

`docker_maid init --agents` installs a portable skill that tells a coding agent
how to drive this tool. The skill is compiled into the binary, so installing it
needs no network:

```
docker_maid init --agents --target claude
docker_maid init --agents --target codex
docker_maid init --agents --target generic --dest /path/to/skills
```

It writes exactly one file, `<skills>/docker-maid/SKILL.md`, and nothing else.
It never reads or edits your configuration: policy stays human-owned.

The target is required rather than guessed, because the alternative is writing
into a home directory nobody chose. Reinstalling reports `unchanged` and needs
no flags. A skill you have edited yourself is kept until you pass `--force`.

The skill teaches the CLI; it does not reimplement it. It sends an agent to
`spawn` and `stamp` rather than to a wrapper of its own, and a test holds every
command it mentions against this build's actual command surface.

Build-cache records do not expose ownership metadata. Configure their explicit
escape hatch in bytes and durations:

```toml
[rules.build_cache]
older_than = "7d"
max_bytes = 10737418240
allow_unscoped = true
```

The rule emits a warning for every planned and applied pass.

## Agents and CI

Every non-interactive command accepts `--format json`; `--json` is its alias.
One-shot commands emit one schema-versioned document. Fatal errors leave stdout
empty and emit one schema-versioned error document on stderr. Exit codes do not
change between table and JSON formats.

```sh
# Inspect config, inventory, dispositions, history, and disk usage.
docker_maid status --format json

# Apply a cleanup pass and receive a versioned result document.
docker_maid clean --apply --format json

# Stream this daemon's versioned NDJSON events.
docker_maid daemon --apply --format json
```

JSON output contains no ANSI escapes, spinners, progress bars, or prompts.
The version 1 schema is additive-only and documented in
[docs/schema.md](docs/schema.md). Stable exit codes are:

| Code | Meaning |
|---|---|
| `0` | Success |
| `1` | `plan` found pending removals |
| `2` | A cleanup pass had target failures or newly ineligible targets |
| `3` | Invalid or unreadable configuration |
| `4` | `tui` does not have terminal stdin and stdout |
| `5` | Docker is unavailable or incompatible |
| `6` | Local protection or activity state failed |
| `7` | Unclassified internal failure |
| `64` | Invalid command invocation |

## Architecture

The implemented configurator, planning, one-shot execution, daemon,
protection-state, activity-journal, and machine-interface slices use `clap`,
`serde`, `serde_json`, `toml`, `toml_edit`, `humantime`, `regex`, `globset`, and `fs2`. The
Docker adapter uses `bollard` and `tokio` without shelling out. The TUI uses
`ratatui` and `crossterm` over the same core.

The safety-critical core is a pure inventory-to-disposition pipeline. It
produces immutable plans for a separate executor, which rechecks the current
configuration, rule match, resource state, and protected set before each
delete request.

## Roadmap

- **M0 — Walking skeleton (implemented from source):** guided configuration, Docker inventory,
  dry-run plans, and conservative one-shot cleanup for all five resource types.
- **M1 — Core engine (implemented from source):** durable protection, activity
  history, and interval-driven daemon execution.
- **M2 — v0.1 interfaces (implemented from source):** stable machine schemas
  and the standalone TUI. Release packaging and broader CI remain.
- **M3 — Later:** disk budgets, sandbox spawning, daemon attachment, and MCP.

See the [PRD milestones](PRD.md#10-milestones) for exit criteria and the full
v1 boundary.

## Development

The minimum supported Rust version is 1.91.

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

Use the [PRD](PRD.md) for product decisions and open an
[issue](https://github.com/socrates8300/docker_maid/issues) for design or
implementation feedback.

## License

docker_maid is available under the [MIT License](LICENSE).
