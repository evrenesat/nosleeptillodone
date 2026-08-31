# Agent Notes

This directory is a standalone Rust utility for macOS power management.

## Rules

- Keep the controller dependency-light and easy to audit.
- Prefer native macOS APIs for state reads; shell out only for privileged `pmset` actions.
- Do not add background work beyond lid-state monitoring and sleep/display control.
- Preserve conservative defaults. The delay should stay short unless the user explicitly changes it.
- Keep privileged power control in the LaunchDaemon/controller. User-facing menu bar code should observe state and open config, not mutate privileged settings directly.
- Update `README.md`, `ARCHITECTURE.md`, and `DEVLOG.md` when behavior, install flow, or safety assumptions change.
