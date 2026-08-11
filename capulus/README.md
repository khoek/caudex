# capulus

Shared support for Rust command-line tools.

capulus provides reusable building blocks for polished, robust CLIs:

- terminal-aware tasks, counters, countdowns, live groups, prompts, and cooperative cancellation;
- single-invocation locks, checked child-process helpers, secure temporary files, and atomic stores;
- container, artifact, path, shell, and Google Cloud helpers; and
- optional Linux support for a CLI that also installs and maintains a privileged systemd agent.

The default feature set contains the general CLI utilities. `managed-client` adds the bounded
management protocol and Unix-socket client. On Linux, `managed-system` adds socket activation,
the management server, transient redeploy workers, recoverable installation transactions, a shared
unprivileged Rust build account, and exact-version reinstall of an invoking user's existing
Cargo-installed CLI.

```toml
[dependencies]
capulus = "0.5"
```

```rust
use capulus::ui::{TaskOptions, Ui, UiOptions};

fn main() -> anyhow::Result<()> {
    let ui = Ui::new(UiOptions::default().validate()?)?;
    let task = ui.task(TaskOptions {
        label: "Loading records".into(),
        ..TaskOptions::default()
    })?;

    // Do the work and update the task as needed.
    task.finish("Records loaded");
    Ok(())
}
```

The managed-system architecture, trust boundaries, wire protocol, filesystem layout, and recovery
model are documented in [DESIGN.md](DESIGN.md).
