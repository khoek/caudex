# capulus

Shared support for komputation command-line tools. Its invocation-scoped UI
provides delayed spinners for fast queries, counters, countdowns, aligned live
groups, ANSI-free plain progress, terminal handoff suspension, and cooperative
Ctrl-C cancellation from one validated `UiOptions` path.

## Managed Linux agents

The opt-in `managed-client` feature provides the bounded Capulus v1 management protocol and its
blocking Unix-socket client. The Linux-only `managed-system` feature adds the server, named systemd
socket-descriptor adoption, managed product declarations, shared build account, transient redeploy
workers, recoverable installation transactions, and ordinary-user Cargo reinstall support.

Capulus is a library, not a broker or daemon. Each product owns one agent process. In the normal
topology, systemd owns the product's application listener at `/run/<product>/agent.sock` and the
Capulus v1 listener at `/run/<product>/capulus.sock`; both are passed to the same agent with
descriptor names `application` and `capulus`. `ApplicationSocketOptions::AgentBound` exists for a
staged migration from an already-deployed self-binding agent: systemd owns only `capulus` until a
later release atomically installs and adopts the application socket unit.

`ManagedProductOptions::validate` fixes every privileged package name, executable, destination,
service argument, state-root mode, unit name, socket path, and hardening policy before side effects.
A private product state root normally uses mode 0700. A product that intentionally exposes a
read-only artifact beneath its state root may declare a more permissive mode such as 0755 while
keeping secret-bearing descendants private. A product then uses `ManagedAgent` as its management
handler and `RedeployWorker` for the one-shot worker command. Only the `ReleaseSource` boundary is
product-specific.

### Protocol v1

The wire format is a four-byte big-endian length followed by at most 64 KiB of CBOR. Every request
has a random 128-bit request ID and an inclusive supported protocol-major range; every response
echoes the ID and selected major. One connection carries one request. The methods are `info`,
`resolve`, `redeploy`, `job-status`, and `repair`, with stable error codes and bounded public job
detail. The server derives PID, UID, and GID from `SO_PEERCRED`; identity claims are not carried in
request bodies.

### Filesystem and process model

- `/var/lib/capulus-build` is the private home of the shared `capulus-build` system account. Rustup,
  the stable minimal toolchain, Cargo cache, target cache, and isolated job roots live beneath it.
- `/run/capulus/jobs/<product>` contains root-only, short-lived redeploy requests. Sanitized job
  status survives under `/var/lib/capulus/jobs/<product>`.
- `/var/lib/capulus/installations/<product>/active.json` journals an in-progress multi-file install.
  New files and backups are staged on the destination filesystem, digested, fsynced, renamed, and
  retained until both agent protocols report the exact new version.
- `/var/lib/capulus/installations/<product>/uninstall.json` journals recoverable removal. Managed
  files first move to same-filesystem private backups; systemd is reloaded before removal is marked
  committed, and interrupted pre-commit removal restores both files and prior enablement.
- `/run/capulus/user-installs` holds an invoking user's ephemeral Cargo configuration. The user's
  own Cargo/Rustup run through `setpriv` with the NSS UID, primary GID, initialized supplementary
  groups, cleared environment, exact package version, and original Cargo install root.

Root never owns a Rust toolchain and never compiles. It creates and validates the build account,
copies validated build-account-owned artifacts, commits root-owned files, and talks directly to PID
1 over D-Bus. Each redeploy is an independently supervised transient
`<product>-redeploy-<job-id>.service`; no permanent update service or generated shell program exists.

Registry credentials exist only in root- or job-user-private ephemeral files. They are redacted from
debug output, removed after Cargo consumes them, and never placed in argv or the environment.
