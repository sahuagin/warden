# FreeBSD deployment artifacts

Files here support running warden's dependencies as managed services
on FreeBSD. They are not built or used by `cargo`; they ship as plain
text and are installed by hand (or by your own deploy tooling).

## `rc.d/claude_proxy`

Service script for [claude-proxy](https://github.com/sahuagin/claude-proxy),
the OAuth-aware HTTP reverse proxy that warden's `anthropic-oauth` model
profile routes through. The proxy reads OAuth tokens from the running
user's `~/.claude*/.credentials.json` files and presents them to
`api.anthropic.com` with the required `anthropic-beta` header.

### Install

Build and stage the proxy binary first (per claude-proxy's own README),
then install the service script and enable it:

```sh
sudo install -m 555 -o root -g wheel freebsd/rc.d/claude_proxy /usr/local/etc/rc.d/claude_proxy

sudo sh -c 'cat >> /etc/rc.conf <<EOF
claude_proxy_enable="YES"
claude_proxy_user="<your-username>"
EOF'

sudo service claude_proxy start
sudo service claude_proxy status
```

`claude_proxy_user` is required — the script will refuse to start
without it. All other settings have defaults derived from that user;
override them in `/etc/rc.conf` if you need to (see the script header
for the full list).

### Verify

```sh
curl -sS http://127.0.0.1:3181/status | python3 -m json.tool
```

A healthy proxy returns both backends with `token_expired: false`
and an empty `active_faults` list. Logs go to `/var/log/claude_proxy.log`.

### Notes on design

- Runs as `${claude_proxy_user}` via `rc.subr`'s `su -m` wrapping, **not**
  `daemon -u`. Combining `daemon -u` with `daemon -r` (auto-restart)
  loops on `setusercontext()` after the first privilege drop and crashes
  the proxy on every restart.
- `HOME` is set explicitly in the `env(1)` invocation. Without it, the
  proxy resolves `~/.config/claude-proxy/config.toml` to
  `/.config/claude-proxy/config.toml` and exits.
- Pidfile lives at `/var/run/claude_proxy.pid`. On a stock FreeBSD,
  `/var/run` is `755 root:wheel`; the running user needs write access
  there. Either add the user to `wheel` and `chmod g+w /var/run`,
  or override `pidfile` in `/etc/rc.conf` to a path the user can write.
