# Contributing to Cirrusync

Thank you for helping make Cirrusync safer and more useful. Small, focused
changes with tests are easiest to review.

## Development setup

Install stable Rust 1.85 or newer, Git, and ShellCheck. Then run:

```console
git clone https://github.com/wiresock/cirrusync.git
cd cirrusync
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

Normal tests must not need a Cloudflare credential or make live Cloudflare
changes. Use HTTP mocks for network behavior. Never commit API tokens, active
configuration, captured `Authorization` headers, or logs containing secrets.
The real `tests/bootstrap_lifecycle.sh` test mutates system users, systemd,
trusted certificates, and `/etc/hosts`; run it only in disposable privileged
CI, never on a workstation or server.

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
