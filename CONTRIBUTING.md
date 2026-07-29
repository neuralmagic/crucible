# Contributing

PRs and issues are welcome.

- **Sign your work**: every commit needs a [DCO](https://developercertificate.org/)
  sign-off (`git commit -s`, which adds a `Signed-off-by:` trailer).
- **Before opening a PR**: run `just lint` (fmt + clippy) and `cargo test` locally; CI runs
  the same under `.github/workflows/`.
- Keep changes focused; one logical change per PR.
- By contributing you agree your work is dual-licensed under MIT OR Apache-2.0 (see
  `LICENSE-MIT` / `LICENSE-APACHE`).
