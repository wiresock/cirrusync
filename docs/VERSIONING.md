# Versioning

Cirrusync uses release-only [Semantic Versioning](https://semver.org/) and
requires every pull request targeting `main` to advance the software version.

## Source of truth

The `[package].version` value in the repository's root `Cargo.toml` is
canonical. The root `cirrusync` package entry in `Cargo.lock` is a generated
mirror and must match it. Do not add a separate version file or hard-code a
version in Rust or `bootstrap.sh`.

The Rust build exposes the canonical package version through
`CARGO_PKG_VERSION`, which Clap uses for:

```console
cirrusync --version
```

This command does not load the active configuration, read the API token, or
access the network.

## Accepted versions

Versions must use strict `MAJOR.MINOR.PATCH` form, where each component is a
non-negative decimal integer without leading zeroes and no greater than
`999999999`. Examples:

| Version | Accepted | Reason |
| --- | --- | --- |
| `0.1.1` | Yes | Canonical release version |
| `1.0.0` | Yes | Canonical release version |
| `0.1.01` | No | Numeric component has a leading zero |
| `v0.1.1` | No | A `v` prefix is not part of the package version |
| `0.1` | No | Patch component is missing |
| `0.1.1-rc.1` | No | Prerelease versions are not used |
| `0.1.1+build.2` | No | Build metadata is not used |

Restricting the project to release versions gives the pull-request check and
privileged installer one unambiguous ordering.

## Pull-request policy

Every pull request targeting `main`, including documentation and CI-only
changes, must set a version that has strictly greater semantic precedence than
the version on `main`. Equal versions and downgrades fail the required
version-policy check. The `Cargo.lock` package version must be updated in the
same pull request.

Choose the increment according to the change:

- **patch** for compatible fixes, maintenance, documentation, and CI changes;
- **minor** for backward-compatible functionality;
- **major** for an incompatible release.

Before selecting a version, update the branch from its target branch. If two
pull requests choose the same next version, the later one must update its base
and choose a version greater than the version that merged first. Keeping the
`cirrusync/version-policy` status required and requiring branches to be current
before merge prevents duplicate or out-of-order releases without a post-merge
bot commit. That status is published by a trusted `pull_request_target`
workflow which reads the proposed version files as Git data; it never checks
out or executes pull-request code with its status-write permission.

Update and verify a version locally with:

```console
# Choose patch, minor, or major. This updates Cargo.toml and Cargo.lock.
python3 scripts/versioning.py bump patch
python3 scripts/versioning.py validate
cargo fmt --all --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --all-features --locked
cargo build --release --locked
cargo run --locked -- --version
git diff --check
```

The version helper replaces each file atomically and attempts to restore both
originals if either replacement fails. A power loss between the two
replacements can still leave them inconsistent; `validate` and the subsequent
`--locked` commands detect that state. Record user-visible changes in
`CHANGELOG.md`.

Because each merge is a release, its pull request must also add a dated
`[X.Y.Z]` changelog section matching the selected package version. Maintainers
confirm that date before merge; `[Unreleased]` is only a temporary drafting
area.

## Tags

After a versioned change reaches `main` and all required checks pass, a
maintainer creates an annotated `vMAJOR.MINOR.PATCH` tag at that exact commit
and pushes it. A tag must never be moved or reused. Tags identify reviewed
source states; they do not currently publish prebuilt binaries or a GitHub
Release automatically.

## Bootstrap upgrade behavior

Use the version-aware upgrade action:

```console
sudo bash bootstrap.sh --upgrade
```

`--update` is retained as a compatibility alias. The installer compares the
selected source version with the version reported by the trusted installed
binary:

| Comparison | Result |
| --- | --- |
| Source is newer | Build, test, activate, and validate it transactionally |
| Versions and managed commit are equal, and the installation is complete | Report that Cirrusync is current; do not replace Cirrusync files, refresh Rust, or restart the service |
| Version is equal but managed artifacts are missing | Repair the incomplete installation |
| Version is equal but the source commit differs | Refuse unversioned source unless `--force-reinstall` is explicit |
| Source is older | Refuse the downgrade and leave the installation unchanged |
| A prior installation remains but both its binary and managed-source version are unavailable | Refuse recovery because no downgrade-safe version floor can be established |
| Either version is unsupported or malformed | Stop with an error before replacing the installed binary |

`--upgrade --force-reinstall` may rebuild an equal version when recovery is
necessary. It does not permit installing an older version.

An equal-version no-op also requires the service account, file ownership and
modes, ACLs, trusted systemd unit, configuration drop-in, state directory, and
active/enabled service state to match the installed contract. Repairable drift
in those items enters transactional repair instead of being reported as
current. Unsafe extended/default ACLs on the configuration directory or token
are rejected during preflight and must be removed manually. If both
version-bearing artifacts were manually removed, restore either the trusted
managed source or the last installed binary before upgrading; do not guess a
recovery version.

The built candidate's reported version must agree with the selected source
version before activation. Configuration, API token, service state, and the
last-known-good binary remain covered by the installer's existing rollback
transaction.
