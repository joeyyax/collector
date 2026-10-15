# JSON API

Read-only, `GET` only, on `127.0.0.1:9273` unless `listen` says otherwise. With `api.token` or `api.token_file` set, every endpoint except `/health` needs `Authorization: Bearer <token>` and answers 401 without it. `/metrics` is gzipped when the client sends `Accept-Encoding: gzip`, as Prometheus and VictoriaMetrics do.

Every response reflects the last collection (every `interval`, 15 s by default); nothing is read from the host on request.

Rates (`*_per_second`, `usage_ratio`, `usage_cores`, `busy_ratio`) cover the time between the last two collections. They're `null` after the first collection and after a counter reset.

## `GET /health`

```json
{"status": "ok", "version": "0.1.0", "host": "web-1", "last_collect_age_seconds": 3.2, "docker": true, "logs_last_success": 1791656289}
```

`status` is `stale` with HTTP 503 when the last collection is older than three intervals. `docker` is present when container metrics are on, `logs_last_success` (epoch seconds) when logs are on.

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

`per_cpu` is omitted when `host_metrics.per_cpu = false`. `used_ratio` is used ÷ (used + available to non-root), as `df` reports it. `sensors[].kind` is `temp` (°C) or `fan` (RPM).

## `GET /api/v1/containers`

An array of the objects below, sorted by name.

## `GET /api/v1/containers/:id`

`:id` is the full container ID, the name, or an ID prefix of at least four characters. 404 with `{"error": "container not found"}` otherwise.

```json
{
  "id": "3f2a9c4e1b7d8a6f5e4c3b2a1908f7e6d5c4b3a29180f7e6d5c4b3a2918070f6",
  "name": "shop-redis",
  "image": "redis:7.4",
  "labels": {"com.docker.compose.project": "shop", "com.docker.compose.service": "redis", "com.example.team": "web"},
  "pid": 4242,
  "network_mode": "shop_default",
  "started_at": "2026-10-10T07:02:25.289401507Z",
  "created": "2026-10-10T07:02:24.910334322Z",
  "cpu": {"usage_seconds_total": 391.17, "usage_cores": 0.0096, "limit_cores": null, "throttled_periods_total": 0, "throttled_seconds_total": 0.0},
  "memory": {
    "usage_bytes": 123588608, "working_set_bytes": 119992320, "peak_bytes": 136196096, "limit_bytes": 536870912,
    "rss_bytes": 117788672, "cache_bytes": 4624384, "swap_bytes": 0, "oom_kills_total": 0
  },
  "io": [{"device": "/dev/sda", "read_bytes_total": 18513920, "write_bytes_total": 0}],
  "network": [{"interface": "eth0", "receive_bytes_total": 311769077, "transmit_bytes_total": 1519029101,
               "receive_bytes_per_second": 14723.5, "transmit_bytes_per_second": 4361.8}],
  "pids": {"current": 9, "limit": 76881}
}
```

- `labels` holds only the kept labels (`containers.labels`).
- `cpu.usage_cores` is cores in use: 1.0 is one full core. `limit_cores` comes from `cpu.max`; `null` means unlimited.
- `memory.limit_bytes` and `pids.limit` are `null` when unlimited. `peak_bytes` is `null` on kernels before 5.19.
- `network` is empty for `network_mode: host` containers.
