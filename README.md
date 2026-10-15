# collector

Watch a Docker host's CPU, memory, disks, network, GPUs, containers and logs with one small binary instead of four agents.

A typical Docker host runs node_exporter for host metrics, cAdvisor for container metrics, a GPU exporter and a log shipper such as Alloy or Promtail. Each is a separate process with its own polling, and cAdvisor alone can use a full CPU core on a busy host. The collector reads the same numbers straight from `/proc`, `/sys` and cgroup v2 files on a timer, asks Docker only for the container list and its events, and keeps `nvidia-smi` running for GPU readings. It serves everything as Prometheus metrics, the text format most monitoring tools scrape, and ships container logs to VictoriaLogs, a log database, or to a local file.

Host and GPU metric names match node_exporter and the NVIDIA GPU exporter. Container metrics use cAdvisor's names with clean labels (`container`, `container_id`, `compose_project`, `compose_service`); scrape `/metrics?compat=cadvisor` to get cAdvisor's exact names and labels for existing dashboards and alerts. It also reports what cAdvisor can't: restarts, exit codes, OOM kills of the main process, health checks and per-container GPU memory. [docs/metrics.md](docs/metrics.md) lists what matches, what differs and what's missing.

It keeps no history: each response is the latest collection, and log cursors are the only state on disk.

## Run it

```yaml
services:
  collector:
    image: ghcr.io/joeyyax/collector:latest
    restart: unless-stopped
    read_only: true
    cap_drop: [ALL]
    security_opt: [no-new-privileges:true]
    stop_grace_period: 20s
    environment:
      COLLECTOR_HOST: web-1
      COLLECTOR_API__TOKEN_FILE: /run/secrets/collector-token
      COLLECTOR_LOGS__ENABLED: "true"
      COLLECTOR_LOGS__URL: http://victorialogs:9428
    ports:
      - "127.0.0.1:9273:9273"
    volumes:
      - /proc:/host/proc:ro
      - /sys:/host/sys:ro
      - /:/rootfs:ro,rslave
      - /var/run/docker.sock:/var/run/docker.sock:ro
      - /var/lib/docker/containers:/var/lib/docker/containers:ro
      - collector-state:/var/lib/collector
    secrets: [collector-token]

secrets:
  collector-token:
    file: ./collector-token

volumes:
  collector-state:
```

[deploy/compose.yaml](deploy/compose.yaml) has the full snippet, including the GPU settings. Every mount is read-only except the state volume, which holds log cursors.

| Mount | Used for |
|---|---|
| `/proc` → `/host/proc` | Host CPU, memory, disks, network; container network namespaces via `/proc/<pid>/net/dev` |
| `/sys` → `/host/sys` | cgroup v2 files, hwmon sensors, block device names |
| `/` → `/rootfs` | Filesystem usage (`statvfs`) and the host name |
| Docker socket | Container list, labels and events (`GET` only) |
| `/var/lib/docker/containers` | Container log files |

GPU hosts need the NVIDIA runtime so `nvidia-smi` is injected: `runtime: nvidia` with `NVIDIA_VISIBLE_DEVICES=all` and `NVIDIA_DRIVER_CAPABILITIES=utility`. The image uses a glibc distroless base for this; the binary itself is static.

Per-container GPU memory also needs `pid: host`. `nvidia-smi` lists only the processes in its own PID namespace, so without it every container reports no GPU use and the collector logs why at startup. It also lets the collector see, and as root signal, every process on the host, so turn it on only where per-container GPU numbers are worth that.

Writable-layer sizes (`containers.writable_layer`, off by default) need `cap_add: [DAC_READ_SEARCH]` to see inside directories containers keep private; see [docs/api.md](docs/api.md#writable-layer).

The image's `HEALTHCHECK` runs `collector health`.

Without Docker: `collector --config collector.toml` with the default paths (`/proc`, `/sys`, `/`).

## Configure

One TOML file, every key optional. [collector.example.toml](collector.example.toml) lists them all with defaults.

Environment variables override the file: `COLLECTOR_<KEY>` for top-level keys and `COLLECTOR_<SECTION>__<KEY>` (double underscore) for sections. Values parse as TOML, so lists work:

```sh
COLLECTOR_INTERVAL=30s
COLLECTOR_LOGS__ENABLED=true
COLLECTOR_LOGS__EXCLUDE_NAMES='["^backup-", "^tmp-"]'
COLLECTOR_REMOTE_WRITE__URL=http://victoriametrics:8428/api/v1/write
```

`COLLECTOR_CONFIG` names the file. `collector --once` prints one collection in Prometheus format and exits.

Docker labels are dropped unless listed. `containers.labels` keeps them on metrics, logs and the API. `containers.api_labels` keeps them on logs and the API only, which suits per-deploy values that would multiply metric series. Both take exact keys, `*` or a trailing `.` for a prefix:

```toml
[containers]
labels = ["com.docker.compose.project", "com.docker.compose.service"]
api_labels = ["com.example."]
```

## HTTP API

The binary listens on `127.0.0.1:9273` by default (`listen`). The image sets `0.0.0.0:9273` because loopback inside a container is unreachable; publish the port on `127.0.0.1` or a private address.

| Endpoint | Auth | Returns |
|---|---|---|
| `GET /metrics` | token | Prometheus text format, gzip when the client accepts it. `?compat=cadvisor` for cAdvisor's names |
| `GET /health` | none | `{"status":"ok","version":...}`; 503 when the last collection is older than three intervals plus 5 s |
| `GET /api/v1/snapshot` | token | Host, GPUs, running and recently stopped containers from one collection |
| `GET /api/v1/stream` | token | The snapshot as server-sent events; collection runs every `live_interval` (5 s) while anyone follows |
| `GET /api/v1/host` | token | Host snapshot with derived rates |
| `GET /api/v1/containers` | token | Every running container with its latest readings |
| `GET /api/v1/containers/:id` | token | One container by full ID, name or unique ID prefix of four or more characters |
| `GET /api/v1/gpus` | token | GPUs with the containers holding memory on each |

Container lists, the snapshot and the stream take `sort` (`cpu`, `memory`, `net_rx`, `net_tx`, `disk_read`, `disk_write`, `name`) and `limit`, so `/api/v1/containers?sort=cpu&limit=10` is the ten busiest.

With `api.token` or `api.token_file` set, requests need `Authorization: Bearer <token>`; without one the API is open. Requests are served by `api.workers` threads (4), so a slow client can't hold up a scrape; each stream gets its own thread, up to 16. Shapes and examples: [docs/api.md](docs/api.md).

## Command line

`collector health`, `host`, `containers` and `gpus` call the matching route and print a table, or the raw response with `--json`:

```sh
$ collector containers --sort cpu --limit 3
NAME        STATE    HEALTH   CPU   MEMORY     LIMIT      NET RX      NET TX     READ   WRITE       RESTARTS
shop-web    running  healthy  1.42  611.3 MiB  1.0 GiB    1.2 MiB/s   3.4 MiB/s  0 B/s  12.0 KiB/s  0
shop-redis  running  -        0.01  114.4 MiB  512.0 MiB  14.4 KiB/s  4.3 KiB/s  0 B/s  0 B/s       2
```

They ask the local collector on the port from `listen`, with the token from the config. `--url` and `--token-file` point them at another one. `collector health` exits 1 when the collector is stale or unreachable.

## Logs

| Log driver | How it's read |
|---|---|
| `json-file` (Docker's default) | The files under `/var/lib/docker/containers/<id>/`, rotations included. Exact resume by inode and byte offset |
| `local`, `journald` | The Docker logs API, one follower thread per container. Resume by timestamp |
| anything else (`syslog`, `gelf`, `fluentd`, `none`, ...) | Not readable. One warning per container at startup; counted in `collector_log_containers_unsupported` |

- **Cursors** live in `/var/lib/collector/log-cursors.json` and are saved only after the sink accepts a batch. Restarts, including `kill -9`, neither repeat nor drop lines.
- **Long lines.** Docker splits lines over 16 KB into pieces; they're joined back into one record, up to 1 MiB.
- **Multi-line records** are off by default. `logs.multiline.mode`:
  - `indent`: a line starting with a space or tab continues the previous one, which covers most stack traces.
  - `continue`: lines matching `pattern` continue the previous one.
  - `start`: lines matching `pattern` start a record, for example `^\d{4}-\d{2}-\d{2}`.
  - A group is sent when the next record starts, after `timeout` (2 s) without a new line, or at `max_lines`/`max_bytes`.
- **Backpressure.** One batch is in flight at a time (1 MiB or 5,000 records, gzipped on the wire). When the sink is down, that batch is retried with backoff and nothing else is read; the log files on disk are the buffer. API followers block on a bounded queue (`logs.api_queue_lines`).
- **Shutdown.** SIGTERM stops reading, sends pending multi-line groups, waits up to `shutdown_timeout` (10 s) for the last batch and saves cursors. Give the container a longer stop timeout than that (`stop_grace_period: 20s`).
- **Skipped**: containers labeled `collector.logs=false` (`logs.exclude_label`), names matching `logs.exclude_names` and the collector itself.
- **First run**: existing history is skipped (`logs.first_run = "end"`). Containers that start later are read from the beginning.
- **Fields**: `_time`, `_msg`, `stream`, `host`, `container`, `container_id`, `image`, `compose_project`, `compose_service`, the labels kept by `containers.labels` and `containers.api_labels`, and `level` when the line is JSON with a recognizable level. `compose_project` and `compose_service` come from the Compose labels whether or not those lists keep them. A label never replaces one of these fields. Stream fields default to `host`, `container`, `compose_project`, `compose_service`.
- **Tenants**: with `logs.tenant_label` set, each container's logs go to the VictoriaLogs tenant (`AccountID`) in that Docker label. Each flush sends one request per tenant; unlabeled containers and non-numeric values go to tenant 0. Without it, everything goes to tenant 0 unless `logs.headers` sets an `AccountID`.

Lines lost to Docker's own rotation, when a file rotates out of `max-file` before it's read, are counted in `collector_log_rotations_missed_total`.

## Remote write

Remote write uses the clean metric names; set `metrics.compat = "cadvisor"` to push cAdvisor's. Each collection is encoded once (protobuf, snappy as the protocol requires) and queued. Failed pushes are retried in order with backoff up to `remote_write.max_backoff`; the queue holds up to `queue_bytes` (64 MiB; a push from a 20-container host is about 70 KB, so roughly seven hours) and drops the oldest first, counting them in `collector_remote_write_dropped_samples_total`. On SIGTERM the queue gets `shutdown_timeout` to drain.

## Build and release

Only Docker with buildx is needed; the Rust toolchain runs in a container.

```sh
make test lint          # unit tests, clippy with warnings as errors
make binaries           # dist/collector-<version>-linux-{amd64,arm64} + SHA256SUMS
make image              # multi-arch OCI archive in dist/
make image-push         # push $(IMAGE):<version> and :latest
```

The version comes from `git describe` (override with `VERSION=1.2.3`) and shows in `collector --version`, `/health` and `collector_build_info`. `.github/workflows/build.yml` runs the same targets but nothing depends on it.

Feature flags, all on by default: `gpu`, `tls` (rustls for HTTPS sinks) and `jemalloc`. Without jemalloc the binary uses musl's allocator, whose RSS settles several times higher.

## Versioning

Releases are tagged `vX.Y.Z` and listed in [CHANGELOG.md](CHANGELOG.md). Images are tagged with the version and `latest`.

## License

MIT. See [LICENSE](LICENSE).
