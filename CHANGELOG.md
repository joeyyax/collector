# Changelog

## 0.3.0

Breaking: container metrics use clean names by default. See [Migrating from 0.2](docs/metrics.md#migrating-from-02).

- Container series carry `container`, `container_id`, `image`, `compose_project` and `compose_service` instead of `name`, `id` and the Compose `container_label_*` labels. Memory gauges end in `_bytes`: `container_memory_rss_bytes`, `_cache_bytes`, `_mapped_file_bytes`, `_kernel_usage_bytes`, `_swap_bytes`.
- `?compat=cadvisor` on a scrape, or `metrics.compat = "cadvisor"` for remote write, renders cAdvisor's names and labels as 0.2 did.
- `container_accelerator_memory_used_bytes` is now `container_gpu_memory_used_bytes` in both modes, since it counts the container's own processes rather than the whole GPU.
- `container_processes` counts processes from `cgroup.procs`; it used to report `pids.current`, which counts threads. Threads are now `container_threads`. The API adds `pids.processes`, the process count from `cgroup.procs`, or `null` when the file can't be read.
- Lifecycle from Docker events and inspect: `container_state` (kept 10 minutes after a container stops), `container_health`, `container_restarts_total`, `container_last_exit_code` and `container_oom_killed`, which catches OOM kills of the main process. The API adds a `lifecycle` object per container.
- `GET /api/v1/snapshot`: host, GPUs, running and recently stopped containers from one collection.
- `GET /api/v1/stream`: each snapshot as a server-sent event. Collection runs every `live_interval` (5 s) while anyone follows it.
- `GET /api/v1/gpus`: GPUs with the containers holding memory on each, or `null` when per-container GPU memory isn't read. `/health` adds `gpu_per_container`.
- `sort` and `limit` on `/api/v1/containers`, the snapshot and the stream.
- Containers report disk read and write rates, and `compose_project` and `compose_service` as plain fields.
- Writable-layer size, off by default: `containers.writable_layer` measures one container at a time so a pass takes `writable_layer_interval` (1 h). `container_writable_layer_bytes` and `writable_layer` in the API.
- `logs.tenant_label`: send each container's logs to the VictoriaLogs tenant in that label, one request per tenant per flush.
- Host metrics: `node_uname_info`, `node_time_seconds`, `node_vmstat_{oom_kill,pgmajfault,pswpin,pswpout}`, `node_pressure_*`, `node_network_{up,speed_bytes,mtu_bytes}`, `node_filefd_*` and `node_nf_conntrack_entries{,_limit}`. Containers add `container_pressure_*`. Each is left out when its file is missing.
- `collector health`, `host`, `containers` and `gpus` print the API as tables, or raw with `--json`. The image runs `collector health` as its `HEALTHCHECK`.
- Fixed: dropping the Compose labels from `containers.labels` also dropped the `compose_project` and `compose_service` log fields.
- Fixed: a kept Docker label named like a log field, such as `host`, replaced that field.
- Fixed: a container ID prefix matching several containers returned the first; it's now an error.
- The `/health` docs now give the stale threshold the code uses: three intervals plus 5 s.

## 0.2.0

- Per-container GPU memory from a second long-running `nvidia-smi --query-compute-apps` process: `gpus` in the containers API and `container_accelerator_memory_used_bytes`. Needs `pid: host`.
- `collected_at` and `interval_seconds` on `/health` and on each container in the API.
- `containers.api_labels`: labels kept on the JSON API and log fields but never on metrics.
- Image index annotations for source, description and license, so GHCR links the package to the repo.
- Auto GPU mode no longer turns GPU metrics off for good when `nvidia-smi` restarts and reports nothing.
- Images built without `VERSION` report the crate version instead of an empty one.

## 0.1.0

First release.

- Host metrics from `/proc` and `/sys`, including hwmon temperatures and fans, with node_exporter names.
- Per-container metrics from cgroup v2 files and network namespaces, with cAdvisor names. Docker events re-inspect only the containers they name.
- NVIDIA GPU metrics from one long-running `nvidia-smi` process, with nvidia_gpu_exporter names.
- Container logs from Docker `json-file` files, shipped to VictoriaLogs or a local file with per-container cursors.
- Prometheus text at `/metrics`, optional remote-write push and a JSON API with optional bearer-token auth.
