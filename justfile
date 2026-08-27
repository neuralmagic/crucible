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

# Build the whole Rust workspace.
build-loop:
    cargo build --release

# Score the agent-stream decoder (examples/selfhost's gate): ns/line over the synthetic corpus.
bench-stream:
    cargo bench -p crucible-harness --bench stream_json -q

# Lint + test the Rust workspace.
lint:
    cargo fmt --check && cargo clippy --workspace --all-targets && cargo test --workspace

# Run the pre-commit hooks over the whole tree (prek: https://github.com/j178/prek).
hooks:
    prek run --all-files

# End-to-end proof of forge-layer-capture: throwaway registry:2 + unprivileged userns capture in a
# Linux container (add a file, delete one) + docker-run verification of the derived digest ref.
# Observable step by step; re-runs are fast (cargo caches in named docker volumes).
forge-capture-e2e:
    scripts/forge-capture-e2e.sh

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
