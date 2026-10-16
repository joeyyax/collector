# JSON API

Read-only, `GET` only, on `127.0.0.1:9273` unless `listen` says otherwise. With `api.token` or `api.token_file` set, every endpoint except `/health` needs `Authorization: Bearer <token>` and answers 401 without it. `/metrics` is gzipped when the client sends `Accept-Encoding: gzip`, as Prometheus and VictoriaMetrics do.

Every response reflects the last collection (every `interval`, 15 s by default); nothing is read from the host on request. `/health` and each container carry `collected_at`, the Unix time of that collection in seconds, and `interval_seconds`, the configured interval. Judge staleness against `interval_seconds` rather than a fixed 15 s. While a client follows `/api/v1/stream`, collection runs every `live_interval` (5 s) instead.

Rates (`*_per_second`, `usage_ratio`, `usage_cores`, `busy_ratio`) cover the time between the last two collections. They're `null` after the first collection and after a counter reset.

The collector keeps no history. Each response is the latest collection only.

| Route | Returns |
|---|---|
| `GET /health` | Freshness, version, Docker and log status; no token needed |
| `GET /api/v1/snapshot` | Host, GPUs, running and stopped containers from one collection |
| `GET /api/v1/stream` | The snapshot as server-sent events, once per collection |
| `GET /api/v1/host` | Host readings and GPUs |
| `GET /api/v1/containers` | Running containers |
| `GET /api/v1/containers/:id` | One running container |
| `GET /api/v1/gpus` | GPUs with the containers using each |
| `GET /metrics` | Prometheus text; see [metrics.md](metrics.md) |

## Query parameters

| Parameter | Routes | Values |
|---|---|---|
| `sort` | `/api/v1/containers`, `/api/v1/snapshot`, `/api/v1/stream` | `cpu`, `memory`, `net_rx`, `net_tx`, `disk_read`, `disk_write` or `name` (the default) |
| `limit` | same | Keep the first N containers after sorting |
| `compat` | `/metrics` | `cadvisor` renders cAdvisor's metric names and labels; `none` (the default) renders clean names |

Sorting is descending except for `name`. `cpu` is `usage_cores` and `memory` is `working_set_bytes`; the others sum the per-interface or per-device rates. Containers without a reading yet, such as after their first collection, and host-network containers sorted by network go last; ties go by name. An unknown value answers 400.

## `GET /health`

```json
{"status": "ok", "version": "0.3.0", "host": "web-1", "last_collect_age_seconds": 3.2, "collected_at": 1791656309.731, "interval_seconds": 15.0, "docker": true, "logs_last_success": 1791656289, "gpu_per_container": true}
```

`status` is `stale` with HTTP 503 when the last collection is older than three intervals plus 5 s. The slack keeps one slow collection, such as a `statfs` timeout, from flipping a short interval to stale. `last_collect_age_seconds` and `collected_at` are `null` before the first collection. `docker` is present when container metrics are on, `logs_last_success` (epoch seconds) when logs are on.

`gpu_per_container` is present when the host has a GPU. `true` means per-container GPU memory is read, so a container's empty `gpus` means it holds no GPU memory. `false` means it isn't read, usually because the collector runs without `pid: host`, so an empty `gpus` says nothing.

## `GET /api/v1/snapshot`

Everything from one collection in one response. 503 with `{"error": "no data yet"}` before the first collection.

```json
{
  "host": {"host": "web-1", "timestamp": 1791656309.7, "cpu": {"count": 32, "usage_ratio": 0.21}, "memory": {"total_bytes": 51539607552}},
  "gpus": [{"index": 0, "uuid": "00000000-0000-0000-0000-000000000000", "name": "NVIDIA GeForce RTX 3060", "utilization_ratio": 0.01, "containers": []}],
  "gpu_per_container": true,
  "containers": [{"id": "3f2a9c4e…", "name": "shop-redis", "cpu": {"usage_cores": 0.0096}}],
  "stopped": [{"id": "7b1c…", "name": "shop-worker", "lifecycle": {"state": "exited", "health": null, "restarts_total": 0, "last_exit": {"code": 137, "oom_killed": true, "exited_at": 1791656290.1}}, "stopped_at": 1791656290.1}],
  "collected_at": 1791656309.731,
  "interval_seconds": 15.0
}
```

Objects are abbreviated here. `host` has the shape of [`/api/v1/host`](#get-apiv1host) without `gpus`, and is `null` with `host_metrics.enabled = false`. `gpus` has the shape of [`/api/v1/gpus`](#get-apiv1gpus) and `containers` the shape of [`/api/v1/containers`](#get-apiv1containers), including `sort` and `limit`. `stopped` lists [stopped containers](#stopped-containers); `sort` and `limit` don't apply to it.

## `GET /api/v1/stream`

[Server-sent events](https://html.spec.whatwg.org/multipage/server-sent-events.html). The latest snapshot is sent on connect, then each new one as it's collected:

```
event: snapshot
data: {"host": {...}, "gpus": [...], "containers": [...], "stopped": [...], "collected_at": 1791656309.731, "interval_seconds": 15.0}

```

`data` is the [`/api/v1/snapshot`](#get-apiv1snapshot) response on one line, and `sort` and `limit` work the same way. While at least one client is connected, collection runs every `live_interval` (5 s) instead of `interval`; it returns to `interval` when the last one disconnects. GPU readings still refresh every `interval`. A `: keepalive` comment is sent after 15 s without a collection. Up to 16 clients can follow at once; more get 503.

```sh
curl -N -H "Authorization: Bearer $TOKEN" 'http://127.0.0.1:9273/api/v1/stream?sort=cpu&limit=10'
```

## `GET /api/v1/host`

```json
{
  "host": "web-1",
  "timestamp": 1791656312.9,
  "boot_time": 1790663446,
  "uptime_seconds": 992866.9,
  "cpu": {
    "count": 32,
    "usage_ratio": 0.21,
    "modes": {"user": 0.15, "system": 0.04, "iowait": 0.01, "idle": 0.79, "nice": 0.0, "irq": 0.0, "softirq": 0.01, "steal": 0.0},
    "per_cpu": [{"cpu": "0", "usage_ratio": 0.3}]
  },
  "load": {"load1": 2.1, "load5": 1.9, "load15": 1.7},
  "memory": {
    "total_bytes": 51539607552, "available_bytes": 30000000000, "used_bytes": 21539607552,
    "free_bytes": 12000000000, "buffers_bytes": 0, "cached_bytes": 14000000000,
    "swap_total_bytes": 0, "swap_free_bytes": 0, "swap_used_bytes": 0
  },
  "filesystems": [
    {"device": "/dev/sda1", "mountpoint": "/", "fstype": "ext4", "size_bytes": 274877906944, "free_bytes": 69877104640,
     "avail_bytes": 69877104640, "used_ratio": 0.75, "files": 134000000, "files_free": 133000000, "readonly": false}
  ],
  "disks": [
    {"device": "nvme0n1", "read_bytes_total": 1285268645888, "written_bytes_total": 932363079680,
     "read_bytes_per_second": 0.0, "written_bytes_per_second": 2018150.4, "busy_ratio": 0.02}
  ],
  "networks": [
    {"device": "eth0", "receive_bytes_total": 1313635844545, "transmit_bytes_total": 99000000000,
     "receive_bytes_per_second": 961860.3, "transmit_bytes_per_second": 120000.0, "receive_errors_total": 0, "transmit_errors_total": 0}
  ],
  "sensors": [
    {"chip": "pci0000:00_0000:00:18_3", "chip_name": "k10temp", "sensor": "temp1", "label": "Tctl", "kind": "temp", "value": 72.5, "max": null, "crit": null},
    {"chip": "nvme_nvme0", "chip_name": "nvme", "sensor": "temp1", "label": "Composite", "kind": "temp", "value": 38.85, "max": 84.85, "crit": 84.85}
  ],
  "gpus": [
    {"index": 0, "uuid": "00000000-0000-0000-0000-000000000000", "name": "NVIDIA GeForce RTX 3060", "temperature_celsius": 37.0,
     "utilization_ratio": 0.01, "memory_utilization_ratio": 0.0, "memory_used_bytes": 20971520.0, "memory_total_bytes": 12884901888.0,
     "power_draw_watts": 14.05, "fan_speed_ratio": 0.3}
  ]
}
```

`per_cpu` is omitted when `host_metrics.per_cpu = false`. `used_ratio` is used ÷ (used + available to non-root), as `df` reports it. `sensors[].kind` is `temp` (°C) or `fan` (RPM). `gpus` here has no `containers`; [`/api/v1/gpus`](#get-apiv1gpus) adds them.

## `GET /api/v1/containers`

An array of the objects below, sorted by name unless `sort` says otherwise.

## `GET /api/v1/containers/:id`

`:id` is the full container ID, the name or a unique ID prefix of at least four characters. 404 with `{"error": "container not found"}` or `{"error": "ambiguous id prefix"}` otherwise. Only running containers are found.

```json
{
  "id": "3f2a9c4e1b7d8a6f5e4c3b2a1908f7e6d5c4b3a29180f7e6d5c4b3a2918070f6",
  "name": "shop-redis",
  "image": "redis:7.4",
  "compose_project": "shop",
  "compose_service": "redis",
  "labels": {"com.docker.compose.project": "shop", "com.docker.compose.service": "redis", "com.example.team": "web"},
  "pid": 4242,
  "network_mode": "shop_default",
  "started_at": "2026-10-10T07:02:25.289401507Z",
  "created": "2026-10-10T07:02:24.910334322Z",
  "lifecycle": {"state": "running", "health": "healthy", "restarts_total": 1,
                "last_exit": {"code": 137, "oom_killed": true, "exited_at": 1791656001.42}},
  "cpu": {"usage_seconds_total": 391.17, "usage_cores": 0.0096, "limit_cores": null, "throttled_periods_total": 0, "throttled_seconds_total": 0.0},
  "memory": {
    "usage_bytes": 123588608, "working_set_bytes": 119992320, "peak_bytes": 136196096, "limit_bytes": 536870912,
    "rss_bytes": 117788672, "cache_bytes": 4624384, "swap_bytes": 0, "oom_kills_total": 0
  },
  "io": [{"device": "/dev/sda", "read_bytes_total": 18513920, "write_bytes_total": 0, "read_bytes_per_second": 0.0, "write_bytes_per_second": 0.0}],
  "network": [{"interface": "eth0", "receive_bytes_total": 311769077, "transmit_bytes_total": 1519029101,
               "receive_bytes_per_second": 14723.5, "transmit_bytes_per_second": 4361.8}],
  "pids": {"current": 9, "limit": 76881, "processes": 1},
  "gpus": [{"uuid": "00000000-0000-0000-0000-000000000000", "memory_used_bytes": 1658847232}],
  "writable_layer": {"bytes": 52441088, "measured_at": 1791654003.2},
  "collected_at": 1791656309.731,
  "interval_seconds": 15.0
}
```

- `compose_project` and `compose_service` come from the Compose labels whether or not `containers.labels` keeps them, and are `null` for containers Compose didn't start.
- `labels` holds the labels kept by `containers.labels` and `containers.api_labels`. Only the first list reaches `/metrics`.
- `lifecycle.state` is Docker's state (`running`, `restarting`, ...). `health` is `starting`, `healthy` or `unhealthy`, and `null` without a health check. `restarts_total` is Docker's restart count under the restart policy.
- `lifecycle.last_exit` is the most recent exit the collector saw, or Docker reported for a container that stopped before the collector started; `null` otherwise. `oom_killed` is true when Docker reported an OOM kill in the container after it last started, which includes the main process. `exited_at` is Unix seconds.
- `cpu.usage_cores` is cores in use: 1.0 is one full core. `limit_cores` comes from `cpu.max`; `null` means unlimited.
- `memory.limit_bytes` and `pids.limit` are `null` when unlimited. `peak_bytes` is `null` on kernels before 5.19.
- `pids.current` is `pids.current` from the cgroup, which counts every thread. `pids.processes` counts processes in `cgroup.procs`, including child cgroups, and is `null` when that file can't be read.
- `network` is empty for containers with `network_mode: host`, `container:<name>` or Compose's `service:<name>`, which share another network namespace.
- `gpus` has one entry per GPU the container's processes hold memory on, summed over its processes. It's empty for containers with no GPU processes, without GPUs and when the collector doesn't run with `pid: host`; `/health` says which via `gpu_per_container`. `uuid` matches `uuid` in `/api/v1/gpus`.
- `writable_layer` is `null` until the container is measured, and always without `containers.writable_layer`. `bytes` counts every file in the container's writable layer, hard links once; `measured_at` is Unix seconds. See [Writable layer](#writable-layer).

## Stopped containers

`stopped` in the snapshot lists containers that left the running set in the last 10 minutes: they exited, died, were removed, are restarting or are paused. Each has `id`, `name`, `image`, `compose_project`, `compose_service`, `labels`, `pid`, `network_mode`, `started_at` and `created` from its last inspect, a `lifecycle` object as above with `state` set to `exited`, `dead`, `restarting`, `paused` or `removed`, and `stopped_at` in Unix seconds. Containers that stopped before the collector started aren't listed.

## `GET /api/v1/gpus`

```json
[
  {"index": 0, "uuid": "00000000-0000-0000-0000-000000000000", "name": "NVIDIA GeForce RTX 3060", "temperature_celsius": 37.0,
   "utilization_ratio": 0.41, "memory_utilization_ratio": 0.12, "memory_used_bytes": 2168455168.0, "memory_total_bytes": 12884901888.0,
   "power_draw_watts": 64.2, "fan_speed_ratio": 0.3,
   "containers": [{"id": "9a1f…", "name": "llm", "memory_used_bytes": 1658847232}]}
]
```

`containers` lists the containers holding memory on that GPU, largest first. It's `[]` when none do and `null` when per-container GPU memory isn't read (see `gpu_per_container` on `/health`). An empty array means the host has no GPU or `nvidia-smi` isn't available. Per-container GPU utilization isn't reported: `nvidia-smi` gives memory per process but utilization only per GPU.

## Writable layer

With `containers.writable_layer = true`, a background thread adds up the files in each container's writable layer (the overlay upper directory from `GraphDriver.Data.UpperDir`, or the root mount in `/proc/<pid>/mountinfo`). It measures one container at a time, spacing them so a full pass takes `containers.writable_layer_interval` (1 h); containers never measured go first. The walk reads under the `/rootfs` mount and doesn't touch the Docker API, so it doesn't load the daemon the way `docker ps --size` does.

Directories a container keeps private, such as a database's `0700` data directory, can only be listed with `CAP_DAC_READ_SEARCH`. Without it those directories are skipped, the size comes out low and the collector logs a warning once. Add it when turning this on:

```yaml
    cap_add: [DAC_READ_SEARCH]
```
