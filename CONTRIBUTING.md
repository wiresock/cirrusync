# Contributing to Cirrusync

Thank you for helping make Cirrusync safer and more useful. Small, focused
changes with tests are easiest to review.

## Development setup

Install stable Rust 1.85 or newer, Python 3.11 or newer, Git, and ShellCheck.
On Windows, use `py -3.11` in place of `python3` below. Then run:

```console
git clone https://github.com/wiresock/cirrusync.git
cd cirrusync
cargo fmt --all --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --all-features --locked
cargo build --release --locked
python3 scripts/versioning.py validate
python3 -m unittest tests/test_versioning.py -v
shellcheck --severity=warning \
  bootstrap.sh \
  tests/bootstrap_static.sh \
  tests/bootstrap_functions.sh \
  tests/bootstrap_fsmonitor_probe.sh \
  tests/bootstrap_lifecycle.sh
bash tests/bootstrap_static.sh
bash tests/bootstrap_functions.sh
```

Normal tests must not need a Cloudflare credential or make live Cloudflare
changes. Use HTTP mocks for network behavior. Never commit API tokens, active
configuration, captured `Authorization` headers, or logs containing secrets.
The real `tests/bootstrap_lifecycle.sh` test mutates system users, systemd,
trusted certificates, and `/etc/hosts`; run it only in disposable privileged
CI, never on a workstation or server.

## Versioning

Every pull request targeting `main` must increase the package version relative
to `main`. `Cargo.toml` under `[package]` is the source of truth, and the root
`cirrusync` package entry in `Cargo.lock` must contain the same version.

Cirrusync accepts release versions in strict `MAJOR.MINOR.PATCH` form. The new
version must have greater semantic precedence than the target branch:

- equal versions and downgrades are rejected;
- prerelease identifiers, build metadata, a leading `v`, missing components,
  and leading zeroes are not accepted;
- use a patch increment for compatible fixes and documentation, a minor
  increment for compatible functionality, and a major increment for an
  incompatible release.

Rebase or merge the latest target branch before choosing the version. This
prevents two concurrent pull requests from reusing the same next version.
Use the repository helper to update both version files, then run the normal
locked checks:

```console
python3 scripts/versioning.py bump patch # or: minor / major
python3 scripts/versioning.py validate
cargo fmt --all --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --all-features --locked
cargo build --release --locked
cargo run --locked -- --version
git diff --check
```

Confirm that `cargo run --locked -- --version` reports the version selected in
`Cargo.toml`. Add a dated `[X.Y.Z]` section for that version to `CHANGELOG.md`;
maintainers confirm the release date before merging. See
[docs/VERSIONING.md](docs/VERSIONING.md) for the complete policy, post-merge
tag procedure, and upgrade semantics.

## Pull requests

- Open an issue first for large behavior, configuration-schema, or dependency
  changes.
- Keep dependencies conservative and explain why each new one is needed.
- Add tests for behavior changes, including errors and partial failures.
- Update `README.md`, `config.example.toml`, and `CHANGELOG.md` when users will
  notice the change.
- Preserve backward compatibility where practical. Clearly call out any
  migration.
- Use comments to explain security decisions or non-obvious constraints, not
  to restate code.

By contributing, you agree that your contribution is licensed under the MIT
License in this repository.

## Reporting security issues

Do not open a public issue for a suspected vulnerability or accidental secret
exposure. Follow [SECURITY.md](SECURITY.md) instead.
