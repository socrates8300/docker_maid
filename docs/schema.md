# docker_maid machine schema

This document defines machine output schema version `1`.

Use `--format json` or its `--json` alias on any non-interactive command. One-shot commands write exactly one JSON document to stdout. `daemon` writes one JSON object per line (NDJSON). Every document and event contains:

```json
{"schema_version":1}
```

Schema version 1 is additive-only. Existing fields keep their meaning and type. New optional fields can be added without changing `schema_version`.

JSON mode does not emit ANSI escapes, progress bars, or prompts. A fatal error leaves stdout empty and writes one error document to stderr. Warnings are one JSON document per stderr line.

## Global error and warning documents

A fatal error has this shape:

```json
{
  "schema_version": 1,
  "error": {
    "kind": "docker_unreachable",
    "message": "cannot connect to Docker",
    "details": []
  }
}
```

Stable error kinds are:

| Kind | Exit code | Meaning |
|---|---:|---|
| `config_invalid` | `3` | Configuration is missing, unreadable, or invalid |
| `docker_unreachable` | `5` | Docker is unavailable or incompatible |
| `state_io` | `6` | Protection or activity state cannot be used safely |
| `partial_failure` | `2` | An applied pass skipped or failed at least one target |
| `internal` | `7` | Output or an internal invariant failed |
| `usage` | `64` | Command-line invocation is invalid |

An applied partial pass writes its complete result document to stdout and a `partial_failure` error document to stderr.

A warning has this shape:

```json
{
  "schema_version": 1,
  "warning": {
    "kind": "authorized_unscoped",
    "message": "build-cache policy is authorized-unscoped"
  }
}
```

## Plan and clean documents

`plan`, dry-run `clean`, and `clean --apply` use one common document:

```json
{
  "schema_version": 1,
  "command": "clean",
  "applied": true,
  "pending_removals": 1,
  "inventory": {
    "total": 4,
    "pending_removals": 1,
    "by_resource_kind": {"network": 4},
    "by_disposition": {"owned": 1, "unowned": 3}
  },
  "items": [
    {
      "resource_kind": "network",
      "id": "full-daemon-id",
      "name": "agent-network",
      "state": "available",
      "disposition": "owned",
      "action": "remove",
      "matched_rule": "agent-networks",
      "age_seconds": 120,
      "size_bytes": null,
      "reason": "matched orphan network rule"
    }
  ],
  "result": {
    "removed": 1,
    "skipped": 0,
    "failed": 0,
    "outcomes": [
      {
        "resource_kind": "network",
        "id": "full-daemon-id",
        "name": "agent-network",
        "matched_rule": "agent-networks",
        "status": "removed",
        "detail": "removed network"
      }
    ]
  }
}
```

`items` contains the complete inventory, not only removals. `result` is present only when `--apply` was used. Exit `1` means a dry run found pending removals. Exit `2` means an applied pass contains a skipped or failed outcome.

## Status document

`status` returns the selected configuration summary, complete inventory and dispositions, known Docker bytes, runtime protection count, rule-health baselines, and the last completed activity pass:

```json
{
  "schema_version": 1,
  "command": "status",
  "configuration": {
    "path": "/absolute/path/docker_maid.toml",
    "hash": "stable-config-hash",
    "interval": "5m",
    "rules": {"containers": 1, "images": 0, "volumes": 0, "networks": 0, "build_cache": 0},
    "configured_protection": {"names": 0, "labels": 0}
  },
  "inventory": {},
  "items": [],
  "disk_usage": {"known_bytes": 0, "unknown_size_items": 0, "by_resource_kind": {}},
  "runtime_protection_entries": 0,
  "rule_health": [],
  "last_completed_pass": null
}
```

A rule is `regressed` only when its previous completed pass match count was non-zero and its current match count is zero.

## Protection and configuration documents

`protect` and `unprotect` return `command`, `resource_kind`, `changed`, and `total_runtime_protection_entries`.

`config default`, `config check`, and `config print` return `command`, `path`, and the parsed `configuration` object. `path` is `null` for the built-in default.

`--version --format json` returns `command: "version"` and `version`.

## Daemon NDJSON

`daemon --format json` writes these event types to stdout:

| Event | Purpose |
|---|---|
| `daemon_started` | Process mode and interval |
| `pass_started` | Pass number, trigger, and mode |
| `plan` | Complete inventory and policy result for one pass |
| `action` | One applied target outcome |
| `pass_summary` | Pending and execution totals |
| `pass_error` | Recoverable pass failure; the daemon will retry on cadence |
| `configuration_reload_requested` | SIGHUP requested a reload |
| `daemon_stopped` | Graceful SIGTERM or SIGINT completion |

Every event contains `schema_version`, `event`, and Unix `timestamp`. Pass events also contain `pass_number`. A recoverable `pass_error` embeds the same `kind`, `message`, and `details` fields as a fatal error, but remains on the daemon stdout stream because the process continues.

Consumers must parse each line independently. They must ignore unknown fields to remain compatible with additive schema changes.
