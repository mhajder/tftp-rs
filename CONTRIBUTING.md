# Contributing

Thanks for helping out. Bug reports and feature requests go through the [issue templates](https://github.com/mhajder/tftp-rs/issues/new/choose). For security problems, follow [SECURITY.md](SECURITY.md) instead.

## Development setup

```sh
git clone https://github.com/mhajder/tftp-rs.git
cd tftp-rs
cargo build
```

You need a recent stable Rust toolchain (the crate uses the 2024 edition, so Rust 1.85 or newer) with `rustfmt` and `clippy`. `cargo run -- -p 6969 -d /tmp/tftp` starts the dashboard on an unprivileged port, which saves running as root while you work.

The checks are run by [prek](https://github.com/j178/prek), a faster drop-in for pre-commit; install it with `brew install prek`, `uv tool install prek` or `pipx install prek`. `prek install` (run it once in the clone) sets up the git hook configured in [`.pre-commit-config.yaml`](.pre-commit-config.yaml), which runs on every commit:

- basic file checks: trailing whitespace, line endings, YAML/TOML/JSON syntax, merge conflicts, files over 100 KB
- [typos](https://github.com/crate-ci/typos) for spelling
- `cargo fmt --check`, `cargo check --all-targets` and `cargo check --lib --no-default-features`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test`

CI runs the same hooks on every push and pull request.

## Before opening a pull request

```sh
prek run --all-files
```

Hooks that fix files (trailing whitespace, line endings, end of file) change them in place; review and stage the changes, then run it again.

- Add a test for the behavior you changed. Tests that drive the real server over UDP belong in [`tests/library_api.rs`](tests/library_api.rs); packet encoding and netascii tests live next to the code in [`src/tftp_protocol.rs`](src/tftp_protocol.rs).
- Try the change with a real client, for example `tftp`, `atftp` or `curl tftp://localhost:6969/file -o file`, and with the option you touched (`blksize`, `windowsize`, `netascii` and so on).
- Keep the library building without the TUI: `cargo check --lib --no-default-features` is part of the hooks for that reason.
- Mention which OS you tested on in the pull request, especially for `--interface`, which behaves differently on Linux and macOS.
- Update the README when you add an option or change behavior.

## Commit messages

Use [Conventional Commits](https://www.conventionalcommits.org/), for example `feat: add a --read-only-dir option` or `fix: keep the transfer alive on ENOBUFS`. [release-please](https://github.com/googleapis/release-please) builds the changelog and the next version number from them, so the type matters: `feat` and `fix` show up in release notes, `chore` does not.

Describe the change itself, not how it was found.

## License

By contributing, you agree that your contributions are licensed under the project's MIT license.
