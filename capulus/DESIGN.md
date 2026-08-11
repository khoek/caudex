# capulus design

## Library boundaries

capulus is a library, not a resident broker. Its default modules are process-local CLI utilities:
terminal UI, cancellation, locking, process execution, persistent stores, temporary files, paths,
shell rendering, containers, and artifact metadata.

Managed Linux support is split by feature. `managed-client` contains the protocol and blocking
Unix-socket client. `managed-system` adds Linux- and systemd-specific server, activation, build,
installation, and worker code. A product links those components into its own agent; capulus does not
install a global daemon or own an application protocol.

## Managed-agent topology

Each product has one persistent root process and two independently governed Unix sockets:

```text
application clients ---- /run/<product>/agent.sock ---+
                                                       +--> <product>-agent.service
management clients  ---- /run/<product>/capulus.sock -+
```

In the final topology systemd owns both listeners, gives them descriptor names `application` and
`capulus`, and passes both to the same service with `Accept=no`. The application protocol remains
entirely product-owned. capulus handles only information, release resolution, redeploy scheduling,
job status, and installation repair.

`ApplicationSocketOptions::AgentBound` supports a staged transition from an already-deployed agent
that binds its own application socket. In that state systemd owns only the capulus listener. A later
installation can atomically add the application socket unit and switch the service to descriptor
adoption. The installation transaction understands both directions of this topology change.

There is no persistent update broker or redeploy service. A redeploy is an independent transient
systemd service named `<product>-redeploy-<job-id>.service`. PID 1 supervises its cgroup while it
replaces and restarts the persistent agent.

## Validated product model

`ManagedProductOptions::validate` consumes the public configuration before side effects. It fixes
the product and package identifiers, installed binaries, user CLI, agent executable, service argv,
socket paths and policy, hardening, state mode, build deadline, task limit, and transient-worker
runtime. Paths and unit text are restricted so untrusted request data cannot become an executable,
unit name, destination, systemd specifier, or argument.

The generated persistent service owns only `StateDirectory=<product>`. Shared capulus state is
created explicitly as root with mode 0700; it is added to `ReadWritePaths` for services using strict
filesystem protection. Runtime directories remain product-specific plus the shared capulus runtime
root needed for private job requests and locks.

## Protocol v1

The protocol is a four-byte big-endian length followed by at most 64 KiB of CBOR. Every request has
a random 128-bit request ID and an inclusive supported protocol-major range. Every response echoes
the request ID and selected major. One connection carries one request.

The methods are `info`, `resolve`, `redeploy`, `job-status`, and `repair`. Errors use stable codes and
bounded public messages. Internal diagnostics stay in a separate root-only diagnostic file and the
installation journal. The server obtains PID, UID, and GID from `SO_PEERCRED`; request bodies contain
no identity claim.

Connections, request duration, frame size, concurrent handlers, and per-UID request rate are
bounded. A non-root redeploy must request exact-version reinstall of an existing Cargo-installed
user CLI. capulus validates the caller's live process, NSS identity, Cargo/Rustup paths, and binary
ownership before resolving a release. Root may schedule a system-only redeploy; a non-root caller
may not downgrade the system installation.

## Build and privilege model

Root never owns a Rust toolchain and never compiles. System builds use one `capulus-build` system
account with a nologin shell and a root-owned traverse-only home at `/var/lib/capulus-build`:

- build-account-owned `cargo-tools` contains rustup and its Cargo proxy;
- build-account-owned `rustup` contains the stable minimal toolchain;
- build-account-owned `cache` and `target` are shared across serialized builds; and
- root-owned `jobs` contains isolated build-account-owned per-redeploy roots.

Because the home itself is not writable by the build account, it cannot rename or replace those
top-level directory entries. Root creates and validates each boundary using non-following file
descriptors before delegating work.

A global build lock serializes toolchain mutation, private registry configuration, shared caches,
and the commit workflow across products. Root downloads and verifies rustup-init, then executes it
as `capulus-build` with cleared supplementary groups and an exact environment. Cargo builds into a
fresh job install root. Root accepts only expected executable regular files owned by the build
account and verifies every reported version and the staged agent's installation manifest.

Registry credentials and custom CA material exist only in private per-job files. They are removed
after Cargo consumes them and are never placed in argv or diagnostic output.

If the invoking non-root user already has the managed CLI in that user's Cargo bin directory, a
successful system installation runs that user's own Cargo and Rustup under the revalidated NSS UID,
primary GID, supplementary groups, home, Cargo home, and Rustup home. Cargo installs only the user
binary at the exact committed version. capulus never creates a user installation where none
existed and never replaces Cargo metadata with a symlink or dispatcher.

## Redeploy state and liveness

Secret-bearing requests live under `/run/capulus/jobs/<product>`. Sanitized status and root-only
diagnostics live under `/var/lib/capulus/jobs/<product>`. Job phases are monotonic and distinguish
preparation, toolchain work, build, validation, staging, system commit, agent readiness, user
reinstall, completion, and failure.

The coordinator coalesces only an identical active release and required UID. It checks the exact
transient unit's active state before treating a stored job as live. A short queued/request-file grace
closes the `StartTransientUnit` race; an inactive same-boot unit is otherwise recorded as failed.
Different-boot active state is failed during store initialization.

Transient workers have a finite runtime covering toolchain work, both possible Cargo builds, system
activation, health checks, and cleanup. Their systemd cgroup has explicit task, CPU, I/O, memory,
kill, OOM, logging, and network-ordering policy.

## Recoverable installation

Each product has a non-waiting installation lock under `/run/capulus/locks`. Repair, install,
recovery, and uninstall hold it across the complete transaction, so repair cannot roll back a live
worker.

An installation journal lives at
`/var/lib/capulus/installations/<product>/active.json`. Before commit, capulus:

1. validates the target manifest and build-account artifacts;
2. records current digests and exact prior unit enablement;
3. stages replacements and backups on each destination filesystem; and
4. represents target omissions as journaled removals.

Commit renames each old file to its same-filesystem backup, renames a replacement into place when
present, fsyncs the destination directory, and durably advances the journal. Once every target file
is present, capulus runs `systemd-analyze verify` with a deadline against the installed unit paths,
before systemd reloads or activates anything. This lets a first installation validate an
`ExecStart` binary committed by the same transaction. A verification failure rolls the journaled
files back. This is a recoverable multi-file transaction, not a claim of globally atomic
replacement.

A normal same-topology redeploy keeps listening sockets alive and restarts the service after reload.
A topology change first quiesces both sockets and the service, removes only the exact verified
root-owned application socket inode, restores unit enablement, starts target sockets, and then
starts the service. Rollback performs the inverse cutover before restarting the restored service.
The transaction is finalized only after both the capulus protocol and product application protocol
report the exact target version.

Uninstallation uses a separate journal. It deactivates units, moves every managed file to a private
same-filesystem backup, reloads systemd, marks removal committed, and then deletes backups. An
interruption before that boundary restores files and prior enablement.

## Filesystem layout

```text
/var/lib/capulus-build/                  shared unprivileged Rust build home
/var/lib/capulus/jobs/<product>/         durable job status and diagnostics
/var/lib/capulus/installations/<product>/ installation and uninstall journals
/run/capulus/jobs/<product>/             ephemeral secret redeploy requests
/run/capulus/locks/                      build, scheduling, and installation locks
/run/capulus/user-installs/              ephemeral requesting-user Cargo configuration
/run/<product>/agent.sock                product application protocol
/run/<product>/capulus.sock              capulus management protocol
```

Private shared state is root-owned mode 0700. Product state may intentionally use a different
validated mode, but that mode never propagates to `/var/lib/capulus`.
