# Changelog

## 0.1.0

First release.

- Host metrics from `/proc` and `/sys`, including hwmon temperatures and fans, with node_exporter names.
- Per-container metrics from cgroup v2 files and network namespaces, with cAdvisor names. Docker events re-inspect only the containers they name.
- NVIDIA GPU metrics from one long-running `nvidia-smi` process, with nvidia_gpu_exporter names.
- Container logs from Docker `json-file` files, shipped to VictoriaLogs or a local file with per-container cursors.
- Prometheus text at `/metrics`, optional remote-write push and a JSON API with optional bearer-token auth.
