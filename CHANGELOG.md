# Changelog

## 0.4.0

Breaking: only the `json-file` log driver is read, and `logs.multiline` is gone.

- Removed: logs from the `local` and `journald` drivers through the Docker logs API. Those containers get one warning each and count in `collector_log_containers_unsupported`, like `syslog`. `collector_log_containers_followed_api` is gone. Cursor files with API entries still load; the entries are dropped.
- Removed: `logs.multiline` and `logs.api_queue_lines`. A config or `COLLECTOR_*` override that still sets one fails at startup naming the key. Docker's 16 KB partial lines are still joined, up to 1 MiB.
- Fixed: a rotation between reading the file list and opening a file could mislabel or skip bytes, and a file rotated mid-scan could be missed. Files are read through open handles identified by `fstat`; the file holding the cursor stays readable after Docker deletes it, so a removed `--rm` container's last lines still ship.
- Fixed: a corrupt or empty cursor file was treated as a first run and skipped all history. It's now renamed to `log-cursors.json.bad-<time>`, logged as an error, and every container is read from its oldest file.
- Fixed: cursor saves weren't durable. The file is fsynced before the rename, and the directory after.
- Fixed: one tenant failing blocked every tenant, and a restart re-sent tenants that had already been delivered. Each tenant's batch is delivered, retried and committed on its own.
- While a partial line is unfinished, that container's later lines wait behind it, for at most `logs.partial_timeout` (2 s; 0 is a startup error). Then the unfinished part is sent as is and the rest follows as a new record, counted in `collector_log_partials_split_total`. The saved cursor no longer carries `replay_until`. A 0.3 cursor saved mid-partial re-sends the records after that point once.
- Polling costs one `stat` per container instead of one per possible rotation.
- `logs.poll` or `remote_write.interval` of 0 is a startup error.

## 0.3.1

- Fixed: restarts, health and pids went stale after a gap in Docker's event stream, such as a dockerd restart. Events resume where they left off, and the periodic sync re-inspects containers whose process changed.
- Fixed: a timed-out or failed inspect marked a running container as removed. Only a 404 does now.
- Fixed: restarting containers were missing from both the running and stopped lists. They're listed as stopped with state `restarting`.
- Containers sharing another container's network namespace (`network_mode: container:<id>`) report no network, like host-network ones, so traffic isn't counted twice.
- Fixed: two kept labels that sanitize to the same metric label name broke the scrape. The last one wins.
- A `nvidia-smi` that stops printing is killed and restarted.
- Fixed: environment overrides for string settings keep the value as written, so `COLLECTOR_API__TOKEN=123456` no longer stops startup and quotes are no longer stripped.
- Durations accept fractions such as `COLLECTOR_INTERVAL=0.5` or `interval = 0.5`. `interval` or `live_interval` of 0 and durations over 366 days are startup errors.
- Without a token, listening beyond loopback logs a warning at startup unless `api.auth = "off"`; `api.auth = "token"` refuses to start without one. `/health` adds `auth`: `token` or `open`.
- An empty `api.token_file` is a startup error; it used to leave the API open.
- `/api/v1/host` answers 404 `host metrics are disabled` when they're off, instead of 503 `no data yet` forever. `/health` reports `host` with host metrics off.
- A response that makes no progress for 30 s is dropped, so clients that stop reading `/metrics` or a stream can't pin the workers or stream slots.
- Remote write drops a push the receiver rejects with a 4xx other than 429 and counts its samples in `collector_remote_write_dropped_samples_total`; 5xx, 429 and network errors are still retried.
- A panic in one thread no longer stops the process: it's logged and counted in `collector_panics_total`. Host and container collection fail independently, and remote write restarts.
- `deploy/compose.yaml`: uncommenting the GPU lines yields a valid file.
- `collector health`, `host`, `containers` and `gpus` report a config that doesn't load instead of falling back to defaults.
- `COLLECTOR_VERSION` in the environment is ignored instead of failing as an unknown key.

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
- Containers report disk read and write rates. `compose_project` and `compose_service` are plain fields.
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
