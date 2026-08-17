---
name: docker-maid-config
description: "Author, apply, and prove a docker_maid policy file. Use when a repository has no docker_maid.toml, when a cleanup pass removes nothing you expected it to remove, or when you are asked to make agent-created Docker resources reclaimable on a shared host."
---

# Writing a docker_maid policy

`docker_maid` removes nothing until a rule selects it. With no configuration
file there is no policy, and with a policy that has no rules every resource is
reported as unowned and kept. Stamping resources is not enough on its own: the
stamp makes a resource *findable*, and a rule makes it *removable*.

This document is about the file. The companion `docker-maid` skill covers
creating and stamping resources.

Never hand-edit an operator's existing configuration. Propose, show the
proposal, and let a human decide.

## Where the file goes

Two implicit locations, with different filenames:

1. `./docker_maid.toml` in the working directory.
2. `$XDG_CONFIG_HOME/docker_maid/config.toml`.

The first that exists wins. `--config <PATH>` overrides both and is used even
when the file is missing, so the error names the path you chose.

No configuration at all is a hard failure, not a default: `plan`, `clean`,
`daemon`, and `status` exit `3`.

## Start from the tool, not a blank file

Two starting points. Prefer the second.

A commented starter that is safe because it contains no active rule:

```sh
docker_maid config default > docker_maid.toml
```

Or derive rules from resources that actually exist. This is better because
every selector it writes is one the daemon confirmed:

```sh
docker_maid --json config survey
docker_maid --json config propose --candidate <ID> > proposal.json
docker_maid --json config write --proposal proposal.json
docker_maid config check --config ./docker_maid.toml
docker_maid --json plan
```

Steps 2, 3, and 4 must run back-to-back. A proposal records a hash of the
current file and a signature of the current Docker inventory, and `config write`
refuses a proposal whose world has moved. That refusal is the feature: it means
nothing is written on top of a change you did not see.

`config propose` writes no file. Only `config write` does, and it keeps a backup
of what was there.

Rule ids that begin with `docker-maid.configure/` belong to that flow. Never
write one by hand; use your own prefix.

## The schema

Every table is optional. Every unknown key anywhere is a hard error, because
the schema is strict on purpose: a typo that was accepted and ignored would
look exactly like a rule that does not work.

A key this build has retired fails at parse with a note naming it and telling
you what to delete. Follow the note; do not try to make the key work.

Four rule kinds plus build cache:

```toml
[defaults]
interval = "5m"

[protect]
names = ["^postgres-prod$"]
labels = ["com.example.keep=true"]

[[rules.containers]]
id = "manual/agent-sandboxes"
name = "agent-sandboxes"
stopped_ttl = "2h"
select.labels = ["ai-agent.owner=my-agent"]

[[rules.volumes]]
id = "manual/agent-volumes"
name = "agent-volumes"
orphan_for = "24h"
select.labels = ["ai-agent.owner=my-agent"]

[[rules.networks]]
id = "manual/agent-networks"
name = "agent-networks"
orphan = true
orphan_for = "2h"
select.labels = ["ai-agent.owner=my-agent"]

[[rules.images]]
id = "manual/agent-builds"
name = "agent-builds"
unused_for = "72h"
image_tag_patterns = ["localhost/agent-*"]
select.labels = ["ai-agent.owner=my-agent"]

[tui]
refresh = "5m"
```

Durations are written the way a person says them: `30s`, `5m`, `2h`, `30d`.
Zero is refused.

## The traps

**`select` is mandatory.** Every container, image, volume, and network rule
needs at least one selector. A rule with none is refused, not treated as
"match everything".

**`scope` and `allow_unscoped` are one switch, not two.** They must agree.
`scope = "all"` requires `allow_unscoped = true`, and `allow_unscoped = true`
requires `scope = "all"`. Setting either alone is a validation error. Together
they mean: act on resources that carry no ownership evidence. That is the
dangerous mode, so it is deliberately awkward to reach.

```toml
[[rules.containers]]
id = "manual/every-stopped-sandbox"
name = "every-stopped-sandbox"
stopped_ttl = "24h"
scope = "all"
allow_unscoped = true
select.names = ["^sandbox-"]
```

**A network rule without `orphan = true` removes nothing.** It parses, it
validates, and it never selects anything.

**Build cache is always unscoped** and needs a floor. Docker build-cache
records carry no ownership metadata at all, so `allow_unscoped = true` is
required, and so is at least one of `older_than` or `max_bytes`:

```toml
[rules.build_cache]
id = "manual/build-cache"
older_than = "30d"
max_bytes = 21474836480
allow_unscoped = true
```

**Rule `id` is unique across every rule kind**, not just within one kind. A
duplicate is a validation error naming the id.

## Selectors match three different ways

Getting this wrong produces a rule that parses, validates, and silently matches
nothing. There is no warning for a selector that never fires.

| Field | Syntax |
|---|---|
| `select.labels` | glob |
| `select.names` | regex |
| `select.name_parts` | plain substring |
| `protect.names` | regex |
| `protect.labels` | glob |
| `image_tag_patterns` | glob |

A label glob is matched against the bare key and against `key=value`, so
`ai-agent.owner=my-agent` pins one agent and `ai-agent.*` covers the namespace.

Run `docker_maid labels` for the keys this build treats as ownership evidence.
A key it does not advertise is not ownership evidence, however convincing it
looks.

## Prove it

A policy you have not checked against real resources is a guess.

```sh
docker_maid config check --config ./docker_maid.toml
docker_maid --json plan
```

Read `items[]` from the plan document. Each item carries a `disposition`, and a
`matched_rule` when a rule claimed it. `matched_rule` holds the rule's `name`,
not its `id`. Confirm two things:

1. The resources you meant to cover are `owned` and name your rule.
2. Nothing else moved. Count the items carrying a `matched_rule` and check that
   the count is the one you expected. A rule that pulls in a resource you did
   not intend is the failure mode that matters here.

Exit `1` from a dry run means pending removals were found. That is information,
not failure.

**Check which daemon you are talking to before you believe the answer.** A host
can run more than one. If the `docker` CLI uses a context and `DOCKER_HOST` is
unset, the two clients look at different daemons, and a correct rule reports
nothing because your resources are not on the daemon being inventoried.

## Why the first pass removes nothing

Volume, image, and network age floors measure **continuous observed-unreferenced
time**, not a Docker timestamp. The clock starts on the first pass that sees the
resource unreferenced, and it is recorded under
`$XDG_STATE_HOME/docker_maid/observation.toml`.

So a brand new policy reports age `0` for everything and keeps it all. An
unmeasured age never satisfies a floor.

Do not treat this as a broken rule and "fix" a working configuration. Wait for
the floor, or lower it for a test and put it back.

Container `stopped_ttl` and `running_ttl` are different: they read Docker's own
state timestamps and work on the first pass.

## Protection is separate and machine-owned

```sh
docker_maid protect container my-important-container
docker_maid protect label owner=platform-team
docker_maid unprotect container my-important-container
```

Protection is typed runtime state written under a lock, stored apart from the
configuration file. Never hand-edit that state, and never encode a protection
decision as a rule. Protection always wins over a rule.

`protect.names` and `protect.labels` in the configuration are the operator's
own standing list. They are the human half; leave them to the human.

## Rules to follow

1. Never write a policy the operator has not seen. Propose, show, wait.
2. Never widen `scope` to `all` to make a rule fire. Fix the selector instead.
3. Check every file you write, then prove it against a real plan.
4. Read `--json`; do not scrape the human tables.
5. If a rule matches nothing, suspect the selector syntax before the floor, and
   suspect the floor before the tool.
