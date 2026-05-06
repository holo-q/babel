Harness Protocols
=================

Each directory owns the protocol knowledge for one native agent harness.

- `sessions.rs` reads native session storage for `babel ls-sessions` and `babel resume`.
- `ops.rs` plans provider-native migration/doctor/apply operations.
- `spec.rs` defines the static roster facts: display name, color, install
  surface, identity fields, cmdline markers, hook dialect, and resume command.

The public entrypoints stay domain-shaped:

- `native_sessions` is the listing registry.
- `harness_ops` is the migration operations registry.
- `agent_kind` keeps the enum and delegates roster lookup here.

The files live here so adding or auditing a harness starts in one place instead
of chasing scattered feature modules.
