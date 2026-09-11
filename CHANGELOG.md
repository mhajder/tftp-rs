# Changelog

## [0.4.0](https://github.com/mhajder/tftp-rs/compare/v0.3.2...v0.4.0) (2026-09-11)


### 🚀 Features

* bind TFTP sockets to a named network interface ([#28](https://github.com/mhajder/tftp-rs/issues/28)) ([79c4588](https://github.com/mhajder/tftp-rs/commit/79c45884712762f76f74332d7047dfb69fc60e83))
* expose an embeddable library API with optional dashboard dependencies ([#27](https://github.com/mhajder/tftp-rs/issues/27)) ([05c48b6](https://github.com/mhajder/tftp-rs/commit/05c48b6eb982b31cbb77a88e42de5f0e77be8388))


### 🐛 Bug Fixes

* let the HTTP file server bind a wildcard address ([#32](https://github.com/mhajder/tftp-rs/issues/32)) ([cdeb5a0](https://github.com/mhajder/tftp-rs/commit/cdeb5a00ac23a0639679a6fbefbc186136de9203))
* report an unusable bind, and scope --interface to every socket ([#33](https://github.com/mhajder/tftp-rs/issues/33)) ([119116d](https://github.com/mhajder/tftp-rs/commit/119116d5feb044bb8f81b87dbacfe5272c692fb1))

## [0.3.2](https://github.com/mhajder/tftp-rs/compare/v0.3.1...v0.3.2) (2026-04-06)


### 🐛 Bug Fixes

* crates.io authentication token for cargo publish in release workflow ([#16](https://github.com/mhajder/tftp-rs/issues/16)) ([26165ec](https://github.com/mhajder/tftp-rs/commit/26165eca20ed41d81db78727487d603c7b5708f5))

## [0.3.1](https://github.com/mhajder/tftp-rs/compare/v0.3.0...v0.3.1) (2026-04-06)


### 🐛 Bug Fixes

* update crates.io publishing to use OIDC authentication via rust-lang/crates-io-auth-action ([#14](https://github.com/mhajder/tftp-rs/issues/14)) ([c33fb82](https://github.com/mhajder/tftp-rs/commit/c33fb821fcaf69d73e814612c371859964b649e7))

## [0.3.0](https://github.com/mhajder/tftp-rs/compare/v0.2.0...v0.3.0) (2026-04-06)


### 🚀 Features

* add automated crates.io publishing workflow and clean up Cargo.toml categories ([#12](https://github.com/mhajder/tftp-rs/issues/12)) ([ee1e959](https://github.com/mhajder/tftp-rs/commit/ee1e959e5c17ca11a4a59f3c6b2f3ed028807d38))

## [0.2.0](https://github.com/mhajder/tftp-rs/compare/v0.1.0...v0.2.0) (2026-02-26)


### 🚀 Features

* **server:** add RFC option negotiation, windowed and netascii mode ([#7](https://github.com/mhajder/tftp-rs/issues/7)) ([8da71db](https://github.com/mhajder/tftp-rs/commit/8da71db25846c0762a92ab809b27a9da4f7bb80d))

## [0.1.0](https://github.com/mhajder/tftp-rs/compare/v0.0.1...v0.1.0) (2026-02-13)


### 🚀 Features

* initialize Rust TFTP server with TUI, HTTP, and CI ([e892710](https://github.com/mhajder/tftp-rs/commit/e89271072970b56b70c20197a9b521203c653eba))
