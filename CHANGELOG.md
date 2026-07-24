# Changelog

All notable changes to Cirrusync are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and releases follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Initial Cloudflare Dynamic DNS daemon with one-shot, validation, and
  continuous operation modes.
- IPv4 and IPv6 public-address discovery with provider fallback.
- Multi-zone and multi-record synchronization through Cloudflare API v4.
- Defensive Debian/Ubuntu bootstrap installer and hardened systemd service.
- Unit, integration, installer, and CI checks.
- User-owned and account-owned Cloudflare API tokens, including explicit
  account-scoped verification.

### Security

- Enforced direct HTTPS without redirects or ambient proxies for Cloudflare
  and public-address requests, with bounded response bodies and secret-safe
  errors.
- Hardened configuration and token opening against links, races, special
  files, oversized input, and unsafe ownership or permissions.
- Added process exclusion, transactional installer rollback, isolated builds,
  strict service-identity validation, trusted unit verification, and
  crash-loop detection.

### Changed

- Zone IDs are verified as active and matching; automatic lookup rejects
  inactive or ambiguous zones across paginated results.
- Runtime cycles reconcile TTL/proxy metadata, aggregate typed failures, honor
  bounded server retry delays, and shut down without abandoning mutations.
- Missing-record creation is rechecked immediately before mutation and exact
  duplicate records are rejected instead of selected nondeterministically.
- `check` now requires an explicit edit probe or permitted creation before it
  claims DNS Edit was verified.
- Non-interactive configuration replacement now uses the explicit
  `--reconfigure` action.
- Authentication aborts report their root cause without derivative DNS-edit
  permission failures.

[Unreleased]: https://github.com/wiresock/cirrusync/commits/main
