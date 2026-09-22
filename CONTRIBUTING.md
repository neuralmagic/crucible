# Contributing

PRs and issues are welcome.

- **Sign your work**: every commit needs a [DCO](https://developercertificate.org/)
  sign-off (`git commit -s`, which adds a `Signed-off-by:` trailer).
- **Before opening a PR**: run `just lint` (fmt + clippy) and `cargo test` locally; CI runs
  the same under `.github/workflows/`.
- **The controller** needs a Postgres for its tests (`just dev-pg`, then
  `DATABASE_URL=... SQLX_OFFLINE=true cargo test -p crucible-controller`) and
  [bun](https://bun.sh) for the UI (`cd crucible-controller/ui && bun run check`).
- Keep changes focused; one logical change per PR.
- By contributing you agree your work is dual-licensed under MIT OR Apache-2.0 (see
  `LICENSE-MIT` / `LICENSE-APACHE`).
