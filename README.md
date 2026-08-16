# docker_maid

> Declare what agent sprawl looks like; docker_maid keeps it at zero.

> [!IMPORTANT]
> **Project status: early implementation.** Strict configuration, read-only
> planning, and one-shot cleanup for containers, images, volumes, networks, and
> build cache work from source. Typed runtime protection and durable cleanup
> activity history and daemon mode also work from source. JSON mode and the TUI
> are not implemented or released yet. Commands in planned sections do not work.

docker_maid is an early-stage Rust CLI that will reclaim Docker resources left
behind by coding-agent workflows. It targets one Docker host at a time and puts
deletion policy, protection, and auditability ahead of aggressive cleanup.

The complete product contract is in the [PRD](PRD.md).

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
- The planned TUI can apply only a policy-generated, immutable plan after
  confirmation.
- The protected set is the union of configuration and typed runtime state.
- Protected resources always win over cleanup rules.
- Every target must still match its rule immediately before deletion.
- Delete-time revalidation can remove targets from a plan, but never add them.
- Unowned resources require `scope = "all"` and `allow_unscoped = true`.

There is no `--yes` flag and no direct-delete shortcut in v1.

## One product, three interfaces

| Interface | Intended user | Planned contract |
|---|---|---|
| `docker_maid tui` | People at a terminal | Dashboard, inventory, plan review, activity, and rules |
| Table output | People and shell scripts | Readable text with automatic color control |
| `--format json` | Coding agents and CI | Versioned JSON, NDJSON streams, and machine-readable errors |

All three interfaces use the same inventory, classification, planning, and
execution core. Frontends contain no policy logic.

## Available now: configuration

Build the binary and generate, validate, or normalize a strict TOML
configuration:

```sh
cargo build

cargo run -- config default > docker_maid.toml
cargo run -- config check
cargo run -- config print
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
unused policies. In this stateless slice, image `unused_for` and volume
`orphan_for` are resource-age floors, not duration since last use; Docker does
not expose a last-used or detach timestamp in these list responses. Built-in
Docker networks are implicitly protected.

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

docker_maid unprotect network shared-services
```

Entries persist in `$XDG_STATE_HOME/docker_maid/protection.toml`, or
`~/.local/state/docker_maid/protection.toml` when `XDG_STATE_HOME` is unset.
Concurrent writers use one exclusive lock and an atomic, durable file
replacement. The state directory is mode `0700` and its files are mode `0600`
on Unix. Repeated `protect` and `unprotect` operations are idempotent.

Configuration `[protect]` entries and runtime entries form one protected set.
`unprotect` cannot remove a matching configuration-sourced name; its diagnostic
points to the configuration field that must be edited. Before each delete, the
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
activity journal. Versioned NDJSON daemon output remains planned.

### Planned TUI flow in 60 seconds

```sh
docker_maid tui
```

1. Open **Dashboard** to inspect Docker usage and disposition counts.
2. Open **Inventory** to inspect why each resource is owned or protected.
3. Open **Plan** to review the fixed target set.
4. Press `y`, inspect the confirmation modal, and press `Enter` to authorize
   that exact plan.
5. Open **Activity** to inspect the resulting actions and reclaimed bytes.

The TUI will refuse to start unless both stdin and stdout are terminals.

## Configuration

Policy lives in `docker_maid.toml`. This example adopts labeled agent
containers and removes them two hours after they stop:

```toml
[[rules.containers]]
name = "agent-sandboxes"
description = "Reap stopped coding-agent containers"
select.labels = ["ai-agent.*", "devcontainer.local_folder=*"]
stopped_ttl = "2h"
adopt = true
```

Human-authored protection rules remain in the configuration file. Runtime
protection entries and activity history are stored separately under
`$XDG_STATE_HOME/docker_maid/` with locked, durable writes.

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

The current table-form `status` and `daemon` commands are available now.
Versioned JSON and NDJSON streams remain planned for agent workflows:

```sh
# Inspect config, inventory, dispositions, history, and disk usage.
docker_maid status --format json

# Apply a cleanup pass and receive a versioned result document.
docker_maid clean --apply --format json

# Stream this daemon's versioned NDJSON events.
docker_maid daemon --apply --format json
```

JSON output will never contain ANSI escapes, spinners, or progress bars.
Fatal JSON-mode errors will use a versioned envelope on stderr. Planned stable
exit codes are:

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

The implemented configuration, planning, one-shot execution, daemon,
protection-state, and activity-journal slices use `clap`, `serde`, `toml`,
`humantime`, `regex`, `globset`, and `fs2`. The Docker adapter uses `bollard`
and `tokio` without shelling out. Planned runtime layers will use `ratatui` with
`crossterm` for the TUI.

The safety-critical core is a pure inventory-to-disposition pipeline. It
produces immutable plans for a separate executor, which rechecks the current
configuration, rule match, resource state, and protected set before each
delete request.

## Roadmap

- **M0 — Walking skeleton (in progress):** configuration, Docker inventory,
  dry-run plans, and conservative one-shot cleanup for all five resource types.
- **M1 — Core engine (implemented from source):** durable protection, activity
  history, and interval-driven daemon execution.
- **M2 — v0.1 interfaces:** TUI, stable machine schemas, reports, and releases.
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
