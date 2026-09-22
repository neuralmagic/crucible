# crucible-controller

The outer loop's ledger and daemon core ([ADR-0043](../gov/adr/ADR-0043-the-outer-loop-discovery-and-scheduling-crucible-triage-crucible-autopilot.toml) /
[ADR-0044](../gov/adr/ADR-0044-orchestration-state-model-the-controllers-ledger.toml)). This is the workspace's **one async
island**: sqlx (and, later, kube + axum) share a tokio runtime here and nowhere else. The
`crucible` binary's `triage` / `autopilot` / `db` subcommands build a runtime and `block_on` into
this library (the `crucible/src/publish.rs` pattern), so no other crate grows an async dependency.

## Layout

- `migrations/0001_baseline.sql` — the ADR-0044 schema, up-only, squashed to a Postgres baseline
  at the cutover. One shared `MIGRATOR` static in `lib.rs`; every test references it via
  `#[sqlx::test(migrator = "crucible_controller::MIGRATOR")]`.
- `operations.rs` — raw SQL as free functions over `impl sqlx::PgExecutor<'_>` (the same
  query runs on the pool or in a transaction).
- `client.rs` — the `Db` domain-verb facade over `operations`, plus `connect()` (create-if-missing
  + migrate-on-open over `DATABASE_URL`) and `transition()` (claim + event-log append in lockstep).
- `event_log.rs` — the append-only NDJSON history at `<state>/controller-events.jsonl`.
- `config.rs` — `ControllerCfg` (clap + env fallbacks) and the `Profile` caps (deploy-profile TOML,
  `deny_unknown_fields`).
- `model.rs` — the strong types: `Status`, `ParkedBy`, and the row shapes.

## Cutting over a sqlite-era ledger

`crucible-controller db import-sqlite --from <state-dir>/outer.sqlite` streams every row of a
pre-Postgres ledger file into the (empty) database at `--db`/`DATABASE_URL`, one transaction,
preserving explicit ids and insertion order. Run it in-cluster with the daemon stopped — the pod
is what can reach both the state PVC and the database server. It refuses a non-empty target, so
re-running it is safe.

## sqlx offline metadata — the regen ritual

Queries are compile-time checked by the `sqlx::query!` macros. The checked-in `.sqlx/` (at the
**workspace root**) lets builds work with `SQLX_OFFLINE=1` and no database (this is what CI and every
normal `cargo build` use). Regenerate it whenever you add or change a query:

```sh
# From the workspace root. Needs a postgres-capable sqlx-cli:
#   cargo install sqlx-cli --no-default-features --features postgres,rustls
# ...and a scratch Postgres server, e.g.:
#   docker run -d --name crucible-pg-dev -e POSTGRES_PASSWORD=crucible \
#     -e POSTGRES_DB=crucible -p 5433:5432 postgres:16
export DATABASE_URL="postgres://postgres:crucible@localhost:5433/crucible"
sqlx database create
sqlx migrate run --source crucible-controller/migrations
rm -rf .sqlx                      # stale entries pass `--check` falsely — always start clean
cargo sqlx prepare --workspace -- --all-targets
```

CI runs `cargo sqlx prepare --check --workspace -- --all-targets` (against a freshly-migrated
throwaway DB) and fails if `.sqlx/` is out of date. Note the format of `.sqlx/` is written by the
crate's own sqlx **macro** version, not the CLI version, so the CLI need only be recent enough to
drive the workflow.

Verify an offline build after regenerating (compilation uses the cache; the tests themselves
still need `DATABASE_URL` at runtime — each `#[sqlx::test]` creates its own throwaway database
on that server):

```sh
SQLX_OFFLINE=true cargo test -p crucible-controller
```

### The Vault suite

`tests/vault_e2e.rs` drives the Vault client against a real server — no mocks. It uses the one at
`VAULT_ADDR`/`VAULT_TOKEN` when both are set (how CI runs it), and otherwise spawns a
`vault server -dev` per test from the `vault` binary on PATH:

```sh
brew install hashicorp/tap/vault    # or any 1.x/2.x release on PATH
SQLX_OFFLINE=true cargo test -p crucible-controller --test vault_e2e
```

Without the binary, `just dev-vault` runs one in docker instead (a `crucible-test-vault` container
on 58200, provisioned with the KV v2 mount and a fixed AppRole, idempotent on re-run) and prints the
`VAULT_*` lines to export:

```sh
just dev-vault
export VAULT_ADDR=http://127.0.0.1:58200 VAULT_TOKEN=crucible-dev-root
SQLX_OFFLINE=true cargo test -p crucible-controller --test vault_e2e
```

With neither a server nor a binary the tests print `SKIP` and pass. Set
`CRUCIBLE_REQUIRE_VAULT_TESTS=1` to make that skip a failure instead; CI sets it, so the suite
cannot silently stop running there. The registry's route tests (`api::secrets_tests`) use the same server the same way, so they run under a plain
`cargo test -p crucible-controller`.

### The OIDC suite

`src/oidc/keycloak_e2e.rs` drives the relying party against a real Keycloak — no mocks and no stub
issuer. It runs the actual browser flow (authorization code with PKCE, Keycloak's rendered login
form, the callback) and presents access tokens Keycloak actually minted, against the same router
`serve` mounts. The realm, clients, and users live in `tests/keycloak/crucible-realm.json`.

It uses the issuer at `KEYCLOAK_URL` when set (how CI runs it), and otherwise starts the shared
`crucible-test-keycloak` container on 58180 itself. `just dev-keycloak` provisions the same one:

```sh
just dev-keycloak
export KEYCLOAK_URL=http://127.0.0.1:58180
SQLX_OFFLINE=true cargo test -p crucible-controller --lib -- oidc::keycloak_e2e
```

The realm is imported only when the container is CREATED, so after editing the realm file:
`docker rm -f crucible-test-keycloak && just dev-keycloak`.

With no issuer and no docker the tests print why they are skipping and pass. Set
`CRUCIBLE_REQUIRE_KEYCLOAK_TESTS=1` to make that skip a failure instead; CI sets it.

The suite also covers the offline credential (`src/oidc/credentials.rs`): the callback storing an
encrypted refresh token, two concurrent refreshes of one owner serializing under the per-subject
advisory lock, the schedule sweep re-reading live groups before it claims a due row, and a refused
refresh downgrading a live session. The realm grants `offline_access` and permits refresh-token
reuse for exactly that reason, so editing those settings out will fail those tests, not skip them.
