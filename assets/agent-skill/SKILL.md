---
name: docker-maid
description: "Create Docker sandboxes that can later be reclaimed, and clear the ones earlier runs abandoned. Use whenever you create a container, image, volume, or network on a shared Docker host, and whenever a build fails on disk space or on the network address pool."
---

# docker_maid for coding agents

`docker_maid` finds and removes the Docker resources that agent workflows
abandon: stopped sandboxes, throwaway build layers, orphaned volumes, and
leaked bridge networks.

Drive the CLI. Do not reimplement any part of it, and do not wrap it in your
own sandbox launcher.

Add `--json` to any non-interactive command. It prints one schema-versioned
document, never prompts, and never emits colour or progress bars. Parse that,
not the human tables.

## Mark what you create

Docker fixes labels when a resource is created. Nothing can relabel a
container, image, volume, or network afterwards, so apply the ownership stamp
at the moment you create it or the resource stays anonymous forever.

Let `docker_maid` create the sandbox, which applies the stamp for you:

```sh
docker_maid spawn --image my-sandbox:latest --owner my-agent \
  --workspace "$PWD" -- npm test
```

The sandbox is detached and is never removed automatically. It outlives the
command, nothing is attached to it, and nothing supervises it. Stop and remove
it yourself when you are done, or leave it for a cleanup rule.

`spawn` covers an image, a name, a workspace, a working directory, and a
command. For anything else, create the resource yourself and interpolate the
stamp:

```sh
docker run -d --network mynet -e KEY=value \
  $(docker_maid stamp --owner my-agent --docker-args) my-sandbox:latest
docker volume create $(docker_maid stamp --owner my-agent --docker-args) my-vol
docker network create $(docker_maid stamp --owner my-agent --docker-args) my-net
```

`docker_maid --json stamp --owner my-agent` returns the same labels as a map if
you are calling the Docker API rather than the CLI.

`--owner` takes letters, digits, dot, dash, and underscore. Anything else is
refused rather than quoted.

## Understand what is already there

```sh
docker_maid --json labels     # the label keys this build treats as ownership
docker_maid --json status     # inventory, dispositions, disk, last pass
docker_maid --json plan       # what the current policy would remove
```

`labels` needs no daemon and no configuration. It is the only list that counts:
a key it does not advertise is not ownership evidence, however convincing it
looks.

## Clean up

```sh
docker_maid --json clean            # dry run; changes nothing
docker_maid --json clean --apply    # remove what the plan selected
```

Nothing is removed unless a rule in the configuration selects it. `clean`
without `--apply` never mutates Docker. Protection always wins over a rule.

Protect something that must survive a pass:

```sh
docker_maid protect container my-important-container
docker_maid protect label owner=platform-team
docker_maid unprotect container my-important-container
```

Protection you add this way is machine-managed runtime state. It is stored
separately from the human-owned configuration file, so it never edits an
operator's policy.

## Turn what you created into policy

If your resources carry the stamp, the maid can already see them:

```sh
docker_maid --json config survey                      # families it could adopt
docker_maid --json config propose --candidate <ID>    # reviewable, writes nothing
docker_maid --json config write --proposal <FILE>     # applies the reviewed proposal
```

Never hand-edit the configuration file on an operator's behalf. Propose, show
the proposal, and let a human decide.

## Exit codes

| Code | Meaning |
|---:|---|
| 0 | Success |
| 1 | Dry run found resources it would remove |
| 2 | An applied pass skipped or failed at least one target |
| 3 | Configuration missing, unreadable, or invalid |
| 4 | The interactive dashboard needs a terminal |
| 5 | Docker is unavailable or refused the request |
| 6 | Protection, observation, or activity state cannot be used safely |
| 7 | Output or an internal invariant failed |
| 64 | The command line was invalid |

Exit `1` is information, not failure: a dry run found pending removals. Treat
exit `2` as a real problem and read the outcomes in the result document.

## Rules to follow

1. Stamp at creation. There is no second chance.
2. Never run `docker system prune`. It is blind to ownership and will take
   resources that belong to someone else on the same host.
3. Never remove a resource you did not create unless a rule and a human say so.
4. Read `--json`; do not scrape the human tables.
5. Report what you removed. `clean --apply --json` lists every outcome.
