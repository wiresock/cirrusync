# Changelog

All notable changes to Cirrusync are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and releases follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2026-07-29

### Changed

- Renamed the public installer from `bootstrap.sh` to
  `cirrusync-install.sh` so downloads can coexist with installers for other
  Wiresock components. Update saved commands and automation to use
  `https://raw.githubusercontent.com/wiresock/cirrusync/main/cirrusync-install.sh`;
  the former raw URL is no longer provided.
- Post-install instructions now invoke the uniquely named installer from the
  managed source checkout, so they also work after a piped installation.
- Clarified that incompatible changes during initial `0.x` development advance
  the minor version.

## [0.1.1] - 2026-07-28

### Added

- Strict, monotonic `MAJOR.MINOR.PATCH` version validation for pull requests,
  with semantic TOML validation and `Cargo.toml` as the version source of
  truth.
- Version-aware installer upgrades that distinguish newer, equal, and older
  source versions before replacing an installed binary.
- Regression coverage and user documentation for the existing
  `cirrusync --version` interface.

### Changed

- The installer's `--upgrade` action is primary; `--update` remains a
  compatibility alias.
- Equal-version upgrades now repair missing or drifted runtime files,
  permissions, service identity, and systemd configuration instead of
  returning a false no-op.
- Contributor and release documentation now describes version selection,
  lockfile synchronization, and downgrade prevention.

## [0.1.0] - 2026-07-24

### Added

- Initial Cloudflare Dynamic DNS daemon with one-shot, validation, and
  continuous operation modes.
- IPv4 and IPv6 public-address discovery with provider fallback.
- Multi-zone and multi-record synchronization through Cloudflare API v4.
- Defensive Debian/Ubuntu installer and hardened systemd service.
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

[Unreleased]: https://github.com/wiresock/cirrusync/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/wiresock/cirrusync/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/wiresock/cirrusync/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/wiresock/cirrusync/releases/tag/v0.1.0
