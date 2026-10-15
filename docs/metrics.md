# Metrics and compatibility

Names and labels match node_exporter (v1.x), cAdvisor (v0.5x) and nvidia_gpu_exporter where the collector produces the same signal, so PromQL written for them keeps working. This page lists what matches, what differs and what's missing.

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

Differences from node_exporter defaults:

- **Network namespace.** node_exporter in a bridge-networked container reports its own container's interfaces unless it runs with `network_mode: host`. The collector always reads the host's, so `node_network_*` values can jump when switching.
- **Interfaces.** `veth*` is excluded by default (`host_metrics.network_exclude`); node_exporter keeps them.
- **Filesystems.** `tmpfs`, `ramfs` and FUSE are excluded by default, and so is everything under `/run`. NFS and CIFS are included. Each `statfs` runs on its own thread with a shared `statfs_timeout` (2 s); a mount that doesn't answer reports `node_filesystem_device_error 1` and is skipped until its call returns, so a hung mount never stalls collection. `collector_filesystems_hung` counts them.
- **Not collected:** `node_uname_info`, `node_time_seconds`, `node_vmstat_*`, `node_netstat_*`, `node_sockstat_*`, `node_pressure_*`, `node_hwmon_in_volts`, `node_hwmon_power_*`, textfile collector, systemd, ZFS, mdadm and the rest of node_exporter's collectors.

## Containers (cAdvisor)

Every container series carries `id`, `name` and `image` like cAdvisor, plus one `container_label_<sanitized key>` per kept Docker label. `container_label_com_docker_compose_project` works as it does with cAdvisor's label whitelist.

### Label cardinality

`containers.labels` is an allowlist. The default keeps `com.docker.compose.project` and `com.docker.compose.service`.

- **Series count.** A label whose value is fixed for a container's life adds no series; it only makes each one longer. About 60 series per container carry every kept label, so on a 100-container host each extra label adds roughly 6,000 label pairs to every scrape and to the index.
- **Churn.** A label whose value changes on every deploy, such as a deployment id, starts a new set of series per deploy. The old ones linger in the TSDB index until retention drops them. Over 90 days with daily deploys that's about 90 times the live series for that app. The container `id` label already churns per container; per-deploy labels on top of it add nothing a query needs.
- **Size.** With five extra labels kept per container, `/metrics` was 3.1 MB on a 100-container host against cAdvisor's 2.6 MB.

To opt back in, list the labels or a prefix:

```toml
[containers]
labels = ["com.docker.compose.project", "com.docker.compose.service", "com.example.*"]
```

Logs get the same kept labels as fields. VictoriaLogs only indexes `stream_fields` as streams, so extra fields there cost storage, not stream cardinality.

| Metric | Source | Notes |
|---|---|---|
| `container_cpu_usage_seconds_total{cpu="total"}` | `cpu.stat` usage_usec | |
| `container_cpu_user_seconds_total`, `container_cpu_system_seconds_total` | `cpu.stat` | |
| `container_cpu_cfs_periods_total`, `container_cpu_cfs_throttled_periods_total`, `container_cpu_cfs_throttled_seconds_total` | `cpu.stat` nr_periods, nr_throttled, throttled_usec | Emitted for every container; cAdvisor only emits them when a quota is set |
| `container_spec_cpu_period`, `container_spec_cpu_quota` | `cpu.max` | Quota only when limited |
| `container_memory_usage_bytes` | `memory.current` | |
| `container_memory_working_set_bytes` | `memory.current` − `inactive_file` | Same formula as cAdvisor |
| `container_memory_rss`, `container_memory_cache`, `container_memory_mapped_file`, `container_memory_kernel_usage`, `container_memory_total_{active,inactive}_file_bytes` | `memory.stat` anon, file, file_mapped, kernel, active_file, inactive_file | |
| `container_memory_max_usage_bytes` | `memory.peak` | Kernel 5.19+ |
| `container_memory_swap` | `memory.swap.current` | |
| `container_spec_memory_limit_bytes` | `memory.max` | 0 when unlimited, as in cAdvisor |
| `container_oom_events_total` | `memory.events` oom_kill | |
| `container_fs_{reads,writes}_bytes_total`, `container_fs_{reads,writes}_total` | `io.stat` | `device` is `/dev/<name>` from `/sys/dev/block` |
| `container_network_{receive,transmit}_{bytes,packets,errors,packets_dropped}_total` | `/proc/<pid>/net/dev` | `interface` label; `lo` skipped |
| `container_processes` | `pids.current` | cAdvisor exposes this only with its process metrics enabled |
| `container_spec_pids_limit` | `pids.max` | Not in cAdvisor |
| `container_start_time_seconds` | Docker `State.StartedAt` | Matches cAdvisor; cAdvisor rounds to whole seconds |
| `container_last_seen` | | |
| `container_accelerator_memory_used_bytes` | `nvidia-smi --query-compute-apps` | `uuid` label. One series per GPU the container's processes hold memory on; none for containers without GPU processes. Needs `pid: host` |

Differences from cAdvisor:

- **Running containers only, Docker only.** No series for the root cgroup, systemd slices or stopped containers, so queries don't need `name!=""` (it's harmless).
- **`id` label.** The cgroup path relative to the cgroup root (`/system.slice/docker-<id>.scope`), same as cAdvisor.
- **Host-network containers** get no `container_network_*` series. cAdvisor reports the host's interfaces for them, which double-counts host traffic.
- **Not collected:** `container_fs_usage_bytes` and the other writable-layer size metrics (they need a filesystem walk; this is cAdvisor's most expensive work), `container_fs_inodes_*`, `container_cpu_load_average_10s`, `container_tasks_state`, `container_memory_failures_total`, `container_memory_failcnt`, `container_pressure_*`, `container_spec_cpu_shares`, `container_spec_memory_reservation_limit_bytes`, `container_spec_memory_swap_limit_bytes`, `container_health_state`, `container_creation_time_seconds`, `container_blkio_device_usage_total`.
- **GPU memory.** `container_accelerator_memory_used_bytes` sums the memory of the container's own processes. cAdvisor reported the whole GPU's memory for every container the GPU was attached to, with `make`, `model` and `acc_id` labels. Here the label is `uuid`, so it joins with `nvidia_smi_*`. The other `container_accelerator_*` metrics aren't collected.
- **Timestamps.** cAdvisor attaches its own sample timestamps; the collector doesn't, so scrapes use scrape time.

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

Readings come from `nvidia-smi --query-gpu --loop-ms=<interval>`. Once it reports a GPU and container metrics are on, a second `nvidia-smi --query-compute-apps` loop reads per-process GPU memory for `container_accelerator_memory_used_bytes`. Each process's container comes from `/proc/<pid>/cgroup`. With `gpu.enabled = "off"`, a build without the `gpu` feature or no `nvidia-smi`, neither runs.

## The collector's own

| Metric | Meaning |
|---|---|
| `process_cpu_seconds_total`, `process_resident_memory_bytes` | Collector process only (not the `nvidia-smi` children) |
| `collector_filesystems_hung` | Mounts whose `statfs` hasn't returned |
| `collector_log_containers_followed_api`, `collector_log_containers_unsupported` | Containers on `local`/`journald` (read through the API) and on drivers that can't be read |
| `collector_remote_write_dropped_samples_total`, `collector_remote_write_queue_pushes`, `collector_remote_write_queue_bytes` | Remote-write retry queue |
| `collector_heap_bytes` | Bytes allocated right now; compare with RSS to tell a leak from allocator overhead |
| `collector_build_info{version}` | |
| `collector_collect_duration_seconds`, `collector_last_collect_timestamp_seconds`, `collector_series`, `collector_containers` | Last collection |
| `collector_docker_up` | Whether the last Docker sync worked |
| `collector_log_lines_shipped_total`, `collector_log_bytes_shipped_total`, `collector_log_batches_total`, `collector_log_send_errors_total` | Log delivery |
| `collector_log_lines_unparsed_total`, `collector_log_rotations_missed_total` | Log data loss signals |
| `collector_log_containers_followed`, `collector_log_last_success_timestamp_seconds` | |
| `collector_remote_write_pushes_total`, `collector_remote_write_errors_total`, `collector_remote_write_last_success_timestamp_seconds` | |

## Labels on push

`/metrics` has no `host` or `instance` label; the scraper adds them. Remote-write adds `remote_write.labels`, which defaults to `host = <host>`. A configured label replaces a sample label with the same name.
