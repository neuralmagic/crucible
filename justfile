# crucible, goal-directed autoresearch loops over real systems. The engine is domain-neutral;
# domain packs live in their own (private or public) repos and are mounted at runtime.
# See README.md and docs/ (the mdbook) for the architecture.

set shell := ["bash", "-uc"]

cargo_bin := env_var_or_default("CARGO_HOME", env_var("HOME") + "/.cargo") + "/bin"

# List recipes.
default:
    @just --list

# The OpenShell fork rev pinned in Cargo.lock (openshell-core's git source). The runtime image
# takes it as --build-arg OPENSHELL_REV so its gateway/CLI match the rev crucible's gRPC
# client compiled against, e.g.:
#   docker build -f Containerfile.runtime --build-arg OPENSHELL_REV=$(just openshell-rev) .
openshell-rev:
    @cargo xtask openshell-rev

# Build the mdBook doc site (docs/ -> book/). Same mdbook version as CI.
book:
    mdbook build

# Serve the docs locally with live reload; opens a browser.
book-serve:
    mdbook serve --open

# Symlink the generic control-plane nu tools (tools/*.nu: stop/steer/escalate/session/
# goal-from-issue) onto PATH as bare names, so edits to a .nu are live.
install-tools:
    mkdir -p "{{cargo_bin}}"
    for f in tools/*.nu; do [ -e "$f" ] && ln -sf "$PWD/$f" "{{cargo_bin}}/$(basename "$f" .nu)"; done
    @echo "linked tools -> {{cargo_bin}}"

# Build the whole Rust workspace, the scored loop included.
build-loop:
    cargo build --release --features crucible/autoresearch

# Score the agent-stream decoder (examples/selfhost's gate): ns/line over the synthetic corpus.
bench-stream:
    cargo bench -p crucible-harness --bench stream_json -q

# Lint + test the Rust workspace.
lint:
    cargo fmt --check && cargo clippy --workspace --all-targets --all-features && cargo clippy -p crucible --all-targets && cargo test --workspace --all-features && cargo test -p crucible

# Module dependency graph of one crate (crucible by default; `--root crucible-controller/src`
# for the controller): cycles, fan-in/out, duplicate item names. `just modgraph --check` fails
# on any module cycle; CI runs that over both crates.
modgraph *ARGS:
    cargo run --quiet -p xtask -- modgraph {{ARGS}}

# Scoped controller dev loop: build/test/lint only crucible-controller. The sqlx tests need a
# Postgres at DATABASE_URL (`just dev-pg`) and SQLX_OFFLINE=true so the macros compile against
# the checked-in `.sqlx/` cache rather than whatever the dev database was last migrated to.
build-controller:
    SQLX_OFFLINE=true cargo build -p crucible-controller
    SQLX_OFFLINE=true cargo build -p crucible-controller --features autoresearch

test-controller:
    SQLX_OFFLINE=true cargo test -p crucible-controller
    SQLX_OFFLINE=true cargo test -p crucible-controller --features autoresearch
    SQLX_OFFLINE=true cargo test -p crucible-controller --features autoresearch --lib -- --ignored an_idle_tick_creates_no_span_but_an_ingest_still_traces

lint-controller:
    cargo fmt --check && SQLX_OFFLINE=true cargo clippy -p crucible-controller -p crux --all-targets --all-features -- -D warnings
    SQLX_OFFLINE=true cargo clippy -p crucible-controller -p crux --all-targets -- -D warnings

# A throwaway Postgres for the controller's sqlx tests (DATABASE_URL=postgres://postgres:ci@localhost:55432/crucible).
dev-pg:
    #!/usr/bin/env bash
    set -euo pipefail
    if ! docker inspect crucible-test-pg >/dev/null 2>&1; then
        docker run -d --name crucible-test-pg -e POSTGRES_PASSWORD=ci -e POSTGRES_DB=crucible \
            -p 55432:5432 postgres:16 -c max_connections=400 >/dev/null
        echo "created crucible-test-pg"
    else
        docker start crucible-test-pg >/dev/null
    fi
    echo "DATABASE_URL=postgres://postgres:ci@localhost:55432/crucible"

# See docs/controller-local.md.
# Run the controller on this machine: embedded Postgres, no auth, no Vault, no cluster.
controller-local port="8870" user=env_var("USER"):
    #!/usr/bin/env bash
    set -euo pipefail
    (cd crucible-controller/ui && bun install --frozen-lockfile && bun run build)
    SQLX_OFFLINE=true cargo build -p crucible -p crucible-controller -p crux --bins \
        --features crucible-controller/embedded-db
    bin="${CARGO_TARGET_DIR:-$PWD/target}/debug"
    state="${XDG_STATE_HOME:-$HOME/.local/state}/crucible-controller"
    mkdir -p "$state"
    if [ -z "${OPENSHELL_PODMAN_SOCKET:-}" ] && command -v podman >/dev/null \
        && podman machine inspect >/dev/null 2>&1; then
        export OPENSHELL_PODMAN_SOCKET=$(podman machine inspect --format '{{"{{"}}.ConnectionInfo.PodmanSocket.Path{{"}}"}}')
    fi
    echo "UI  http://127.0.0.1:{{port}} as {{user}}"
    echo "CLI {{ if port != "8870" { "CONTROLLER_URL=http://127.0.0.1:" + port + " " } else { "" } }}$bin/crux whoami"
    exec env -u CONTROLLER_API_TOKEN -u CONTROLLER_PROXY_TOKEN -u CONTROLLER_OIDC_ISSUER -u VAULT_ADDR \
        KUBECONFIG=/dev/null \
        DATABASE_URL=embedded \
        CONTROLLER_API_ADDR=127.0.0.1:{{port}} \
        CONTROLLER_PUBLIC_URL=http://127.0.0.1:{{port}} \
        CONTROLLER_DEV_IDENTITY={{user}} \
        CONTROLLER_ADMINS={{user}} \
        CONTROLLER_AUTH_MODE=proxy \
        CONTROLLER_SESSION_SECURE=false \
        CONTROLLER_PLAYBOOK_EXECUTOR=local \
        CONTROLLER_SCOPE_EXECUTOR=disabled \
        CONTROLLER_STATE_DIR="$state" \
        CRUCIBLE_BIN="$bin/crucible" \
        RUST_LOG="${RUST_LOG:-info}" \
        "$bin/crucible-controller" autopilot

# Regenerate the controller UI's OpenAPI spec + typed client (build artifacts, not committed):
# `cargo run -p crucible-controller --bin openapi-spec` -> openapi.json -> openapi-typescript.
# The UI's dev/check/test/build scripts run this themselves via pre-hooks.
ui-types:
    cd crucible-controller/ui && bun run generate

# Regenerate the sandbox images under images/generated/ (one Containerfile and one INTRO.md per
# image) from images/features/ + images/matrix.toml.
gen-images:
    cargo xtask images gen

# Regenerate docs/dsl-reference.md from the compiler's DSL tables (the pre-commit hook's job,
# for when you want it without a commit).
dsl-docs:
    ./scripts/dsl-docs.sh

# Regenerate docs/loop-states.md and docs/plan-states.md from the engine's transition tables.
state-docs:
    ./scripts/state-docs.sh

# Render the published RFCs from gov/ (the pre-commit hook's job, without a commit).
gov-docs:
    govctl render rfc

# Run the pre-commit hooks over the whole tree (prek: https://github.com/j178/prek).
hooks:
    prek run --all-files

# End-to-end proof of forge-layer-capture: throwaway registry:2 + unprivileged userns capture in a
# Linux container (add a file, delete one) + docker-run verification of the derived digest ref.
# Observable step by step; re-runs are fast (cargo caches in named docker volumes).
forge-capture-e2e:
    scripts/forge-capture-e2e.sh

# End-to-end proof of a revise loop through a real OpenShell sandbox on local podman, with a
# model-free `claude` image (examples/revise-loop). Needs podman, openshell, openshell-gateway.
revise-loop-e2e:
    scripts/revise-loop-e2e.sh

# Spoke smoketest (hub-spoke delegated jobs): submit a CPU-only sentinel Job to <cluster> through
# the full production submit/stream/parse path and print the typed result JSON. `cluster` is a
# [clusters.<name>] entry in the deploy profile; pass `context` to run it against a local
# kubecontext instead of the in-cluster fleet secret:
#   just spoke-smoke gpu-east crucible-system crucible-measure my-kubecontext
spoke-smoke cluster namespace="crucible-system" queue="crucible-measure" context="":
    cargo run -q -p crucible-broker --bin crucible-broker -- spoke-smoke {{cluster}} \
      --namespace {{namespace}} --queue {{queue}} {{ if context != "" { "--context " + context } else { "" } }}

# Steer a running loop: append guidance picked up before the next iteration (audited).
steer text source="operator": install-tools
    steer "{{text}}" --source {{source}}

# Park a running loop: it keeps its best, then exits.
stop source="operator": install-tools
    stop --source {{source}}

# Read-only JSON snapshot of the running (or finished) loop session.
session ws="workspace": install-tools
    session --workspace {{ws}}

# Build the DiffusionGemma structured-reads vLLM image on waldorf; prints the build job's name.
route-serving-image tag="dgemma-reads" namespace="weaton-dev" context="coreweave-waldorf":
    buildit build quay.io/wseaton/vllm:{{tag}} -n {{namespace}} --kubecontext {{context}} \
        -c examples/route/serving -f Containerfile --mode job --request cpu=8 --request memory=32Gi

# Deploy that image with the System One server in front of it, and wait for it to load the model.
route-serving-deploy image namespace="weaton-dev" context="coreweave-waldorf":
    sed 's|image: IMAGE|image: {{image}}|' examples/route/serving/deploy.yaml \
        | kubectl --context {{context}} -n {{namespace}} apply -f -
    kubectl --context {{context}} -n {{namespace}} rollout status deploy/dgemma-systemone --timeout=45m

# Forward the System One endpoint to localhost:8011 for `examples/route`.
route-serving-forward namespace="weaton-dev" context="coreweave-waldorf":
    kubectl --context {{context}} -n {{namespace}} port-forward svc/dgemma-systemone 8011:8011

# Remove the deployment and free its GPU.
route-serving-down namespace="weaton-dev" context="coreweave-waldorf":
    kubectl --context {{context}} -n {{namespace}} delete deploy/dgemma-systemone svc/dgemma-systemone --ignore-not-found
