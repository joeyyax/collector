# Architecture

One process, plain OS threads, no async runtime. Each thread owns one job and shares state through `Arc`s.

```
            Docker socket (GET only)
                 │  list, inspect, events
         ┌───────▼────────┐
         │ registry       │  running containers: name, labels, pid, cgroup dir, log path
         │ docker-events  │  start/die/oom/health → lifecycle, re-inspect
         └───┬────────┬───┘
             │        │
   ┌─────────▼──┐  ┌──▼──────────┐
   │ collect    │  │ logs        │◄ json-file handles → partial join → batch per tenant → gzip → sink
   │ 15 s, 5 s  │  │ every 1 s   │  a tenant's cursors saved after its batch is delivered
   │ if streamed│  └─────────────┘
   └─────┬──────┘   layer-size: one writable-layer walk at a time (optional)
         │ Snapshot { host, containers, MetricSet }
   ┌─────▼──────────────┐   ┌──────────────┐   ┌─────┐
   │ http-0..3 workers  │   │ remote-write │   │ gpu │  nvidia-smi --loop-ms children: GPUs, GPU processes
   │ /metrics /api/v1/* │   │ bounded queue│   └─────┘
   │ stream-* per SSE   │   └──────────────┘
   └────────────────────┘
   main thread: sigwait(SIGTERM) → flush logs and queue → exit
```

## Modules

| Path | Job |
|---|---|
| `src/main.rs` | Flags, config, thread startup, the collection loop |
| `src/cli.rs` | `health`, `host`, `containers` and `gpus` subcommands: one API call, printed as a table |
| `src/state.rs` | The latest snapshot, remote-write counters and stream subscribers |
| `src/config.rs` | TOML plus `COLLECTOR_*` env overrides, defaults |
| `src/metrics.rs` | `MetricSet`: families, samples, shared label sets in clean and cAdvisor form, Prometheus text rendering |
| `src/host/procfs.rs` | Pure parsers for `/proc/stat`, `loadavg`, `meminfo`, `diskstats`, `net/dev`, mounts, `vmstat`, PSI, `file-nr`, conntrack |
| `src/host/deadline.rs` | Runs `statfs` calls on threads with a deadline; skips keys still stuck from an earlier round |
| `src/host/hwmon.rs` | hwmon sensors with node_exporter's chip naming |
| `src/host/gpu.rs` | `nvidia-smi` CSV parsers and the long-running GPU and GPU-process watchers |
| `src/host/mod.rs` | Host collection, rates, node_exporter metric names |
| `src/containers/docker.rs` | Minimal HTTP/1.1 client over the Unix socket (chunked decoding, events stream) |
| `src/containers/cgroup.rs` | Pure parsers for cgroup v2 files, cgroup dir discovery and container ids in `/proc/<pid>/cgroup` |
| `src/containers/mod.rs` | Registry, container collection, GPU processes mapped to containers, container metric names |
| `src/containers/lifecycle.rs` | Exits, OOM kills and recently stopped containers from Docker events and inspect |
| `src/containers/layer.rs` | Writable-layer walks, one container at a time |
| `src/logs/cursor.rs` | Reader over open handles to `<id>-json.log`, `.1`, `.2` …, with per-line positions; cursor state file |
| `src/logs/assemble.rs` | Joins 16 KB partial pieces; reports the oldest held-back position |
| `src/logs/mod.rs` | Follow set, per-tenant batching and retry, commits, shutdown flush |
| `src/logs/sink.rs` | VictoriaLogs `/insert/jsonline` and file sinks |
| `src/remote_write.rs` | Hand-written protobuf `WriteRequest` encoder, snappy, push |
| `src/api.rs` | HTTP routes, sort and limit, the event stream |

## Choices

- **Collect on a timer, serve from the snapshot.** Scrapes and API calls never touch `/proc` or cgroups. `/metrics` streams the text in 16 KiB pieces, so a scrape never holds the whole exposition in memory. A stream subscriber shortens the timer to `live_interval` until it leaves.
- **No history.** Only the latest snapshot is kept; storing readings over time is the consumer's job.
- **Names at render time.** Samples are stored once with both label forms; `/metrics` and remote write pick clean or cAdvisor names as they render.
- **Docker API for metadata only.** Stats come from cgroup files, not `/containers/{id}/stats`, which costs a stream per container.
- **json-file only.** The log on disk gives byte offsets for exact resume and doubles as the backpressure buffer. Other drivers would need a streaming API connection per container and a timestamp resume, so they aren't read.
- **Handles, not paths.** Each container keeps its files open from the cursor's onward and labels lines with the inode from `fstat`, so a rotation can't mislabel or skip bytes. A poll costs one `stat` of the current path; the rotations are rescanned, newest name first, only when its inode changes. The cursor's file stays open after Docker deletes it, one file per container at most.
- **One batch in flight per tenant.** A failing tenant keeps its batch and retries it with backoff; its containers aren't read meanwhile and other tenants carry on. Memory is bounded by one batch per tenant (`logs.batch_bytes` plus one read of `logs.read_bytes`) and, per container, 1 MiB held behind an unfinished partial line, for at most `logs.partial_timeout`.
- **Commit at the oldest held record.** While any partial line is unfinished, every later record of that container is held too, so everything before the oldest held start was delivered and everything after it wasn't. The saved cursor is that start; a restart rebuilds the held records without repeating any. After `logs.partial_timeout` everything held is released and the line's remaining pieces start a new record, so bytes still go out once, split.
- **At least once.** Cursors are saved with fsync after the sink's 2xx, so a crash between the two re-sends one batch per tenant.
- **Few dependencies.** serde, toml, tiny_http, ureq (rustls), snap, libc, regex-lite, jemalloc. No tokio, no protobuf codegen, no Docker SDK.
- **jemalloc.** With musl's allocator, RSS climbed to 19 MiB over a flat 1.3 MiB heap on a 100-container host; jemalloc holds it near 3 MiB at the same CPU.
