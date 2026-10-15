# Changelog

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
