# docker_maid

> Declare what agent sprawl looks like; docker_maid keeps it at zero.

> [!IMPORTANT]
> **Project status: early implementation.** The configuration CLI works from
> source. Docker inventory, cleanup, daemon, JSON mode, and TUI are not
> implemented or released yet. Commands in planned sections do not work.

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

The planned deletion contract is the same in every interface:

- Non-interactive commands are dry-run unless `--apply` is present.
- The TUI can apply only a policy-generated, immutable plan after confirmation.
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

## Available now

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
invariants are errors. Configuration failures exit with code `3`.

## Planned cleanup quickstart

The commands in this section describe the planned v0.1 cleanup interface. They
do not work yet.

```sh
# Review pending cleanup. Nothing is deleted.
docker_maid plan

# Run one authorized cleanup pass without prompts.
docker_maid clean --apply

# Run continuous authorized cleanup.
docker_maid daemon --apply
```

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

Human-authored protection rules will remain in the configuration file.
Runtime protection entries and activity history will be stored separately
under `$XDG_STATE_HOME/docker_maid/` with locked, durable writes.

Build-cache records do not expose ownership metadata. A build-cache rule will
therefore require `allow_unscoped = true` and will emit a warning for every
planned and applied pass.

## Agents and CI

Agent workflows will not need a pseudo-terminal or Docker CLI output parsing:

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

The implemented configuration slice uses `clap`, `serde`, `toml`, and
`humantime`. Planned runtime layers will use `bollard` for the Docker API,
`tokio` for asynchronous execution, and `ratatui` with `crossterm` for the TUI.

The safety-critical core is a pure inventory-to-disposition pipeline. It
produces immutable plans for a separate executor, which rechecks the current
configuration, rule match, resource state, and protected set before each
delete request.

## Roadmap

- **M0 — Walking skeleton (in progress):** configuration, Docker inventory, and dry-run plans.
- **M1 — Core engine:** cleanup rules, protection, daemon mode, and activity history.
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
