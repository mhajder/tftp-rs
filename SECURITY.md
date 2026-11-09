# Security Policy

## Supported versions

Only the latest release gets security fixes. Update to the newest version from [crates.io](https://crates.io/crates/tftp-rs) or [GitHub Releases](https://github.com/mhajder/tftp-rs/releases/latest) and check that the issue still happens before reporting.

## Reporting a vulnerability

Please do not open a public issue. Report it privately through [GitHub private vulnerability reporting](https://github.com/mhajder/tftp-rs/security/advisories/new).

Include the tftp-rs version, the OS, the exact command line (or `ServerConfig` if you use the library), the client or packets that trigger it, and what an attacker could do with it.

I aim to acknowledge a report within 7 days and will keep you updated until it is fixed.

## Scope

In scope is the `tftp-rs` binary and the `tftp_rs` library, for example:

- reading, writing or creating a file outside the served directory, through `..`, absolute paths, symlinks, Windows drive or UNC paths, or any other request
- downloading when `--disable-read` is set, uploading when `--disable-write` is set, or replacing an existing file when `--allow-overwrite false` is set
- reading or replacing an upload's `.part` staging file, or getting a partial upload served as if it were complete, over TFTP or HTTP
- getting past `--max-upload-size` or `--max-concurrent-transfers`, or making the server use memory, disk or descriptors without bound
- a packet or request that crashes the server or stops it answering other clients
- using the server as a reflector: getting it to send a spoofed address replies larger or more numerous than the requests, beyond what the protocol needs
- the HTTP file server (`--http-port`): serving files outside the directory, or HTML/script injection through file names in the directory listing
- a client-supplied filename or mode injecting fake lines or terminal escape sequences into the dashboard or the `--log-file`
- `--interface` letting traffic leave or arrive on a different interface than the one named

Out of scope:

- TFTP has no authentication or encryption. Anyone who can reach the port can download every file in the served directory and, unless `--disable-write` is set, upload files. That is the protocol, not a vulnerability; restrict access with `--bind`, `--interface` and a firewall.
- Flooding the network or the host with more traffic than it can handle.
- Vulnerabilities in dependencies that tftp-rs does not expose; report those to the dependency.
