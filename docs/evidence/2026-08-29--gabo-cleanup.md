# Gabo cleanup + durable policy — 2026-08-29

Mirror of the Boltero runbook, tailored: walden-centric protects, 12h defaults,
launchd daemon. Raw captures: `2026-08-29--gabo-debris-data/`.

## Before / after

| Metric | Before | After |
|---|---|---|
| Volumes | 1886 / 88.7 GB (99% reclaimable) | **11 / 2.7 GB** |
| Images | 45 / 30.2 GB (94% reclaimable) | **8 / 2.6 GB** |
| Containers | 13 (1 running) | **4** (1 running + 3 kept dev) |
| Build cache | 3.5 GB | 2.0 GB (standing rule holds 24 h/10 GiB) |

Total reclaimed ≈ **115 GB** inside the Docker Desktop VM.

## Gabo profile (why it differs from Boltero)

- Active development lives in `~/dev/walden/`. Everything else on the box is
  dev scratch from earlier rounds (jetvision, k2*, blind*, wt-*, openmemory,
  personalfinance, mailhog, six postgres test containers).
- `spanishnumbers-web` (`numeros-vivos`, manual `docker run -p 80:80`) is the
  one persistent app. No compose labels; protected by name.
- `imbue_control_plane_*` (1.8 GB) is the only non-junk big volume per
  operator decision — protected by name.
- `walden_management_walden_{minio,pg,redis}` + the stopped
  `walden_minio/walden_postgres/walden_redis` containers kept for dev.
- 912 anonymous hex volumes (49.4 GB) — agent test-run leftovers; swept.
- Docker Desktop data folder on `/Volumes/Everything/jamesr/dockerDesktop/`;
  orphan Docker.raw there will be reclaimed at Desktop removal.

## Standing config (gabo, `configs/gabo/standing.toml`)

Walder-centric, not a Boltero copy: stopped containers 12 h, unreferenced
images 12 h, unreferenced volumes 12 h, build cache 24 h/10 GiB, everything
else protected by an explicit name list.

## Operations notes (recurring-host gotchas)

1. **Two-pass sweep pattern** — day-2026 observation floors (first pass of a
   new config starts clocks; a 1 h floor pass second run catches images+volumes;
   a 12 h standing floor then governs). Multi-pass revalidation is designed
   and safe.
2. **Concurrent sweeps are safe but slow** — tree `docker_maid clean` instances
   (after an ssh timeout left one alive, a nohup added a second; a third
   appeared) each revalidate per target; overlapping deletes skip. Volume
   chains 1886→11 completed clean. Prefer one nohup'd instance.
3. **macOS 26 screens `~/.local/bin/docker_maid`** — the underscore-named file
   under this specific dir got SIGKILL(137) on exec while an identical copy at
   `/tmp/dm` and a hyphen-named `~/.local/bin/docker-maid` run fine. Workaround:
   deploy as `docker-maid` (hyphen). Future binary updates: scp to /tmp, then
   `cp /tmp/... ~/.local/bin/docker-maid`.
4. **No daemon existed before** — docker_maid was installed but never
   configured (exit 3, no state). The August gap recurred here; the launchd
   agent now closes it (io.walden.docker-maid, RunAtLoad, KeepAlive,
   `--apply`).
5. **Daemon the current build** shipped only after the sweep; the running
   instance already loaded the one in-use inode — replacement via the same
   inode-safe path.

## Next (pending operator go)

- OrbStack swap on gabo: no Homebrew on the box → decide brew-first vs
  direct OrbStack dmg; migrate with `orbctl docker migrate` after, then
  `sudo ln -sfn` the socket; remove Docker Desktop + `/Volumes/Everything`
  Docker.raw (~majority reclaim on everything volume).
- `orbctl docker migrate` will not move the network case — walden images
  compose; recreate with labels, then start.