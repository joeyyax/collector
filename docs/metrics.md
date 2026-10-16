# Metrics and compatibility

Host and GPU names and labels match node_exporter (v1.x) and nvidia_gpu_exporter where the collector produces the same signal. Container metrics use cAdvisor's (v0.5x) names with cleaner labels by default; `?compat=cadvisor` on a scrape, or `metrics.compat = "cadvisor"` for remote write, renders cAdvisor's exact names and labels so existing dashboards and alerts keep working. Values are identical in both modes. This page lists what matches, what differs and what's missing.

A name the collector shares with cAdvisor or node_exporter measures what theirs does. When the meaning differs, the name differs too.

## Host (node_exporter)

| Metric | Labels | Notes |
|---|---|---|
| `node_cpu_seconds_total` | `cpu`, `mode` | Modes user, nice, system, idle, iowait, irq, softirq, steal. With `per_cpu = false`, one series per mode with `cpu="all"` |
| `node_cpu_guest_seconds_total` | `cpu`, `mode` | |
| `node_load1`, `node_load5`, `node_load15` | | |
| `node_memory_<Field>_bytes` | | Every `/proc/meminfo` field, named as node_exporter does (`Active(anon)` → `Active_anon`). `HugePages_*` have no `_bytes` suffix |
| `node_filesystem_{size,free,avail}_bytes`, `node_filesystem_files`, `node_filesystem_files_free`, `node_filesystem_readonly`, `node_filesystem_device_error` | `device`, `mountpoint`, `fstype` | Mount table from the host's PID 1 |
| `node_disk_{reads,writes}_completed_total`, `node_disk_{reads,writes}_merged_total`, `node_disk_read_bytes_total`, `node_disk_written_bytes_total`, `node_disk_{read,write}_time_seconds_total`, `node_disk_io_now`, `node_disk_io_time_seconds_total`, `node_disk_io_time_weighted_seconds_total` | `device` | Same default device filter as node_exporter (partitions, loop, ram excluded) |
| `node_network_{receive,transmit}_{bytes,packets,errs,drop}_total` | `device` | Read from the host's network namespace (`/proc/1/net/dev`) |
| `node_hwmon_temp_celsius`, `node_hwmon_temp_max_celsius`, `node_hwmon_temp_crit_celsius` | `chip`, `sensor` | `chip` uses node_exporter's naming (`pci0000:00_0000:00:18_3`, `platform_nct6683_2592`, `nvme_nvme0`) |
| `node_hwmon_chip_names` | `chip`, `chip_name` | Join on `chip` to filter by driver: `node_hwmon_temp_celsius * on(chip) group_left(chip_name) node_hwmon_chip_names{chip_name="k10temp"}` |
| `node_hwmon_sensor_label` | `chip`, `sensor`, `label` | |
| `node_hwmon_fan_rpm` | `chip`, `sensor` | |
| `node_boot_time_seconds`, `node_context_switches_total`, `node_forks_total`, `node_intr_total`, `node_procs_running`, `node_procs_blocked` | | |
| `node_uname_info` | `sysname`, `release`, `version`, `machine`, `nodename`, `domainname` | From `/proc/sys/kernel/{ostype,osrelease,version,domainname}`. `nodename` is the collector's `host` rather than the container's own hostname |
| `node_time_seconds` | | |
| `node_vmstat_oom_kill`, `node_vmstat_pgmajfault`, `node_vmstat_pswpin`, `node_vmstat_pswpout` | | `/proc/vmstat`; untyped, as node_exporter has them |
| `node_pressure_{cpu,io,memory}_waiting_seconds_total`, `node_pressure_{cpu,io,memory,irq}_stalled_seconds_total` | | `/proc/pressure/*` `some` and `full` lines |
| `node_network_up`, `node_network_speed_bytes`, `node_network_mtu_bytes` | `device` | `/sys/class/net/<device>/{operstate,speed,mtu}`. `up` is 1 when operstate is `up`; virtual devices have no speed |
| `node_filefd_allocated`, `node_filefd_maximum` | | `/proc/sys/fs/file-nr` |
| `node_nf_conntrack_entries`, `node_nf_conntrack_entries_limit` | | `/proc/1/net/stat/nf_conntrack`, falling back to `/proc/sys/net/netfilter/nf_conntrack_count`, and `nf_conntrack_max` |

Each of these is read from one file per collection. When the file is missing or unreadable, for example `/proc/pressure` on a kernel without PSI or conntrack without the module loaded, the series is left out rather than reported as zero.

Differences from node_exporter defaults:

- **Network namespace.** node_exporter in a bridge-networked container reports its own container's interfaces unless it runs with `network_mode: host`. The collector always reads the host's, so `node_network_*` values can jump when switching.
- **Interfaces.** `veth*` is excluded by default (`host_metrics.network_exclude`); node_exporter keeps them.
- **Filesystems.** `tmpfs`, `ramfs` and FUSE are excluded by default, and so is everything under `/run`. NFS and CIFS are included. Each `statfs` runs on its own thread with a shared `statfs_timeout` (2 s); a mount that doesn't answer reports `node_filesystem_device_error 1` and is skipped until its call returns, so a hung mount never stalls collection. `collector_filesystems_hung` counts them.
- **Conntrack.** `/proc/sys/net` shows the reader's own network namespace, so entries come from PID 1's `/proc/1/net/stat/nf_conntrack` to report the host's table from a bridge-networked container.
- **Not collected:** other `node_vmstat_*` fields, `node_netstat_*`, `node_sockstat_*`, `node_timex_*`, `node_hwmon_in_volts`, `node_hwmon_power_*`, textfile collector, systemd, ZFS, mdadm, EDAC and the rest of node_exporter's collectors.

## Containers (cAdvisor)

### Labels

| Clean (default) | `compat=cadvisor` |
|---|---|
| `container`: container name | `name` |
| `container_id`: first 12 characters of the container ID, as `docker ps` and the log field show it | `id`: cgroup path relative to the cgroup root (`/system.slice/docker-<id>.scope`) |
| `image` | `image` |
| `compose_project`, `compose_service`: always present for Compose containers | `container_label_com_docker_compose_project`, `container_label_com_docker_compose_service`, only when `containers.labels` keeps them |
| `container_label_<sanitized key>` for each other label `containers.labels` keeps | the same, plus the Compose labels above |

### Label cardinality

`containers.labels` is an allowlist. The default keeps `com.docker.compose.project` and `com.docker.compose.service`, which matter in `compat=cadvisor` mode; clean mode always has `compose_project` and `compose_service`.

- **Series count.** A label whose value is fixed for a container's life adds no series; it only makes each one longer. About 60 series per container carry every kept label, so on a 100-container host each extra label adds roughly 6,000 label pairs to every scrape and to the index.
- **Churn.** A label whose value changes on every deploy, such as a deployment id, starts a new set of series per deploy. The old ones linger in the TSDB index until retention drops them. Over 90 days with daily deploys that's about 90 times the live series for that app. The container ID label already churns per container; per-deploy labels on top of it add nothing a query needs.
- **Size.** With five extra labels kept per container, `/metrics` was 3.1 MB on a 100-container host against cAdvisor's 2.6 MB.

To opt back in, list the labels or a prefix:

```toml
[containers]
labels = ["com.docker.compose.project", "com.docker.compose.service", "com.example.*"]
```

Labels a consumer needs per container but no query groups by, such as a deployment id or an owner, belong in `containers.api_labels`. It takes the same patterns and adds labels to the JSON API and log fields but never to `/metrics` or remote write:

```toml
[containers]
api_labels = ["com.example.deploy-id", "com.example.owner"]
```

Logs get the labels from both lists as fields. VictoriaLogs only indexes `stream_fields` as streams, so extra fields there cost storage, not stream cardinality.

### Metrics

Names are the same in both modes except where the `compat=cadvisor` column says otherwise.

| Metric | `compat=cadvisor` | Source | Notes |
|---|---|---|---|
| `container_cpu_usage_seconds_total{cpu="total"}` | | `cpu.stat` usage_usec | |
| `container_cpu_user_seconds_total`, `container_cpu_system_seconds_total` | | `cpu.stat` | |
| `container_cpu_cfs_periods_total`, `container_cpu_cfs_throttled_periods_total`, `container_cpu_cfs_throttled_seconds_total` | | `cpu.stat` nr_periods, nr_throttled, throttled_usec | Emitted for every container; cAdvisor only emits them when a quota is set |
| `container_spec_cpu_period`, `container_spec_cpu_quota` | | `cpu.max` | Quota only when limited |
| `container_memory_usage_bytes` | | `memory.current` | |
| `container_memory_working_set_bytes` | | `memory.current` − `inactive_file` | Same formula as cAdvisor |
| `container_memory_rss_bytes` | `container_memory_rss` | `memory.stat` anon | |
| `container_memory_cache_bytes` | `container_memory_cache` | `memory.stat` file | |
| `container_memory_mapped_file_bytes` | `container_memory_mapped_file` | `memory.stat` file_mapped | |
| `container_memory_kernel_usage_bytes` | `container_memory_kernel_usage` | `memory.stat` kernel | |
| `container_memory_total_{active,inactive}_file_bytes` | | `memory.stat` active_file, inactive_file | |
| `container_memory_max_usage_bytes` | | `memory.peak` | Kernel 5.19+ |
| `container_memory_swap_bytes` | `container_memory_swap` | `memory.swap.current` | |
| `container_spec_memory_limit_bytes` | | `memory.max` | 0 when unlimited, as in cAdvisor |
| `container_oom_events_total` | | `memory.events` oom_kill | OOM kills inside a running container; the cgroup and its count go away when the container stops. `container_oom_killed` covers the kill that stops it |
| `container_pressure_{cpu,memory,io}_waiting_seconds_total`, `container_pressure_{cpu,memory,io}_stalled_seconds_total` | | `cpu.pressure`, `memory.pressure`, `io.pressure` `some` and `full` | Left out when the file is missing |
| `container_fs_{reads,writes}_bytes_total`, `container_fs_{reads,writes}_total` | | `io.stat` | `device` is `/dev/<name>` from `/sys/dev/block` |
| `container_network_{receive,transmit}_{bytes,packets,errors,packets_dropped}_total` | | `/proc/<pid>/net/dev` | `interface` label; `lo` skipped |
| `container_processes` | | `cgroup.procs`, counted across child cgroups | Processes, as in cAdvisor. Left out when `cgroup.procs` is missing |
| `container_threads` | | `pids.current` | Every task, so threads count. cAdvisor exposes both only with its process metrics enabled |
| `container_spec_pids_limit` | | `pids.max` | Not in cAdvisor |
| `container_start_time_seconds` | | Docker `State.StartedAt` | Matches cAdvisor; cAdvisor rounds to whole seconds |
| `container_last_seen` | | | |
| `container_gpu_memory_used_bytes` | | `nvidia-smi --query-compute-apps` | `uuid` label. One series per GPU the container's processes hold memory on; none for containers without GPU processes. Needs `pid: host` |
| `container_writable_layer_bytes` | | Walk of the overlay upper directory | Only with `containers.writable_layer`; up to `writable_layer_interval` old. See [api.md](api.md#writable-layer) |

### Lifecycle

From Docker events and inspect, not cgroups, so they cover what cAdvisor can't see. Same names in both modes.

| Metric | Labels | Meaning |
|---|---|---|
| `container_state` | `state` | 1 for Docker's current state (`running`, `restarting`, `exited`, `dead`, `paused`, `removed`). Kept for 10 minutes after a container stops, so an alert can see it go |
| `container_health` | `health` | 1 for the health check's status (`starting`, `healthy`, `unhealthy`); absent without a health check |
| `container_restarts_total` | | Docker's `RestartCount`: restarts under the restart policy |
| `container_last_exit_code` | | Exit code of the last exit the collector saw, or Docker reported for a stopped container |
| `container_oom_killed` | | 1 when Docker reported an OOM kill in the container after it last started and before that exit, which includes the main process; 0 otherwise |

`container_last_exit_code` and `container_oom_killed` are absent until a container has exited while the collector runs, so a container that crash-looped before the collector started shows its restarts but no exit code until the next exit. Stopped containers keep only these lifecycle series; their resource series end when they stop.

```promql
# Restarted in the last 15 minutes
increase(container_restarts_total[15m]) > 0
# Killed by the OOM killer
container_oom_killed == 1
```

### Differences from cAdvisor

- **Docker only.** No series for the root cgroup, systemd slices or other runtimes, so queries don't need `name!=""` (it's harmless in `compat=cadvisor` mode).
- **Shared network namespaces.** Containers with `network_mode: host`, `container:<name>` or Compose's `service:<name>` get no `container_network_*` series. cAdvisor reports the shared namespace's interfaces for them, which double-counts its traffic.
- **Processes and threads.** `container_processes` counts processes and `container_threads` counts threads, as in cAdvisor. Before 0.3.0 the collector reported `pids.current` (threads) as `container_processes`.
- **Not collected:** `container_fs_usage_bytes` and cAdvisor's other filesystem-size metrics (`container_writable_layer_bytes` measures the writable layer alone and is opt-in), `container_fs_inodes_*`, `container_cpu_load_average_10s`, `container_tasks_state`, `container_memory_failures_total`, `container_memory_failcnt`, `container_spec_cpu_shares`, `container_spec_memory_reservation_limit_bytes`, `container_spec_memory_swap_limit_bytes`, `container_threads_max`, `container_creation_time_seconds`, `container_blkio_device_usage_total`.
- **GPU memory.** `container_gpu_memory_used_bytes` sums the memory of the container's own processes. cAdvisor's `container_accelerator_memory_used_bytes` reported the whole GPU's memory for every container the GPU was attached to, with `make`, `model` and `acc_id` labels, so the collector uses a different name in both modes. The label is `uuid`, so it joins with `nvidia_smi_*`. The other `container_accelerator_*` metrics aren't collected.
- **Timestamps.** cAdvisor attaches its own sample timestamps; the collector doesn't, so scrapes use scrape time.

### Migrating from 0.2

0.2 rendered cAdvisor's names and labels. 0.3 renders clean ones by default.

- **Keep 0.2's output:** scrape `/metrics?compat=cadvisor` (Prometheus: `params: {compat: [cadvisor]}`) and set `metrics.compat = "cadvisor"` for remote write. Only the two renames below still apply.
- **Or move queries to clean names:**

| 0.2 | 0.3 |
|---|---|
| `name` | `container` |
| `id` (cgroup path) | `container_id` (12-character container ID) |
| `container_label_com_docker_compose_project` | `compose_project` |
| `container_label_com_docker_compose_service` | `compose_service` |
| `container_memory_rss` | `container_memory_rss_bytes` |
| `container_memory_cache` | `container_memory_cache_bytes` |
| `container_memory_mapped_file` | `container_memory_mapped_file_bytes` |
| `container_memory_kernel_usage` | `container_memory_kernel_usage_bytes` |
| `container_memory_swap` | `container_memory_swap_bytes` |

In both modes:

| 0.2 | 0.3 |
|---|---|
| `container_accelerator_memory_used_bytes` | `container_gpu_memory_used_bytes` |
| `container_processes` (was threads) | `container_threads`; `container_processes` now counts processes |

## GPU (nvidia_gpu_exporter)

| Metric | Labels |
|---|---|
| `nvidia_smi_gpu_info` | `uuid`, `name`, `index` |
| `nvidia_smi_temperature_gpu` | `uuid` |
| `nvidia_smi_utilization_gpu_ratio`, `nvidia_smi_utilization_memory_ratio` | `uuid` |
| `nvidia_smi_memory_used_bytes`, `nvidia_smi_memory_total_bytes` | `uuid` |
| `nvidia_smi_power_draw_watts` | `uuid` |
| `nvidia_smi_fan_speed_ratio` | `uuid` |

`uuid` drops the `GPU-` prefix, as the exporter does. `nvidia_smi_gpu_info` has fewer labels than the exporter's (no driver or VBIOS version).

Readings come from `nvidia-smi --query-gpu --loop-ms=<interval>`. Once it reports a GPU and container metrics are on, a second `nvidia-smi --query-compute-apps` loop reads per-process GPU memory for `container_gpu_memory_used_bytes`. Each process's container comes from `/proc/<pid>/cgroup`. When the GPU loop prints nothing for three intervals, both are killed and restarted. With `gpu.enabled = "off"`, a build without the `gpu` feature or no `nvidia-smi`, neither runs.

## The collector's own

| Metric | Meaning |
|---|---|
| `process_cpu_seconds_total`, `process_resident_memory_bytes` | Collector process only (not the `nvidia-smi` children) |
| `collector_filesystems_hung` | Mounts whose `statfs` hasn't returned |
| `collector_log_containers_followed_api`, `collector_log_containers_unsupported` | Containers on `local`/`journald` (read through the API) and on drivers that can't be read |
| `collector_remote_write_dropped_samples_total`, `collector_remote_write_queue_pushes`, `collector_remote_write_queue_bytes` | Remote-write retry queue |
| `collector_panics_total` | Panics caught in collector threads; each is logged as an error |
| `collector_heap_bytes` | Bytes allocated right now; compare with RSS to tell a leak from allocator overhead |
| `collector_build_info{version}` | |
| `collector_collect_duration_seconds`, `collector_last_collect_timestamp_seconds`, `collector_series`, `collector_containers` | Last collection |
| `collector_docker_up` | Whether the last Docker sync worked |
| `collector_log_lines_shipped_total`, `collector_log_bytes_shipped_total`, `collector_log_batches_total`, `collector_log_send_errors_total` | Log delivery |
| `collector_log_lines_unparsed_total`, `collector_log_rotations_missed_total` | Log data loss signals |
| `collector_log_containers_followed`, `collector_log_last_success_timestamp_seconds` | |
| `collector_remote_write_pushes_total`, `collector_remote_write_errors_total`, `collector_remote_write_last_success_timestamp_seconds` | |

## Labels on push

`/metrics` has no `host` or `instance` label; the scraper adds them. Remote-write adds `remote_write.labels`, which defaults to `host = <host>`. A configured label replaces a sample label with the same name. Remote write uses clean names unless `metrics.compat = "cadvisor"`.
