# Changelog

## [0.4.1](https://github.com/mhajder/tftp-rs/compare/v0.4.0...v0.4.1) (2026-09-13)


### 🐛 Bug Fixes

* answer a failed request with a TFTP ERROR packet ([f24d150](https://github.com/mhajder/tftp-rs/commit/f24d15008f6a6191837f4e11d1d6b9c9562f2dfa))
* answer a read request that names something other than a regular file ([b3f1067](https://github.com/mhajder/tftp-rs/commit/b3f10679a415e8988b62153b56a41d69b68edac7))
* bound the dashboard's directory walk and log buffer ([04101dd](https://github.com/mhajder/tftp-rs/commit/04101ddae66a6a486ce4da235833ef9b410dd694))
* bound the post-upload wait and promote on filesystems without hard links ([8f662d6](https://github.com/mhajder/tftp-rs/commit/8f662d6267d5811591cdc8fd5703dbe045532c93))
* bound the resends a windowed download will make without progress ([8822c90](https://github.com/mhajder/tftp-rs/commit/8822c9007617b67f54d1624335dd2b9815b7ad67))
* count unexpected packets against the retry budget ([785ee0b](https://github.com/mhajder/tftp-rs/commit/785ee0b69d5176b6aa616c93e2511bc75f42d2a4))
* decide OACK retransmission by progress, not by block number ([c091b53](https://github.com/mhajder/tftp-rs/commit/c091b53a20c7561674aaf9d0e675699b18eb91cf))
* keep serving when nobody is reading the event channel ([727e317](https://github.com/mhajder/tftp-rs/commit/727e317857b944bf005d3078b8195944af4ce724))
* keep the bare --allow-overwrite form, and bind the HTTP port once ([5bae4ba](https://github.com/mhajder/tftp-rs/commit/5bae4ba61908102b1885a3b7715253801c0bfc9e))
* number windowed download blocks consecutively ([47c0651](https://github.com/mhajder/tftp-rs/commit/47c065131ecfa68b82a70eac8eba469897f8f76d))
* rate-limit replies to junk datagrams and keep accepting binary mode ([a1ad0e7](https://github.com/mhajder/tftp-rs/commit/a1ad0e763ec9df481acf8fc548d5d104c408c17e))
* repeat the OACK when an upload's first block is late ([b1a8aeb](https://github.com/mhajder/tftp-rs/commit/b1a8aeb0669b4ae731ef22653186bd2f78da8a47))
* restore --allow-overwrite, and probe binds the way the server binds ([cfc1543](https://github.com/mhajder/tftp-rs/commit/cfc1543d6360f1b8bfb158d4610ea0c466aab7eb))
* stage each upload under its own name and never clobber on promotion ([3ca3a91](https://github.com/mhajder/tftp-rs/commit/3ca3a915cdad173db1aeb2c45b3044774ac96435))
* stop reporting the file size as tsize for a netascii download ([ca822f4](https://github.com/mhajder/tftp-rs/commit/ca822f41dc49026065d0cbb3c048715b3cd29798))
* stop the directory listing from executing uploaded names and files ([3cb642c](https://github.com/mhajder/tftp-rs/commit/3cb642c17c4ad3d7be3b5cef8cee657719685557))
* survive stray datagrams, short reads, and a lost final ACK ([e4a529a](https://github.com/mhajder/tftp-rs/commit/e4a529a3350370b4e7130e61849f9d3ed0bb8c8b))
* tighten request parsing and keep a netascii transfer's last byte ([f352a93](https://github.com/mhajder/tftp-rs/commit/f352a9346c1127e525ec57680be6dd1881d0b04d))
* treat a repeated window acknowledgment as recovery, not as a stray ([b8755ad](https://github.com/mhajder/tftp-rs/commit/b8755adc1aad687026cba5fce9de8e36bcabbd06))


### 📚 Documentation

* correct the README where it no longer matches the server ([87c32a4](https://github.com/mhajder/tftp-rs/commit/87c32a4d775fabcac49ae1a8b1d33340d8fb3183))

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
