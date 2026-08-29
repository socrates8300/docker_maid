---
name: docker-maid-repo
description: "Set per-repository Docker cleanup defaults for docker_maid: 12h image and volume floors tuned to the repo, with an explicit keep moment for volume data. Use at the start of Docker work in a repository so build images and test volumes never accumulate. The companion docker-maid-config skill covers the policy file schema itself."
---

# docker_maid repo defaults

A repository gets three defaults. One operator decision does the only-copy
check. An agent runs this ritual once per repository, at the start of Docker
work.

## The defaults

1. **Build images die at 12h.** Every image the repo builds, tagged with the
   repo's naming convention, is unreferenced-removed 12h after nothing uses
   it.
2. **Test volumes die at 12h.** Volumes the repo creates for tests and
   scratch work are unreferenced-removed 12h after the last container
   stops.
3. **Anything that persists is kept on purpose.** A volume that holds data is
   protected or cloned before any floor can touch it. The keep step below is
   where the agent says "we need these volumes" — and where a wrong answer
   costs data, so it happens before write, not after.

Everything reusable is on the registry: images re-pull in minutes, build
cache rebuilds. Disk is not infinite; a clean disk beats a warm disk.

## The keep step (do this first)

List every volume this repository creates and currently references:

```sh
docker compose ps 2>/dev/null || true
docker volume ls --format '{{.Name}}' |
  xargs -I{} docker volume inspect --format '{{.Name}} {{.Labels.com.docker.compose.project}}' {}
```

For each volume: is it the only copy of data with no backup? If yes, it gets
a `[protect]` entry. If it is scratch, it matches a removal pattern. Never
skip this step: an unexamined volume is later deleted at 12h, and a deleted
only-copy database is not a bug report, it is a data loss.

## Propose, write, prove

The configurator does the file work; this skill names what to propose.

```sh
docker_maid config survey         # what the repo already creates
docker_maid config propose        # a compare-and-swap policy to review
docker_maid config write           # apply the proposal
```

Defaults for a repo look like:

```toml
[protect]
names = [".*persistent-.*"]

[[rules.images]]
id = "docker-maid.configure/repo-images"
name = "repo-images"
scope = "all"
allow_unscoped = true
unused_for = "12h"
image_tag_patterns = ["yourrepo/*", "localhost/*"]

[rules.images.select]
names = [".+"]

[[rules.volumes]]
id = "docker-maid.configure/repo-volumes"
name = "repo-volumes"
scope = "all"
allow_unscoped = true
orphan_for = "12h"

[rules.volumes.select]
names = ["^(scratch|test)-.*", "^ci-junk-.*$"]
```

Leave the operator's manual rules untouched. Only the configurator-managed
region carries the repo defaults. Durations are the way a person says them:
`1h`, `2h` — zero is refused.

## When agents create Docker resources

Prefer `docker run` with the repo's stamps over raw creation:

```sh
docker_maid stamp --docker-args --owner "$REPO_NAME"
docker run --rm $(docker_maid stamp --docker-args --owner "$REPO_NAME") ...
```

The stamp is a label, fixed at creation. A resource created unstamped stays
unowned and is not reclaimable by the repo defaults.

## This repo is not a host operator

The config file with defaults belongs to the repo operator; the agent runs
the ritual, shows the proposal, and waits. Never hand-edit an operator's
existing configuration to remove a [protect] entry.