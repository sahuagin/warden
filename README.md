# warden

Ephemeral FreeBSD jail orchestration for AI agents.

## Architecture

- **ZFS clones** for instant copy-on-write agent sandboxes
- **FreeBSD jails** for OS-level isolation
- **etcd** for coordination, task queuing, and agent communication
- **MCP server** (Rust) as the interface between Claude Code and the jail/etcd layer
- **nullfs** read-only mounts for shared data access without copying

## Components

- `src/` — Rust MCP server (rmcp + etcd-client + tokio)
- `jails/` — jail template configuration
- `docs/` — architecture documentation

## Design Goals

- Orchestrator agent stays high-level; worker agents handle file edits and tool calls
- Each agent task runs in an isolated ZFS clone, destroyed on completion
- etcd watch API replaces polling; no sync coordination overhead
- Single-node etcd for local use, expandable to cluster for multi-machine
- No credentials in the repo — all config via environment variables
