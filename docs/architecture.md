# Architecture

One process, plain OS threads, no async runtime. Each thread owns one job and shares state through `Arc`s.

```
            Docker socket (GET only)
                 │  list, inspect, events
         ┌───────▼────────┐
         │ registry       │  running containers: name, labels, pid, cgroup dir, log path
         │ docker-events  │  start/die/rename → resync
         └───┬────────┬───┘
             │        │
   ┌─────────▼──┐  ┌──▼──────────┐  json-file tail ─┐
   │ collect    │  │ logs        │◄ logs-api-* ─────┤ partial join → multi-line → batch → gzip → sink
   │ every 15 s │  │ every 1 s   │  (local/journald)┘ cursors saved after each delivered batch
   └─────┬──────┘  └─────────────┘
         │ Snapshot { host, containers, MetricSet }
   ┌─────▼──────────────┐   ┌──────────────┐   ┌─────┐
   │ http-0..3 workers  │   │ remote-write │   │ gpu │  nvidia-smi --loop-ms children: GPUs, GPU processes
   │ /metrics /api/v1/* │   │ bounded queue│   └─────┘
   └────────────────────┘   └──────────────┘
   main thread: sigwait(SIGTERM) → flush logs and queue → exit
```

## Modules

| Path | Job |
|---|---|
| `src/main.rs` | Flags, config, thread startup, the collection loop |
| `src/config.rs` | TOML plus `COLLECTOR_*` env overrides, defaults |
| `src/metrics.rs` | `MetricSet`: families, samples, shared label sets, Prometheus text rendering |
| `src/host/procfs.rs` | Pure parsers for `/proc/stat`, `loadavg`, `meminfo`, `diskstats`, `net/dev`, mounts |
| `src/host/deadline.rs` | Runs `statfs` calls on threads with a deadline; skips keys still stuck from an earlier round |
| `src/host/hwmon.rs` | hwmon sensors with node_exporter's chip naming |
| `src/host/gpu.rs` | `nvidia-smi` CSV parsers and the long-running GPU and GPU-process watchers |
| `src/host/mod.rs` | Host collection, rates, node_exporter metric names |
| `src/containers/docker.rs` | Minimal HTTP/1.1 client over the Unix socket (chunked decoding, events stream) |
| `src/containers/cgroup.rs` | Pure parsers for cgroup v2 files, cgroup dir discovery and container ids in `/proc/<pid>/cgroup` |
| `src/containers/mod.rs` | Registry, container collection, GPU processes mapped to containers, cAdvisor metric names |
| `src/logs/cursor.rs` | Rotation-aware reader over `<id>-json.log`, `.1`, `.2` …, with per-line positions; cursor state file |
| `src/logs/assemble.rs` | Joins 16 KB partial pieces and groups multi-line records; reports the oldest held-back position |
| `src/logs/mod.rs` | Follow set, logs API followers, batching, retry, commit and replay, shutdown flush |
| `src/logs/sink.rs` | VictoriaLogs `/insert/jsonline` and file sinks |
| `src/remote_write.rs` | Hand-written protobuf `WriteRequest` encoder, snappy, push |
| `src/api.rs` | HTTP routes |

## Choices

- **Collect on a timer, serve from the snapshot.** Scrapes and API calls never touch `/proc` or cgroups. `/metrics` streams the text in 16 KiB pieces, so a scrape never holds the whole exposition in memory.
- **Docker API for metadata only.** Stats come from cgroup files, not `/containers/{id}/stats`, which costs a stream per container.
- **Logs from files, not the API.** The json-file log on disk gives byte offsets for exact resume and doubles as the backpressure buffer.
- **One batch in flight.** The log thread blocks on a failing sink rather than buffering; memory is bounded by `logs.batch_bytes`.
- **Commit and replay.** A held-back record (a partial line or an open multi-line group) can span batches. The saved cursor points at its start and also records how far reading got (`replay_until`). After a crash the reader goes back to the start, rebuilds the held-back record and drops anything it emits before `replay_until`, since that was already delivered.
- **Logs API only where needed.** Files give exact byte offsets and cost nothing between polls; `local` and `journald` have no readable files, so those containers get one streaming API connection each, resumed by nanosecond timestamp.
- **Few dependencies.** serde, toml, tiny_http, ureq (rustls), snap, libc, regex-lite, jemalloc. No tokio, no protobuf codegen, no Docker SDK.
- **jemalloc.** With musl's allocator, RSS climbed to 19 MiB over a flat 1.3 MiB heap on a 100-container host; jemalloc holds it near 3 MiB at the same CPU.
