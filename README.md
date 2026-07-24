# Cirrusync

Cirrusync is a small, auditable Cloudflare Dynamic DNS client. It discovers the
machine's public IPv4 and/or IPv6 address, compares it with one or more
Cloudflare DNS records, and changes only records that are out of date. It can
run continuously under systemd or perform a single synchronization for tests
and cron-style use.

The display name is **Cirrusync** and the executable, package, service account,
and systemd service use the lowercase name `cirrusync`. “Cirrus” evokes clouds
without reusing the heavily overloaded `cfddns` name, while “sync” describes
the actual behavior. The canonical repository is
[`wiresock/cirrusync`](https://github.com/wiresock/cirrusync). A fork can
rename the project by changing the package/binary name, installer constants,
systemd unit and paths; there is no persisted database or protocol bearing the
name. Perform the appropriate repository-name and trademark checks before
publishing a fork.

> [!IMPORTANT]
> Dynamic DNS only updates DNS. It does not bypass carrier-grade NAT (CGNAT),
> ISP filtering, a router firewall, or missing port forwarding. The detected
> public address must be assigned or routed to your connection. If your ISP
> shares an IPv4 address through CGNAT, unsolicited inbound IPv4 connections
> generally cannot reach your router even when DNS is correct.

## Features

- Stable Rust with a conservative dependency set and rustls-based HTTPS.
- Cloudflare API v4 bearer-token authentication; Global API Keys are neither
  needed nor supported.
- Exact zone and DNS record lookup without manually copying record IDs.
- Multiple A and AAAA records across multiple zones, with per-process zone-ID
  caching.
- Ordered public-IP providers, strict address validation, and provider
  fallback.
- Updates only when managed content, TTL, or proxy state differs; optional
  creation of missing records.
- Immediate startup cycle, graceful SIGINT/SIGTERM shutdown, and bounded retry
  with exponential backoff and jitter.
- Record-scoped failures are aggregated so one record does not prevent
  unrelated records from being attempted. Global authentication and rate-limit
  failures stop the remaining API work in that cycle.
- Human-readable structured logs suitable for journald, without tokens or
  authorization headers.
- A defensive, idempotent installer and a sandboxed, unprivileged systemd
  service.

Cirrusync targets Debian 12, Ubuntu 22.04, Ubuntu 24.04, and reasonably
compatible Debian-based distributions on `amd64` and `arm64`. The Rust daemon
itself is portable; the bootstrap and service integration are Linux-specific.

## How it works

| Component | Responsibility |
| --- | --- |
| `config` | Loads TOML and rejects unsafe or contradictory definitions. |
| `public_ip` | Tries configured HTTPS providers in order and accepts only a public address of the requested family. |
| `cloudflare` | Authenticates, performs pagination-safe exact lookups, and creates or updates records through API v4. |
| `updater` | Runs one non-overlapping discovery/compare/update cycle and aggregates per-record results. |
| `daemon` | Schedules cycles, applies bounded retry/backoff, and handles shutdown signals. |
| `bootstrap.sh` | Builds in an unprivileged context, validates configuration, installs atomically, and manages systemd. |

Cloudflare remains the authoritative state. Cirrusync does not rely on a local
address cache after restart.

## Cloudflare preparation

### 1. Create a restricted API token

For an unattended service, prefer an account-owned token because it acts as a
service principal and is not tied to an individual user. Open **Manage Account
→ Account API Tokens**. A user-owned token from **My Profile → API Tokens** is
also supported. Choose **Create Token** and use the **Edit zone DNS** template
or a custom token.

Configure the token policy as follows:

| Dashboard setting | Required value |
| --- | --- |
| Zone resource | **Specified Domains**, limited to the exact zone Cirrusync manages |
| **DNS** permission | **Edit**; the review summary may call this **DNS Write** |
| **Zone** permission | **Read** |
| Other permissions | Leave unchecked |

Do not grant Zone Edit, account-wide administration, or unrelated permissions.
For multiple managed zones, include only those zones.

Choose the optional restrictions according to how the service will be
operated:

- **Expiration:** a finite lifetime limits exposure but stops updates when the
  token expires. Prefer an expiry when you have a reliable reminder or
  automated rotation process. **No expiration** avoids an unplanned DDNS outage
  but requires deliberate periodic rotation and prompt revocation after any
  suspected exposure.
- **Client IP address filtering:** normally leave this as **All IP addresses
  allowed** for a DDNS client. Restricting the token to today's dynamic WAN
  address will lock the client out after that address changes. Use a restriction
  only when API calls leave through a separate, stable egress CIDR; include both
  address families if the host may reach Cloudflare over IPv4 and IPv6. An
  `Allow 0.0.0.0/0` rule is effectively unrestricted for IPv4 and, by itself,
  does not cover IPv6.

Before creating the token, the summary should show the exact domain, **DNS
Write**, **Zone Read**, the intended expiration, and the intended IP policy.
Cloudflare displays the token string only when it is created, so copy it into a
password manager or another secure secret store. Do not put it in the
configuration file, shell history, an issue, or a log.

A successful token-verification request proves that the token is active. It
does not prove that the token includes the configured zone, Zone Read, or DNS
Edit; the installer validates those separately.

For an account-owned token (new tokens start with `cfat_`), also copy the
account's 32-character **Account ID**. The installer asks for it so Cirrusync
can use Cloudflare's
[account-token verification endpoint](https://developers.cloudflare.com/api/resources/accounts/subresources/tokens/methods/verify/).
User-owned `cfut_` tokens omit the Account ID. Global API Keys are not
supported.

#### Zone name, Account ID, and Zone ID

The installer's **Cloudflare zone** prompt expects the DNS zone name, not an
opaque ID. For example, when updating `home.example.com`, the zone is normally
`example.com`. Find it in the Cloudflare dashboard's account domain list or by
opening the domain's Overview page.

For an account-owned token, copy the Account ID from the Cloudflare **Account
home** menu using **Copy account ID**. Depending on the dashboard layout, it is
also shown in the account Overview page's **API** section. The Account ID is an
identifier rather than a secret, but it must be the 32-character hexadecimal ID
of the account that owns the token.

The interactive installer does not require a Zone ID. Advanced manual
configurations may set `zone_id` to the zone's 32-character identifier; it is
available alongside the Account ID in Cloudflare's **API** section.

See Cloudflare's official [account-token
guide](https://developers.cloudflare.com/fundamentals/api/get-started/account-owned-tokens/),
[API token
guide](https://developers.cloudflare.com/fundamentals/api/get-started/create-token/),
[token restriction
guide](https://developers.cloudflare.com/fundamentals/api/how-to/restrict-tokens/),
and [Account and Zone ID
instructions](https://developers.cloudflare.com/fundamentals/account/find-account-and-zone-ids/)
for the current dashboard flow.

### 2. Create the initial DNS record

In **DNS → Records**, add an A record for IPv4 and/or an AAAA record for IPv6.
The initial address may be the connection's current public address. Keep
`create_if_missing = false` when you want a missing record to be treated as an
error; set it to `true` if Cirrusync may create the configured record.

Use **DNS only** (gray cloud, `proxied = false`) for SSH, WireGuard, other VPNs,
game servers, and arbitrary TCP/UDP services. Cloudflare's orange-cloud proxy
publishes Cloudflare anycast addresses and proxies supported web traffic; it
does not transparently forward arbitrary protocols. Enable `proxied = true`
only when the hostname intentionally fronts a Cloudflare-supported service.
Cloudflare documents the distinction in
[Proxy status](https://developers.cloudflare.com/dns/proxy-status/).

## Installation

Downloading and reviewing a privileged installer is safer than piping it
directly to a shell:

```console
curl -fsSLo bootstrap.sh \
  https://raw.githubusercontent.com/wiresock/cirrusync/main/bootstrap.sh
less bootstrap.sh
sudo bash bootstrap.sh
```

The compact one-command form is:

```console
curl -fsSL https://raw.githubusercontent.com/wiresock/cirrusync/main/bootstrap.sh \
  | sudo bash
```

The interactive prompts map to these values:

| Prompt | What to enter |
| --- | --- |
| Cloudflare zone | The zone name, such as `example.com` |
| Cloudflare account ID | The owning Account ID for a `cfat_` token; leave blank for a `cfut_` user token |
| DNS record name | The full hostname, such as `home.example.com` |
| Enable IPv4 updates | `Y` when managing an A record |
| Enable IPv6 updates | `Y` only when the host has working globally routed IPv6 and an AAAA record is wanted |
| Update interval | `300` seconds is a conservative default |
| Allow creation of a missing record | `Y` only when Cirrusync may create the configured A/AAAA record |
| Enable Cloudflare proxying | Normally `N` for VPN, SSH, game, or arbitrary TCP/UDP endpoints |
| Cloudflare API token | Paste the complete token and press Enter |

Token input is intentionally hidden: nothing appears while pasting or typing,
and the token is not printed afterward. A blank-looking token prompt is
therefore expected.

The installer then:

1. verifies the operating system and architecture;
2. installs `curl`, Git, CA certificates, build tools, `pkg-config`, `procps`,
   `util-linux`, and the ACL inspection tools;
3. uses a suitable trusted system Rust toolchain or installs a
   checksum-verified managed toolchain without changing shell profiles;
4. clones the selected branch into a staged source tree as the isolated build
   account with Git hooks, external protocols, redirects, and ambient Git
   configuration disabled;
5. verifies that an existing token is inaccessible to the build account, then
   builds and tests `--locked` as the dedicated, non-login
   `cirrusync-build` account with privilege gain disabled;
6. starts a rollback transaction, creates or validates the tightly constrained
   service identity, and stages the binary and configuration;
7. verifies the repository unit against the installer-pinned SHA-256, runs
   `systemd-analyze verify`, and runs a Cloudflare edit-capability check as the
   `cirrusync` account;
8. starts the hardened unit, requires five continuously healthy samples with
   no restart, and only then promotes the tested source and commits.

If any transactional step fails, the prior binary, configuration, token, unit,
source tree, directory metadata, service enablement, and active state are
restored where applicable. The persistent build identity and managed Rust
toolchain may remain for future upgrades. No installer can make remote source
risk-free: review the script, repository, selected branch, and ideally the
target commit before installation. The installer accepts credential-free
HTTPS repository URLs only and never installs a unit whose checksum differs
from its audited built-in value.

On a failed fresh installation, the newly staged configuration and token are
removed during rollback. It is therefore normal for `/etc/cirrusync/token` not
to exist after a validation failure; do not recreate it manually. Correct the
input and rerun the installer. An update or reconfiguration failure instead
restores the last-known-good installed files and service state.

### Confirm a successful installation

A successful installation leaves `cirrusync.service` enabled and active. The
first cycle may immediately update Cloudflare when the discovered public
address differs from the DNS record:

```console
sudo systemctl is-enabled cirrusync
sudo systemctl is-active cirrusync
sudo journalctl -u cirrusync -n 50 --no-pager
```

The journal reports either **DNS record updated** or an unchanged cycle
summary; it never prints the token. Allow for the configured DNS TTL before
concluding that a resolver still has the old address.

The complete account-owned-token path has been exercised by the privileged CI
installer lifecycle and confirmed in a live installation: configuration
validation completed, the hardened systemd service started successfully, and
an existing Cloudflare DNS record was updated to the discovered public address.

### Non-interactive installation

Prefer a root-readable token source file over an environment variable. This
example uses an account-owned token; omit `CIRRUSYNC_ACCOUNT_ID` for a
user-owned token:

```console
sudo install -o root -g root -m 0600 /path/to/downloaded-token \
  /root/cirrusync-token
sudo CIRRUSYNC_ZONE=example.com \
  CIRRUSYNC_ACCOUNT_ID=0123456789abcdef0123456789abcdef \
  CIRRUSYNC_RECORD=home.example.com \
  CIRRUSYNC_ENABLE_IPV4=true \
  CIRRUSYNC_ENABLE_IPV6=false \
  CIRRUSYNC_INTERVAL=300 \
  bash bootstrap.sh \
    --token-file /root/cirrusync-token \
    --non-interactive
```

`CIRRUSYNC_ACCOUNT_ID` is required for account-owned tokens and must be omitted
for user-owned tokens. Optional variables are `CIRRUSYNC_CREATE` and
`CIRRUSYNC_PROXIED`, both defaulting to `false`. `CFDDNS_*` aliases are accepted
to ease migration from generic installer examples.
`CIRRUSYNC_TOKEN`/`CFDDNS_TOKEN` are supported, but environment secrets may be
inherited by child processes, retained in automation configuration, or visible
to sufficiently privileged process inspection. They are not printed by the
installer.

An ordinary non-interactive rerun selects update mode and preserves the
existing configuration and token. To replace configuration from automation,
use `--reconfigure --non-interactive` and provide at least
`CIRRUSYNC_ZONE` and `CIRRUSYNC_RECORD`; other omitted values use their safe
defaults. An existing token is retained unless an explicit token file or
token environment value is supplied. Interactive reconfiguration separately
confirms replacement of the configuration and token.

Other installer options:

```text
--repo URL
--branch BRANCH
--config /absolute/path/config.toml
--token-file /absolute/path/token-source
--non-interactive
--skip-tests
--update
--reconfigure
--uninstall
--help
```

`--skip-tests` weakens the installation gate and should be reserved for a
diagnosed test-environment problem. `--token-file` names an input file; the
installed token remains next to the target configuration. Its source must be a
single-link, root-owned file with mode `0400` or `0600`, under an absolute,
root-owned directory chain that is not writable by group or other users (for
example `/root/cirrusync-token`). `--config` is intentionally limited to a
direct child of `/etc/cirrusync`, and `--repo` accepts only a credential-free
HTTPS URL.

## Configuration

The default configuration is `/etc/cirrusync/config.toml`. A complete template
is in [`config.example.toml`](config.example.toml):

```toml
interval_seconds = 300
request_timeout_seconds = 15

[cloudflare]
api_token_file = "/etc/cirrusync/token"
# Required for a cfat_ account-owned token; omit for a user-owned token.
# account_id = "0123456789abcdef0123456789abcdef"

[ipv4]
enabled = true
providers = [
    "https://api.cloudflare.com/cdn-cgi/trace",
    "https://api.ipify.org",
]

[ipv6]
enabled = false
providers = ["https://api6.ipify.org"]

[[records]]
zone = "example.com"
# zone_id = "0123456789abcdef0123456789abcdef"
name = "home.example.com"
type = "A"
ttl = 120
proxied = false
create_if_missing = false
```

Add one `[[records]]` table per record. A records require IPv4 discovery and
AAAA records require IPv6 discovery. Record names must belong to their zone,
and duplicate type/name definitions are rejected. `zone_id` is optional;
when supplied, Cirrusync verifies that it identifies the configured active
zone. Without it, Cirrusync requires one unambiguous active exact-name match
and caches the ID for the process lifetime.

`cloudflare.account_id` selects Cloudflare's account-token verification
endpoint. It is required for `cfat_` account-owned tokens and for legacy
unprefixed account tokens, and must be omitted for `cfut_` user-owned tokens.
It is an identifier rather than a secret, but it must contain exactly 32
hexadecimal characters.

`cloudflare.api_token_file` must be absolute. On Unix, Cirrusync accepts a
root-owned regular file with owner-read permission and no permissions beyond
`0640` (for example `0400` or `0600`). On Linux, the installer-managed `0640`
form is also accepted when the file belongs to the process's effective service
group and has no extended POSIX access ACL. Other Unix targets accept only
owner-only token files because their ACLs are not inspected. Symbolic links and
path-replacement races are rejected. Configuration files must be owned by root
or by the current effective user, which preserves user-owned development setups
without trusting a third party's mutable file. For local development,
`CIRRUSYNC_TOKEN` overrides the token file, but do not place that environment
variable in a production systemd unit.

Cloudflare requires proxied records to use automatic TTL. Express that as
`ttl = 1`; configuration with `proxied = true` and another TTL is rejected.
Use `proxied = false` and an explicit TTL for services that Cloudflare does not
proxy.

Keep `create_if_missing = false` unless Cirrusync has exclusive ownership of
record creation for that name and type. Cloudflare permits multiple A or AAAA
records with the same name, and its API has no conditional-create operation.
Cirrusync rechecks immediately before creating and refuses ambiguous matches,
but a simultaneous dashboard or API create can still race the final request.
If that happens, remove the unintended duplicate in Cloudflare before resuming
synchronization.

Providers return either a plain IP address or Cloudflare trace text containing
an `ip=` line. They are tried in order. Private, loopback, unspecified,
multicast, link-local, and documentation-only addresses are rejected rather
than cached. Only direct HTTPS providers are accepted: Cirrusync does not
follow redirects and deliberately ignores ambient HTTP proxy settings so it
cannot publish a proxy's address. Keep explicit providers you trust.

To keep failure latency and Cloudflare API use bounded, the polling interval
must be 60–86,400 seconds, at most 50 records and eight providers per address
family may be configured, the request timeout must be 1–120 seconds, and the
TOML file is limited to 256 KiB. Each family's provider fallbacks share one
configured discovery deadline rather than receiving a fresh timeout per
provider. A complete `check` has its own five-minute upper bound independent
of the daemon's polling interval.

The installer uses:

```text
/etc/cirrusync/                 root:cirrusync 0750
/etc/cirrusync/config.toml      root:cirrusync 0640
/etc/cirrusync/token            root:cirrusync 0640
/usr/local/bin/cirrusync        root:root       0755
/var/lib/cirrusync              cirrusync:cirrusync 0750
```

A root-owned token with mode `0600` would be unreadable by the unprivileged
daemon. Mode `0640` plus the dedicated `cirrusync` group gives only root and
the service access. Do not add interactive users to that group. After manual
changes:

```console
sudo chown root:cirrusync /etc/cirrusync/config.toml /etc/cirrusync/token
sudo chmod 0640 /etc/cirrusync/config.toml /etc/cirrusync/token
sudo chmod 0750 /etc/cirrusync
```

## Command-line use

All commands accept the global configuration option:

```console
sudo -u cirrusync -- /usr/local/bin/cirrusync \
  --config /etc/cirrusync/config.toml check
sudo -u cirrusync -- /usr/local/bin/cirrusync \
  --config /etc/cirrusync/config.toml once
sudo -u cirrusync -- /usr/local/bin/cirrusync \
  --config /etc/cirrusync/config.toml check --allow-edit-probe
sudo -u cirrusync -- /usr/local/bin/cirrusync \
  --config /etc/cirrusync/config.toml check --allow-create
cirrusync print-config
```

Installed commands that mutate DNS (`once`, `run`, `check --allow-create`, or
`check --allow-edit-probe`) must run as the dedicated service account and
require the systemd service to be stopped. Cross-process, per-record locks
reject another updater managing any overlapping DNS resource. A plain
read-only `check` may run while the service is active. On a development host
without the installed `/var/lib/cirrusync` state directory, a development-owned
configuration and `CIRRUSYNC_TOKEN` can be used directly as your development
user.

- `run` performs a cycle immediately, continues at the configured interval,
  retries transient failures with bounded backoff, and stops gracefully.
- `once` performs one synchronization cycle. It exits zero when every record
  is already correct or was updated/created, and nonzero on failure.
- `check` validates syntax, token access, authentication, zones, record
  existence or creation eligibility, DNS read access, and public-address
  discovery.
  Read-only validation deliberately does not claim that DNS Edit was proved,
  and therefore does not return full success. `check --allow-edit-probe`
  explicitly PATCHes one existing record per configured zone with its current
  type, name, and TTL while omitting address and proxy fields.
  `check --allow-create` may create configured missing records
  where `create_if_missing = true`; combine both flags when either state is
  possible.
- `print-config` prints an example, never the active configuration or token.

An edit probe is intended to preserve DNS resolution, but Cloudflare does not
document a conditional-write precondition for this endpoint. A simultaneous
dashboard or API edit can therefore race the probe; in particular, a
concurrent name or TTL change may be replaced by the probed value. The probe
never resends a cached address or proxy setting. Use it only when that explicit
write is acceptable and avoid other DNS edits during the check. Cloudflare may
also update metadata or audit timestamps. A successful creation proves DNS
Edit for that zone too.

`--verbose` (repeatable) and `--quiet` adjust command-line log detail, and
`CIRRUSYNC_CONFIG` can provide the configuration path. Set `RUST_LOG` for
detailed service diagnostics without editing the unit:

```console
sudo systemctl edit cirrusync
```

```ini
[Service]
Environment=RUST_LOG=cirrusync=debug
```

Then run `sudo systemctl daemon-reload && sudo systemctl restart cirrusync`.
Debug logs are designed to remain secret-safe, but inspect them before sharing.

## Manual build and installation

Stable Rust 1.85 or newer is required because the crate uses Rust edition
2024:

```console
git clone https://github.com/wiresock/cirrusync.git
cd cirrusync
cargo test --all-targets --all-features --locked
cargo build --release --locked
sudo install -o root -g root -m 0755 target/release/cirrusync \
  /usr/local/bin/cirrusync
```

Create the account and directories:

```console
sudo groupadd --system cirrusync
sudo useradd --system --gid cirrusync --home-dir /var/lib/cirrusync \
  --shell /usr/sbin/nologin cirrusync
sudo install -d -o root -g cirrusync -m 0750 /etc/cirrusync
sudo install -d -o cirrusync -g cirrusync -m 0750 /var/lib/cirrusync
sudo install -o root -g cirrusync -m 0640 config.example.toml \
  /etc/cirrusync/config.toml
sudoedit /etc/cirrusync/config.toml
sudo install -o root -g cirrusync -m 0640 /path/to/token \
  /etc/cirrusync/token
sudo -u cirrusync /usr/local/bin/cirrusync \
  --config /etc/cirrusync/config.toml check --allow-edit-probe --allow-create
```

Only after `check` succeeds, install and start the unit:

```console
sudo install -o root -g root -m 0644 systemd/cirrusync.service \
  /etc/systemd/system/cirrusync.service
sudo systemctl daemon-reload
sudo systemctl enable --now cirrusync.service
```

`groupadd` and `useradd` report an error when the account already exists; omit
those two commands on a repeat manual install.

## Service operation

```console
sudo systemctl status cirrusync
sudo journalctl -u cirrusync -f
sudo systemctl restart cirrusync
sudo systemctl stop cirrusync
sudo systemctl start cirrusync
sudo -u cirrusync -- /usr/local/bin/cirrusync \
  --config /etc/cirrusync/config.toml check
sudo systemctl stop cirrusync
sudo -u cirrusync -- /usr/local/bin/cirrusync \
  --config /etc/cirrusync/config.toml once
sudo -u cirrusync -- /usr/local/bin/cirrusync \
  --config /etc/cirrusync/config.toml \
  check --allow-edit-probe --allow-create
sudo systemctl start cirrusync
```

The unit waits for `network-online.target`, runs as `cirrusync`, drops all
capabilities, makes the system read-only, restricts address families to Unix,
IPv4, and IPv6 sockets, and applies additional kernel/process hardening. It
does not use `PrivateNetwork`, an outbound IP denylist, or a restrictive
syscall filter because DNS resolution, HTTPS, CA certificate access, and
rustls must continue to work.

## Upgrade and uninstall

Download and review the new installer, then:

```console
sudo bash bootstrap.sh \
  --branch main \
  --update
```

The managed checkout must be clean and have the requested origin. An update
clones the selected branch to a separate staging tree, rebuilds and retests,
activates and validates the new binary transactionally, and promotes the staged
source only after the service stays healthy. It never regenerates or prints
the token. If the installation uses a custom configuration filename, repeat
the same `--config /etc/cirrusync/<name>.toml` option on every update,
reconfiguration, unit reinstall, and uninstall.

For an automated configuration change:

```console
sudo CIRRUSYNC_ZONE=example.com \
  CIRRUSYNC_RECORD=home.example.com \
  CIRRUSYNC_ENABLE_IPV4=true \
  bash bootstrap.sh --reconfigure --non-interactive
```

Interactive reruns offer binary update, reconfiguration, unit reinstall, and
uninstall. To uninstall directly:

```console
sudo bash bootstrap.sh --uninstall
```

The service, unit, and binary are removed. Configuration, token, service
account, state directory, and managed source are retained by default; an
interactive uninstall asks separately before deleting them. A
`--non-interactive --uninstall` therefore never deletes secrets.

## Troubleshooting

Start with:

```console
sudo -u cirrusync -- /usr/local/bin/cirrusync \
  --config /etc/cirrusync/config.toml check
sudo systemctl status cirrusync
sudo journalctl -u cirrusync --since "30 minutes ago" --no-pager
```

Common causes:

- **Authentication or permission failure:** create a token, not a Global API
  Key; grant Zone Read and DNS Edit and include the exact zone resource. An
  account-owned token also requires the owning Account ID in
  `cloudflare.account_id`; a user-owned token must omit it.
- **A valid `cfat_` token reports `401 Invalid API Token`:** account-owned
  tokens are verified through
  `/accounts/<ACCOUNT_ID>/tokens/verify`, not `/user/tokens/verify`. Confirm
  that `cloudflare.account_id` is the token's owning account. Cirrusync selects
  the correct endpoint automatically when that value is present.
- **Plain `check` exits nonzero:** read-only validation cannot prove DNS Edit.
  Use `--allow-edit-probe` when the documented narrow PATCH and external-edit
  race are acceptable, and add
  `--allow-create` if configured missing records may be created.
- **Record missing:** create it in Cloudflare or set
  `create_if_missing = true`. Plain `check` does not modify DNS; add
  `--allow-create` only when creation is intended.
- **Address discovery fails:** verify the server can resolve DNS, validate TLS
  certificates, and reach at least one configured provider over HTTPS.
- **IPv6 fails:** an IPv6-looking DNS answer or provider response is not enough;
  the host and ISP need working globally routed IPv6. Disable `[ipv6]` and AAAA
  records when they do not.
- **Hostname resolves but the service is unreachable:** compare the detected
  address with the router WAN address. A private WAN address or an address in
  `100.64.0.0/10` strongly suggests CGNAT. Ask the ISP for a public address,
  use routed IPv6, or use an outbound tunnel. Otherwise configure router port
  forwarding and host/router firewall rules.
- **Web works but SSH/VPN does not:** set the record to DNS only. Cloudflare's
  standard proxy is not a general-purpose TCP/UDP forwarder.
- **Service sandbox failure after customization:** inspect the journal. A
  custom configuration path may need a systemd override; the bootstrap creates
  one when `--config` is used.

DNS caches respect TTL, so clients may briefly retain the previous address
after a successful update.

## Security notes

- Restrict the token to the minimum zones and permissions. Revoke and replace
  it immediately after suspected exposure.
- Account and Zone IDs are identifiers, not authentication secrets. API tokens
  are secrets: never paste one into a screenshot, chat, command line,
  verification URL, or other record that may be retained.
- Never put a token in the TOML file, command line, systemd unit, issue, or
  shared log. Command-line arguments are commonly visible to other users.
- Protect configuration backups; knowing hostnames and update policy can still
  be sensitive.
- The daemon needs outbound DNS and HTTPS only. It never needs root,
  capabilities, an inbound listener, or write access to `/etc`.
- Public-IP providers learn the caller address and User-Agent. Select providers
  according to your privacy and availability requirements.
- A compromised DNS-edit token can redirect traffic even though it cannot
  administer the rest of the Cloudflare account.
- Review dependency and installer changes before upgrading. CI is a useful
  signal, not a substitute for source review.

Report vulnerabilities privately as described in [SECURITY.md](SECURITY.md).

## Development and releases

Normal tests use HTTP mocks and require no live Cloudflare credential:

```console
cargo fmt --all --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --all-features --locked
cargo build --release --locked
shellcheck --severity=warning \
  bootstrap.sh \
  tests/bootstrap_static.sh \
  tests/bootstrap_functions.sh \
  tests/bootstrap_fsmonitor_probe.sh \
  tests/bootstrap_lifecycle.sh
bash tests/bootstrap_static.sh
bash tests/bootstrap_functions.sh
```

`tests/bootstrap_lifecycle.sh` intentionally changes system users, systemd
state, trusted certificates, and `/etc/hosts`; run it only on a disposable
systemd-booted host. CI runs the real privileged lifecycle on Ubuntu 22.04 and
24.04 for x64 and Ubuntu 24.04 for ARM64. Ubuntu 24.04 x64 runs formatting,
Clippy, tests, and a release build; compatibility jobs run the Rust tests on
Ubuntu 22.04, ARM64, Windows Server 2022, and the Rust 1.85 MSRV. Debian 12
runs installer static/function checks and validates the hardened unit with
`systemd-analyze verify`. A separate scheduled RustSec scan keeps
advisory-service availability out of normal push/pull-request validation.

The initial release process is intentionally documented rather than
automatically publishing unaudited binaries: run all checks on a clean tagged
commit, build the locked Linux release for each supported architecture in a
controlled environment, generate SHA-256 checksums, sign the release metadata,
and attach the binaries/checksums to the GitHub release. Add a release workflow
only after reproducible cross-architecture builds and signing policy are
settled.

Contribution guidance is in [CONTRIBUTING.md](CONTRIBUTING.md), and notable
changes are tracked in [CHANGELOG.md](CHANGELOG.md).

## License

Cirrusync is available under the [MIT License](LICENSE).
