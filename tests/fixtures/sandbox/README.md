# Container runtime shapes, for the sandbox tests

No test runs a real container runtime. `fake-runtime.rs` stands in for `docker` and `podman`, and
answers with these files, which hold what the real runtime prints:

| File | What prints it | Where its shape comes from |
|---|---|---|
| `info.txt` | `docker info`, the daemon running | the Docker CLI documentation, until recorded from a real run |
| `daemon-down.txt` | `docker info` on stderr, the daemon stopped | the Docker CLI documentation, until recorded from a real run |
| `no-such-image.txt` | `docker image inspect <name>` on stderr, no such image | the Docker CLI documentation, until recorded from a real run |
| `build-progress.txt` | `docker build --progress=plain` on stderr | the Docker CLI documentation, until recorded from a real run |
| `ps.jsonl` | `docker ps --format json`, a Compose service running | the Docker CLI documentation, until recorded from a real run |
| `network-subnets.txt` | `docker network inspect --format '{{range .IPAM.Config}}{{.Subnet}} {{end}}'` | the Docker CLI documentation, until recorded from a real run |
| `oom-status.txt` | the exit status of a container the out-of-memory killer ended | the Docker CLI documentation, until recorded from a real run |
| `context-show.txt` | `docker context show`, Docker Desktop's context current | the Docker CLI documentation, until recorded from a real run |
| `connection-default.txt` | `podman system connection list --format '{{if .Default}}{{.Name}}{{end}}'`, a Podman machine's two connections | the Podman documentation (no Podman output is recorded) |
