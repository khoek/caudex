# capulus

Shared support for Rust command-line tools.

capulus provides reusable building blocks for polished, robust CLIs:

- terminal-aware tasks, counters, countdowns, live groups, prompts, and typed cancellation;
- invocation locks, checked child-process helpers, secure temporary files, and atomic stores;
- container, artifact, path, shell, and Google Cloud helpers; and
- optional Linux support for a product CLI that also contains its privileged system agent.

The default feature set contains the general CLI utilities. `managed-client` adds the bounded
management protocol, Unix-socket client, and unprivileged exact-version Cargo updater.
`managed-system` adds socket activation, the management server, composable hidden lifecycle
commands, transient redeploy workers, recoverable installation transactions, and a shared
unprivileged Rust build account.

```toml
[dependencies]
capulus = "0.6"
```

```rust
use capulus::ui::{TaskOptions, Ui, UiOptions};

fn main() -> anyhow::Result<()> {
    let ui = Ui::new(UiOptions::default().validate()?)?;
    let task = ui.task(TaskOptions {
        label: "Loading records".into(),
        ..TaskOptions::default()
    })?;

    task.finish("Records loaded");
    Ok(())
}
```

A managed product declares one Cargo binary, one root-owned installed path, and the command prefix
under which it embeds Capulus lifecycle commands. User-installed copies remain ordinary
unprivileged Cargo installations; Capulus never asks a privileged process to execute or rebuild a
user-owned program.

The managed-system architecture, trust boundaries, protocol, filesystem layout, and recovery model
are documented in [DESIGN.md](DESIGN.md).
