# Running VIKRAM on a VM

A VM is the simplest place to run VIKRAM continuously: it has a real kernel, a writable
filesystem, and systemd, so the sandbox and the service units work without changes.

## Requirements

| Resource | Minimum | Comfortable |
|----------|---------|-------------|
| vCPU | 1 | 2 |
| RAM | 1 GB | 4 GB |
| Disk | 5 GB | 20 GB (state + workspaces) |
| OS | Debian 12 / Ubuntu 22.04+ | same, or any systemd distro |

The binary is statically linkable but the default release build links glibc. On a VM this
is the right build. `vikram` needs a C toolchain only if you build from source
(`rusqlite` is vendored with `bundled`).

## Install

```sh
curl -fsSL https://example.invalid/vikram-install.sh -o install.sh
sudo sh install.sh
```

`install.sh` detects the architecture, installs the binary to `/usr/local/bin`, creates the
`vikram` service user, installs `vikram.service` and `vikram-gateway.service`, and reloads
systemd. It does **not** start anything; that is a deliberate choice so a bad config cannot
take down a fresh box on first boot.

Then:

```sh
sudo -u vikram VIKRAM_HOME=/var/lib/vikram vikram --version
sudo systemctl start vikram
sudo systemctl status vikram
```

State lives in `/var/lib/vikram` (`StateDirectory=vikram` in the unit). Back up that
directory; nothing else on the box holds state.

## Do not use the TUI as the service command

`vikram` with no subcommand launches the TUI, which refuses a non-TTY
(`crates/vikram-cli/src/terminal.rs:66-69`) and exits immediately under systemd. The unit
runs `vikram daemon start --foreground` instead. Use `vikram chat "<prompt>"` for one-shot
headless work and `ssh -t` for an interactive session.

## cloud-init

Works on Debian 12, Ubuntu 22.04+ and any image that ships cloud-init.

```yaml
#cloud-config
package_update: true
packages: [ca-certificates, curl]

users:
  - name: vikram
    system: true
    shell: /usr/sbin/nologin
    home: /var/lib/vikram

write_files:
  - path: /etc/vikram/gateway.env
    permissions: '0600'
    content: |
      # Only set this if you actually want an inbound port.
      # GATEWAY_WEBHOOK_BIND=0.0.0.0:8080
      OPENAI_API_KEY=REPLACE_ME

runcmd:
  - mkdir -p /opt/vikram /var/lib/vikram
  - curl -fsSL https://example.invalid/vikram-x86_64-unknown-linux-gnu.tar.gz | tar -xz -C /opt/vikram
  - install -m 0755 /opt/vikram/vikram /usr/local/bin/vikram
  - install -m 0644 /opt/vikram/packaging/systemd/vikram.service /etc/systemd/system/vikram.service
  - install -m 0644 /opt/vikram/packaging/systemd/vikram.tmpfiles /etc/tmpfiles.d/vikram.conf
  - systemd-tmpfiles --create
  - systemctl daemon-reload
  - systemctl enable --now vikram
```

Two things to change before you use this: the release URL, and the API key. Put the real
key in the VM's secret store, not in `user-data`, which is readable from the instance
metadata service.

## Generic VM without cloud-init

1. Copy `vikram` to `/usr/local/bin` and `chmod 0755`.
2. `useradd --system --home /var/lib/vikram --shell /usr/sbin/nologin vikram`
3. Install the units from `packaging/systemd/`, then `systemd-tmpfiles --create` and
   `systemctl daemon-reload`.
4. `systemctl enable --now vikram`.

If your provider's image has no systemd (Alpine, distroless), run
`vikram daemon start --foreground` under whatever supervisor the image uses and give it
`VIKRAM_HOME` on a writable volume.

## Network posture

The daemon opens no listening socket. It makes outbound HTTPS calls to the model provider
and to any configured gateway platform. Nothing needs to be exposed on the VM's firewall
unless you set `GATEWAY_WEBHOOK_BIND` and run `vikram gateway`, in which case expose only
that port and put a TLS terminator in front of it.

## Sandbox on a VM

Inside a VM the kernel is real, so the sandbox works. On Linux the default path is
bubblewrap when installed, otherwise a pass-through recorded as such:

```sh
sudo apt-get install -y bubblewrap
vikram doctor          # reports the active sandbox mechanism
```

`vikram doctor` is the honest way to check: it names the mechanism in use rather than
claiming enforcement.

## Snapshots and migration

Take the VM snapshot with the daemon stopped (`systemctl stop vikram`) so SQLite is not
mid-write; WAL recovery handles the rest. Restoring onto a new host works because state is
a single directory, and the daemon lock records the hostname — a lock copied from another
machine is treated as stale and taken over.
