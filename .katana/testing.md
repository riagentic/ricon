# Testing

- `cargo test` passes and covers every in-process-testable kata rule: pure helpers, theming, key/mouse encoding, session persistence, sidebar layout math, search filtering, replay, live-PTY shell behavior, and UI rendering
- UI tests assert real rendered buffer cells (ratatui TestBackend): text, per-tab background colors, white spinner, yellow favorite star, red replay button, search row, bold modifiers
- performance invariants are tested: staggered /proc sampling, viewport-only item building, on-screen-only agent scans, IO-free persistence
- `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check` are clean
- CI runs fmt, clippy, build and test on every push and pull request
