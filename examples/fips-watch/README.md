# fips-watch

FIPS early warning for a Rust repository. For every shipped (target, feature-set) variant, it
answers one question: does the compiled dependency graph still resolve crypto to the system
OpenSSL? Vendored crypto — `ring`, `aws-lc-rs`, a `vendored` build of `openssl-sys` — can never be
a certified module, so its arrival is the regression to catch.

```sh
FIPS_WATCH_REPO=ai-dynamo/modelexpress crucible plan run \
  --manifest examples/fips-watch/crucible.toml --max-cost 2 --max-time 30m
```

`FIPS_WATCH_REF` picks the branch (default `main`). `--param downstream_repo=<owner/name>` names
the fork that inherits this code on the next sync; the tracking issues are filed against it.

## The shape

`scan` reads the declared variant matrix. `probe` fans out one isolated instance per variant and
decides clean or dirty from `cargo tree -e normal` — free, no model. `select` emits only the dirty
ones, so a clean sweep spends nothing. `triage` is the single agent, and only for a dirty variant:
it names the dependency edge that carries the blocker, proposes the smallest fix, and drafts a
tracking issue. `roundup` assembles the report from captured evidence. `file` files the issues.
Finally, `card` folds the declared verdict and filing fields into bounded Markdown, and the pack
explicitly opts `card.markdown` into the engine-owned Slack `markdown` block. Crucible escapes
Slack control syntax and enforces the block-size limit. The pack never supplies Slack blocks, a
channel, or a webhook URL.

## Why the verdict is a build graph, not a lockfile

Three traps this pack exists to avoid, all observed on a real repository:

- **`Cargo.lock` is not the build graph.** A crate in the lock may be an optional dependency
  nothing enables. `ring`, `native-tls`, `openssl-sys` and `quinn` can all sit in one lock while
  no single build compiles more than one of them.
- **The host target is not the shipped target.** `native-tls` resolves to Security.framework on
  macOS and to OpenSSL on Linux, so an unpinned target answers a question about the developer's
  laptop.
- **A denylist encodes what someone thought of that day.** Feature unification means an unrelated
  dependency can enable a crypto backend four levels down, through a feature named for storage.

An unresolvable variant fails the probe rather than reporting a verdict: refusing to answer beats
answering wrong on a compliance check.

## Filing

`triage` drafts; `file` files. The agent never holds a credential. Issues carry a `fips-watch-key:`
marker, so a schedule firing on a timer comments on the open issue instead of opening another, and
one root cause across several variants is one issue. Without `GH_TOKEN` the run still passes and
leaves the payloads in `ISSUES.json`.
