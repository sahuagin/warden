# Warden Runbook

## Key Rule: Run warden binaries from the HOST, not the warden jail

The warden binary needs ZFS and jail(8) access which are only available on the host.
Build in the warden jail (`jexec warden`), run on the host.

Check with `hostname` before running — should be `threadripper.sahuagin.net`, not `warden`.

## Starting the warden jail

```sh
doas service jail start warden
```

etcd starts automatically via rc.d. Verify:

```sh
doas /usr/sbin/jexec warden /usr/local/bin/etcdctl endpoint health
```

## Building (from inside warden jail)

```sh
sudo jexec warden login -f tcovert
cd ~/src/public_github/warden
cargo build
```

## Testing the spawn lifecycle (from HOST)

```sh
./target/debug/warden-test-spawn <task_id> "<prompt>" <model_profile>
# model_profile: openrouter | minimax
# Example:
./target/debug/warden-test-spawn test-001 "say hello and report your hostname" minimax
```

## Checking task state in etcd (from warden jail)

```sh
etcdctl get /warden/tasks/<task_id>
etcdctl get --prefix /warden/tasks/   # list all tasks
```

## Inspecting a running worker jail

```sh
jls                                                              # list jails
doas /usr/sbin/jexec -U tcovert <jail_name> /bin/sh -c 'ps aux'  # check processes
```

## Cleaning up orphaned jails

If warden exits mid-task, jails and ZFS snapshots are left behind:

```sh
# Stop the jail
doas /usr/sbin/jail -f ~/.config/warden/jails/<jail_name>.conf -r <jail_name>

# Destroy the ZFS clone and snapshot
zfs destroy zroot/jails/<jail_name>
zfs destroy zroot/jails/warden@<jail_name>

# Remove the config file
rm ~/.config/warden/jails/<jail_name>.conf
```

## doas rules (host /usr/local/etc/doas.conf)

```
permit nopass tcovert as root cmd /usr/sbin/jail
permit nopass tcovert as root cmd /sbin/zfs
permit nopass tcovert as root cmd /usr/sbin/jexec
permit nopass tcovert as root cmd /bin/mkdir
permit nopass tcovert as root cmd /bin/sh
permit nopass tcovert as root cmd /usr/sbin/sysrc
```

## ZFS permissions (delegated, no doas needed)

```sh
zfs allow tcovert create,clone,destroy,mount,mountpoint,snapshot zroot/jails
```

Note: `zfs set mountpoint` still requires doas `/sbin/zfs` because it mounts the filesystem.

## MCP tools

- `spawn_agent` — queues a task and returns immediately with `"Task <id> queued"`. The jail lifecycle runs in a background tokio task. The warden process stays alive after the MCP pipe closes until all background tasks complete.
- `get_task` — reads task status from etcd. Poll this after spawn_agent.

## Known issues

- `cannot mount '/usr/jails/warden-<name>'` — harmless warning, appears before doas zfs sets correct mountpoint
- Orphaned jails if warden exits mid-task — clean up manually (see above)
- gemma-4-31b via openrouter is slow (~5-10min) — use minimax for dev testing
