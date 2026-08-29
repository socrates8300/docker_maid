# Docker debris capture — 2026-08-29 (pre-OrbStack, pre-cleanup)

Evidence for the durable-policy work. Captured before the one-time cleanup pass
destroys the data. Raw files live in `2026-08-29--debris-data/` next to this
document. Everything here was read-only.

## Why this exists

The Docker Desktop VM holds ~140 GB of real data in a 494 GB sparse
`Docker.raw`. We plan to migrate to OrbStack and adopt a standing cleanup
policy. Policies written from memory after deletion will target patterns
nobody can verify. This capture fixes the patterns first.

## Machine state

| Fact | Value |
|---|---|
| Engine | Docker Desktop, moby 29.6.2, containerd image store, overlayfs |
| Data file | `/Volumes/BugsBunny/jamesr/DockerDesktop/DockerDesktop/Docker.raw` (custom `DataFolder`, set in settings-store.json) |
| Sparse file allocated | 494.4 GB |
| Real content in VM | ~140 GB (`/var/lib/desktop-containerd` 127 GB + `/var/lib/docker` 8.9 GB) |
| Host free space | internal `/Users`: 72 GiB; BugsBunny: 1.5 TiB |
| CLI skew | brew `docker` 29.7.2 vs server 29.6.2 (works; note for migration) |
| docker_maid daemon | not running; last applied pass 2026-08-18 |

The 494 GB allocation is sparse-file bloat: Docker Desktop never shrinks
`Docker.raw`. Deletion inside the VM frees bytes for the VM, not for the host.
Host-level reclaim happens only when this file is deleted (planned with the
Docker Desktop removal). OrbStack's disk image claims to return freed space;
verify after migration.

## Debris taxonomy (the patterns policies must name)

### 1. Per-commit pipeline images — biggest single family

- `sddv4-predeploy-tests`: 40 images, 36.5 GB unique, 9–14 h old.
- `sddv4-predeploy`: 42 images, 0.3 GB unique, same cadence.
- One image set per commit tag (`0cc7fec`, `5095a24`, …). Obsolete by design
  once the commit is judged.
- 18 `<none>` (dangling) images, 35.1 GB virtual / ~3 GB unique — abandoned
  build residue, mixed ages to 2026-08-27.
- `sddv4-xbuild`: 1 image, 3.2 GB, 2026-08-27.
- Carries no ownership stamp. The standing config deliberately never selects
  unstamped resources, so these accumulate forever unless a reviewed pass or a
  new policy intervenes.

Policy implication (DECIDED 2026-08-29): stamp at build time
(`docker build --label dev.docker-maid.managed=true --label
dev.docker-maid.owner=sddv4`) and let an owned-scope image rule with an
`unused_for` floor of **12 h** reclaim them. James set 12 h as the default
floor for all ephemeral build images across every repo; a repo that needs
longer overrides it explicitly. This is the highest-value rule in the set:
36.5 GB churns per working day.

### 2. Build cache — 456 entries, 60.1 GB

Age distribution (from `buildx du --verbose`):

| Age | Entries | GB |
|---|---|---|
| 0–1 d | 93 | 24.1 |
| 1–3 d | 187 | 17.6 |
| 3–7 d | 0 | 0 |
| 7–30 d | 109 | 3.3 |
| >30 d | 67 | 15.0 |

Readings:

- A single heavy build day (~2 GB/run) outruns any age floor. The
  `max_bytes` cap, oldest-first, is what bounds the cache; the age floor only
  evicts cold entries.
- 15 GB is older than 30 days — dead weight a running daemon would have taken
  months ago. The policy was never the problem; it never ran.
- Types: 439 regular, 16 source.local, 1 frontend.
- Standing config today: `older_than = 7d`, `max_bytes = 10 GiB`.
  DECIDED 2026-08-29: `older_than = "24h"`, `max_bytes` 8–10 GiB. The cap
  bounds load; the 24 h floor evicts cold entries. Expected steady state ≈
  cap under heavy build load, ≪ 4 GB quiet days.

### 3. Long-tail registry images — ~20 GB, re-pullable

`rust` 6.8 GB (4 tags), `node` 6.5 GB, `postgres` 2.6, `wordpress` 2.2,
`mysql` 1.1, `mailhog` 0.6, `busybox`/`alpine`/`ubuntu`/`debian`/`redis`/
`minio`/`curl` small. Some pulled once months ago (`rust` newest 2025-12-08,
`mailhog` built 2020). Every one re-pulls from a registry in minutes.

Policy implication: candidates for an unscoped image rule with a long floor
(e.g. `unused_for = "30d"`), or for repeated reviewed passes. Unscoped rules
need `scope = "all"` + `allow_unscoped = true` and warn on every pass.

### 4. Local app images — rebuildable, not re-pullable

`crm-app` 0.4, `crm-whatsapp-worker` 1.5, `arlisfresh-web` 1.1,
`arlisarangocom-web` 1.1, `arlis-hero-fix-test` 1.1, `next-greenfield-app`
0.4, `reset-path-app` 0.4, `judge-sddv4`, `wordpress`-stack bits. Newest
2026-08-05; none referenced by a container. Rebuild costs a repo checkout +
build, not a pull.

Policy implication: same one-time pass as long-tail; durable handling should
come from build-time stamps in each project, not name-pattern rules.

### 5. Volumes — 103 total, 102 dangling, 6.06 GB

- 76 anonymous hash volumes (compose leftovers, mostly <50 MB).
- ~25 named LaunchKit-style databases: `*_starter-postgres-data` across
  `walden_*`, `fresh-*`, `descuentixco`, `founder-billing-ui`,
  `generated-app`, `marketingops`, `repo`, `reset_path`, `email_cert_*` ×3.
- `walden_management_{pgdata,redisdata,miniodata}`,
  `papasito-dev_{postgres,redis}_data`, `crm_whatsapp_session` (0.54 GB),
  `arlisarangocom_mysql-data` (0.22 GB).
- In use and protected: `openseo_data`, `devnginx` set.

Volume total is small; the risk here is only-copy data, not bytes.

Policy implication: durable rule should key on stamps applied by whoever
creates the volume (LaunchKit generator, compose files) plus the existing
stamped-volume rule. Name-pattern volume rules are the fallback; they need an
explicit protect list for any volume that is the only copy of real data.

### 6. Networks

8 unowned (compose/agent leftovers, 0 GB). Existing stamped rule
(`orphan_for 6h`) is adequate. Compose projects `devnginx` and `openseo` are
protected by label.

### 7. Containers

Only 2, both running and protected (`edge-angie`, `openseo`). No container
debris today; the existing 12 h stopped-TTL stamped rule is adequate.

## docker_maid state at capture time

- Plan: 709 items; 151 pending (all build-cache); 0 images/volumes pending —
  the standing config never selects unstamped resources. Working as written.
- Observation clocks: 247 tracked (132 images, 102 volumes, 13 networks),
  oldest unreferenced clock 12.6 d. Floors set to ≤12 d bite immediately;
  anything new keeps until its first observation + floor.
- Protection: 5 runtime entries + config `[protect]` for `edge-angie`,
  `openseo`, both compose projects.
- Last applied pass 2026-08-18 (one container). The daemon has not been
  running since; that gap is the single biggest durable fix (launchd agent).

## What step 4 must deliver (draft, evidence-based)

1. **Standing config v2** — build cache 24 h floor + 10 GiB cap; owned image
   rule for stamped builds (**12 h** floor, James's default for all repos);
   volumes keyed on stamps with an explicit protect list for only-copy data.
   Open: unscoped registry-image rule (30 d floor, double opt-in) vs reviewed
   passes — see the pushback note below.

### Pushback note — "remove everything after 12 h if there is no policy"

A default that deletes *unclassified* resources on a clock is an unscoped
rule by another name. docker_maid's standing safety wall (double opt-in,
per-pass warning) exists because unclassified ≠ unowned: `openseo_data` on
this machine was an unstamped volume holding the only copy of a live
database, saved only because no unscoped rule existed to eat it. The
12 h default applies to *owned/stamped* resources — that tier is automatic.
For the unowned tier the proposal is: images only, 30 d floor, explicit
`scope = "all"` + `allow_unscoped = true`, never volumes. James decides; the
decision goes in the v2 config, not in memory.
2. **Stamp at birth** — sddv4 pre-deploy builds, LaunchKit generator, and any
   long-lived compose file apply `dev.docker-maid.managed=true` + owner label
   at creation. Debris becomes owned the moment it exists.
3. **Daemon under launchd** — `docker_maid daemon --apply` as a LaunchAgent;
   the 2026-08-18 → 2026-08-29 gap is the failure mode to kill.
4. **Repo verticals** — OrbStack section in README; daemon identity in
   `status`/machine output (split-brain guard for OrbStack vs Desktop vs
   colima); optionally launchd plist generation.
5. **Migration** — cleanup first, re-measure, then install OrbStack, import,
   verify, hand over the socket, remove Docker Desktop, delete `Docker.raw`.

## After-state (2026-08-29, post pass)

Five `clean --apply` runs of `one-time-pass-applied.toml` (floor 1 h; passes
2+ caught resources whose observation clocks elapsed between runs).

| Metric | Before | After |
|---|---|---|
| Inside-VM usage | ~140 GB (127 containerd + 8.9 docker) | **7.6 GB** (4.0 + 3.6) |
| Images | 135 (87 GB reported) | 2, both live, 0 reclaimable |
| Volumes | 103 (102 dangling) | 3, all protected/label-shielded |
| Build cache | 456 entries, 60.1 GB | 0 |
| `Docker.raw` allocated | 494.4 GB | 494.4 GB (sparse file never shrinks; dies with Docker Desktop removal) |

CLI exceptions used for residuals docker_maid's API path cannot reach, both
disclosed: `docker builder prune --all --force` (9.2 GB of buildkit-held
dedup-parent records; repeated passes skipped them with "Docker did not
delete the build-cache record"), and `docker rmi node:22.21.1
node:22.21.1-bookworm` (3.27 GB).

### New findings for repo verticals

1. **Multi-tag image deletion 409** — executor removal of an image referenced
   by two repositories fails with `409: conflict: unable to delete … (must be
   forced) - image is referenced in multiple repositories`. The plan records a
   skip (correct partial-failure behavior), but the executor should untag all
   repository references (or delete per-reference) before giving up. Repro:
   any image with two tags, `unused_for` expired.
2. **Buildkit dedup parents unreachable** — `exec_caches` prune via bollard
   cannot delete base-layer cache records ("pulled from …" entries); the CLI
   `builder prune --all` can. Standing-policy note: a 24 h/10 GiB policy will
   hold the line for fresh cache, but old dedup parents accumulate until a
   CLI prune — candidate for documentation or an executor `--all`-equivalent.
3. **Two starter volumes survive by runtime protection labels**
   (`com.docker.compose.project=reset_path`, `=walden_starter`) added in an
   earlier session — protection worked as designed against the unscoped
   sweep. They are 350 MB total; leave or release deliberately later.

## OrbStack migration record (2026-08-29, same day)

Sequence: one-time pass → `brew install --cask orbstack` (2.2.3) → `orbctl
start` → `orbctl docker migrate` (2 images, 3 volumes, 2 containers; it also
stopped Docker Desktop itself) → app-selection dialog (Docker only; no K8s,
no Linux machines) → context switch `desktop-linux` → `orbstack`, Docker
Desktop quit + removed (user data dirs, `Docker.raw`, app → Trash) → socket
handover.

Post-migration fixes, all recorded because they will recur on other hosts:

1. **Network not migrated** — `orbctl docker migrate` moves containers,
   volumes, images but not compose-created networks. `edge-angie` failed to
   start (`network devnginx_edge_net not found`). Fix: recreate the network
   with its compose labels (`com.docker.compose.project=devnginx`), then
   start. The label is load-bearing: it keeps the network in docker_maid's
   protected set.
2. **Startup-order DNS** — containers restored in arbitrary order; a proxy
   started before its upstream logs `[emerg] host not found in upstream`
   and 502s until restarted after the upstream is on the network. Not a
   data problem; restart the proxy once both are attached.
3. **Socket handover is manual** — OrbStack does not take
   `/var/run/docker.sock` while Docker Desktop's helper lives. After Desktop
   removal: `sudo ln -sfn ~/.orbstack/run/docker.sock /var/run/docker.sock`.
   docker_maid (bollard, default socket) reads OrbStack immediately; docker
   CLI needs `docker context use orbstack` (or the `~/.orbstack/bin` PATH
   shim, which was added to `~/.zshrc`).
4. **JetBrains symlink** — IDEs hardcode `/usr/local/bin/docker`; symlink it
   to `~/.orbstack/bin/docker`.
5. **Migration warning "graph driver not supported: skipping data"** —
   container writable-layer diffs are not copied, only images/volumes/config.
   Fine for these two containers (stateless proxies); a stateful container
   with non-volume writes would lose them. Record for future migrations:
   migrate data into volumes first.

## Verification after migration

- Default socket serves OrbStack engine 29.4.0 (linux/arm64, overlayfs);
  `docker version` with no env, no context reaches it.
- docker_maid default path: 12 items, 0 pending, 9 protected — plan and
  protection survived the engine switch; runtime protection labels intact.
- Live delete cycle on OrbStack: `docker_maid spawn` (stamped sandbox) →
  stop → `clean --apply` with a 1 s stopped-TTL owned rule → removed 1,
  failed 0. Spawn/inventory/plan/executor all confirmed on the new engine.
- `edge-angie` + `openseo` up; LAN ports bound; real vhost
  (seo.waldenpuddle.net via --resolve) serves 200 with content;
  `openseo_data` contents verified intact.
- Credentials: OrbStack's helper repointed
  `/usr/local/bin/docker-credential-osxkeychain` at its own binary; same
  keychain, `ghcr.io`/`ord.vultrcr.com` auth entries preserved.
- Disk: BugsBunny 1.5 → 1.9 TiB free (`Docker.raw` deleted); internal
  66 GiB free; OrbStack footprint ~7.6 GB of source data.
- `orbctl doctor` clean except one advisory (brew `docker-compose` shadows
  OrbStack's in default PATH; user shells have `~/.orbstack/bin` first, and
  compose is engine-agnostic — accepted).

## File index (`2026-08-29--debris-data/`)

| File | Contents |
|---|---|
| `summary.json` | Distilled dataset (this document in numbers) |
| `images.json` | All 135 images, one JSON object per line |
| `images-dangling-ids.txt` | Dangling image IDs |
| `containers.json`, `containers-inspect.json` | The 2 containers |
| `volumes.json`, `volumes-inspect.json` | All 103 volumes with created dates |
| `networks.json` | Networks |
| `buildx-du-verbose.txt` | All 456 build-cache records with dates |
| `system-df-v.txt` | Raw `docker system df -v` |
| `docker-version.json`, `docker-info.txt` | Daemon identity facts |
| `docker-raw-stat.json`, `host-df.txt` | Sparse file and host space facts |
| `docker_maid-{plan.json,status.txt,config.toml,protection.toml,observation.toml,activity.jsonl}` | docker_maid's own evidence at capture time |
