# capulus design

## Boundary

capulus is a library, not a daemon. A product links it into one executable that provides both its
ordinary client commands and a hidden machine-agent namespace. The product owns the namespace name,
its application protocol, installation policy, and first-install bootstrap. Capulus supplies a
composable `AgentLifecycleCommand` for `installation-manifest` and `redeploy-worker`.

Linux support is split by feature:

- `managed-client`: protocol types, the blocking Unix-socket client, and an unprivileged
  exact-version user-program updater.
- `managed-system`: socket activation, server, release resolution, build worker, installation
  transaction, and generated systemd configuration.

## One managed program

`ManagedProductOptions::validate` consumes the complete declaration before side effects.
`ManagedProgramOptions` identifies exactly one Cargo binary, its root-owned path under
`/usr/local/bin`, and a nonempty command prefix. The Cargo binary name must equal the installed
filename. Service and transient-worker commands are derived from that one validated declaration.

For a product named `example` with prefix `agent`, the topology is:

```text
user Cargo copy:       ~/.cargo/bin/example
system copy:           /usr/local/bin/example
service:               /usr/local/bin/example agent serve ...
redeploy worker:       /usr/local/bin/example agent redeploy-worker --job ID
manifest renderer:     STAGED/example agent installation-manifest
```

There is no separately built or installed `example-agent` executable. The hidden namespace is not
an authorization boundary; effective UID, Unix-socket peer credentials, and product operator-group
policy are.

## Sockets and authorization

Each product has one root service and two independently governed Unix sockets:

```text
application clients ---- /run/<product>/agent.sock ---+
                                                       +--> <product>-agent.service
management clients  ---- /run/<product>/capulus.sock -+
```

systemd owns both listeners, names their descriptors `application` and `capulus`, and passes them to
the service with `Accept=no`. The product owns the application protocol. Capulus handles agent
identity, release resolution, redeploy scheduling, job status, and installation repair.

The management socket is either root-only mode 0600 or mode 0660 with an explicit product operator
group. The server obtains the effective UID from `SO_PEERCRED`. A mutating request is authorized only
for root or an NSS member of that group; message bodies contain no identity claim. Non-root callers
cannot downgrade the installed system release.

## Protocol v2

One connection carries one request and response. Each frame is a four-byte big-endian length and at
most 64 KiB of CBOR. Requests carry a random 128-bit ID and an inclusive supported-major range;
responses echo the ID and selected major.

Methods are `info`, `resolve`, `redeploy`, `job-status`, and `repair`. Release resolution returns a
validated semantic version and, for a private Cargo source, its non-secret registry name. Registry
index, token, and CA material never cross the management response. Errors use stable codes and
bounded public messages.

Connections, frames, request duration, concurrent handlers, and per-UID request rate are bounded.

## User program and privilege model

User and system copies are deliberately independent installations of the same Cargo binary.
Before requesting a system cutover, product client code may use `UserProgramUpdateOptions` to run:

```text
cargo install --locked --force --version VERSION PACKAGE --bin BINARY --root INSTALL_ROOT [--registry NAME]
```

This helper refuses root, executes Cargo directly as the invoking user, has a validated deadline,
terminates the complete process group on interruption, and preserves typed cancellation. It is not
part of the privileged management request or worker. A system copy may be executed as a normal user
to drive this step; it does not acquire privilege.

Privileged Capulus code never executes a user-owned binary, user Cargo, user Rustup, or a program
selected from the caller's environment. Initial installation must first place the exact product
program at its declared path through a product-owned trusted bootstrap. Capulus then accepts
`BuildArtifacts::from_installed_program` only when `/proc/self/exe` resolves to that installed path
and the fixed `/usr/local/bin` path is composed of non-writable root-owned directories ending in a
root-owned mode-0755 regular file. Products use that same validation before invoking the system
program through sudo.

## Managed builds

Redeploy builds use the `capulus-build` nologin system account. The root-owned traverse-only home is:

```text
/var/lib/capulus-build/
  cargo-tools/   build-account-owned Rustup and Cargo proxies
  rustup/        build-account-owned stable toolchain
  cache/         build-account-owned Cargo registry and Git caches
  target/        build-account-owned serialized build target
  jobs/          root-owned boundary containing isolated build-account job roots
```

Root creates and validates top-level boundaries through non-following descriptors, then delegates
only compilation. A global lock serializes toolchain mutation, shared caches, and managed builds
across products. Cargo installs one exact package version and one exact binary into a fresh job
root. The staged program renders its own installation manifest through the declared command prefix.
Capulus validates version, ownership, file type, manifest identity, destinations, and unit content
before staging.

Private-registry configuration, credentials, and CA material live only in mode-0600 per-job files,
are omitted from argv and diagnostics, and are removed after Cargo returns.

Only the current root-owned boundary layout and empty states produced by an interrupted creation are
accepted. Build-account-owned legacy boundaries, unexpected entries, symlinks, non-directories,
nonempty interrupted states, and every other ownership or mode combination fail closed. Layout
changes therefore require an explicit operator repair during a release transition; runtime code
does not silently migrate old state.

## Redeploy and recovery

A redeploy is an independent transient systemd service named
`<product>-redeploy-<job-id>.service`; there is no permanent update service. PID 1 supervises its
cgroup and invokes the installed program's hidden worker command. Job phases are monotonic:
preparing, toolchain, building, validation, staging, system commit, agent restart, complete, or
failed.

Secret release requests live under `/run/capulus/jobs/<product>`. Sanitized status lives under
`/var/lib/capulus/jobs/<product>`. Identical active releases coalesce. Stored active state is checked
against the exact transient unit, including a bounded grace for the unit-start race and boot-ID
reconciliation after restart.

The worker has a finite systemd runtime, explicit task and resource accounting, cgroup termination,
and a build deadline. It waits for both the Capulus and product application protocols to report the
exact target identity and version before accepting the installation.

## Installation transaction

Each product has a non-waiting installation lock. A journal under
`/var/lib/capulus/installations/<product>` records prior digests, enablement, same-filesystem backups,
staged replacements, and commit progress. Capulus:

1. validates the manifest and artifacts;
2. stages every replacement and removal;
3. renames and fsyncs each destination while advancing the journal;
4. runs `systemd-analyze verify` with a deadline;
5. reloads units and restarts the service while retaining same-topology listeners; and
6. accepts and cleans the transaction only after both protocols are healthy.

Failure before acceptance rolls back files and prior unit enablement. Recovery resumes from the
journal after interruption. This is an explicit phase-based workflow, not a claim of cross-system
atomicity.

Uninstallation has its own journal. It stops units, moves only managed files to private backups,
reloads systemd, commits removal, and then deletes backups. An interruption before the commit
boundary restores the installation.

## Filesystem layout

```text
/var/lib/capulus-build/                   shared unprivileged Rust build home
/var/lib/capulus/jobs/<product>/          durable sanitized job state
/var/lib/capulus/installations/<product>/ installation and uninstall journals
/run/capulus/jobs/<product>/              ephemeral secret redeploy requests
/run/capulus/locks/                       build, scheduling, and installation locks
/run/<product>/agent.sock                 product application protocol
/run/<product>/capulus.sock               Capulus management protocol
```

Shared Capulus state is root-owned mode 0700 unless a narrower validated boundary explicitly needs
traversal. Product state modes never propagate to the shared roots.
