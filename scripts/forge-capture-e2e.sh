#!/usr/bin/env bash
# End-to-end proof of forge-layer-capture against a REAL registry, observable step by step.
# Two cases, each: seed a base into a throwaway registry:2 with skopeo, capture a command with
# the unprivileged userns executor in a Linux container, skopeo-copy the derived digest ref
# back out, docker-load it, and run it to assert the diff took.
#   busybox — a file add + a deletion (whiteout), on a hardlinked-applet rootfs
#   python  — the real codegen workload: an editable pip install of a local package
#             (site-packages entries, a .pth, a generated console script)
# All registry traffic stays inside a docker network, so the host daemon never needs an
# insecure-registry config. Works on macOS (docker desktop/podman) and Linux. Re-runs are
# fast: cargo caches live in named volumes (forge-e2e-cargo / forge-e2e-target).
set -euo pipefail

NET=forge-e2e
REG=forge-e2e-registry
SKOPEO=quay.io/skopeo/stable:latest

docker network inspect "$NET" >/dev/null 2>&1 || docker network create "$NET" >/dev/null
docker rm -f "$REG" >/dev/null 2>&1 || true
docker run -d --name "$REG" --network "$NET" registry:2 >/dev/null
WORK_DIR=$(mktemp -d)
trap 'docker rm -f "$REG" >/dev/null 2>&1 || true; rm -rf "$WORK_DIR"' EXIT

# seed <public-ref> <repo>: copy a public base image into the throwaway registry.
seed() {
    docker run --rm --network "$NET" "$SKOPEO" copy --dest-tls-verify=false \
        "docker://$1" "docker://$REG:5000/$2:latest" >/dev/null
}

# capture <base-repo> <cmd> <push-repo> [forge-mount]: run forge-layer-capture in a Linux
# container and echo the digest-pinned derived ref. WORK_DIR is available inside the runner
# at /fixture; an optional forge-mount (HOST:CONTAINER, runner paths) binds source into the
# capture root.
capture() {
    local base="$1" cmd="$2" push="$3" fmount="${4:-}"
    local mount_arg=""
    [ -n "$fmount" ] && mount_arg="--mount $fmount"
    docker run --rm --network "$NET" \
        --security-opt seccomp=unconfined --security-opt apparmor=unconfined \
        --tmpfs /tmp:rw,exec,size=2g \
        -v "$PWD":/src -w /src \
        -v "$WORK_DIR":/fixture \
        -v forge-e2e-cargo:/usr/local/cargo/registry \
        -v forge-e2e-target:/ctarget \
        -e CARGO_TARGET_DIR=/ctarget \
        -e "FORGE_INSECURE_REGISTRIES=$REG:5000" \
        -e RUST_LOG=info \
        rust:1 sh -c "cargo run -q -p forge --bin forge-layer-capture -- \
            --base $REG:5000/$base:latest \
            --cmd '$cmd' \
            --push $REG:5000/$push \
            --cache /tmp/forge-capture \
            --executor userns $mount_arg" | tail -1
}

# run_derived <repo> <digest> <tag> <container-cmd...>: pull a derived digest ref out of the
# throwaway registry, docker-load it, run the given command in it, and echo the output.
run_derived() {
    local repo="$1" digest="$2" tag="$3"
    shift 3
    docker run --rm --network "$NET" -v "$WORK_DIR":/out "$SKOPEO" copy --src-tls-verify=false \
        "docker://$REG:5000/$repo@$digest" "docker-archive:/out/$tag.tar:$tag:latest" >/dev/null
    docker load -i "$WORK_DIR/$tag.tar" >/dev/null
    docker run --rm "$tag:latest" "$@"
    docker rmi "$tag:latest" >/dev/null 2>&1 || true
}

# ---- case 1: busybox (whiteouts + hardlinked applets) --------------------------------------
echo "==> [busybox] seeding base into $REG:5000/capture-base"
seed docker.io/library/busybox:latest capture-base

echo "==> [busybox] capturing a file add + a deletion (userns executor)"
REF=$(capture capture-base \
    'echo captured-by-forge > /e2e-proof.txt && rm /etc/group' \
    capture-derived)
echo "==> [busybox] derived: $REF"

echo "==> [busybox] verifying the derived image"
OUT=$(run_derived capture-derived "${REF##*@}" forge-e2e-derived \
    sh -c 'cat /e2e-proof.txt; test -e /etc/group && echo whiteout-MISSING || echo whiteout-applied')
echo "$OUT"
if [ "$OUT" != $'captured-by-forge\nwhiteout-applied' ]; then
    echo "FAIL: [busybox] derived image did not carry the expected diff"
    exit 1
fi
echo "PASS: [busybox] file added + deletion whiteout applied"

# ---- case 2: python (the codegen workload: editable pip install) ---------------------------
echo "==> [python] seeding base into $REG:5000/capture-python-base"
seed docker.io/library/python:3.12-slim capture-python-base

# The tiny local package fixture: a pyproject + one module with a console script.
mkdir -p "$WORK_DIR/forgepkg"
cat > "$WORK_DIR/forgepkg/pyproject.toml" <<'EOF'
[build-system]
requires = ["setuptools"]
build-backend = "setuptools.build_meta"

[project]
name = "forgepkg"
version = "0.1.0"

[project.scripts]
forgepkg-hello = "forgepkg:main"

[tool.setuptools]
py-modules = ["forgepkg"]
EOF
cat > "$WORK_DIR/forgepkg/forgepkg.py" <<'EOF'
def main():
    print("hello-from-forgepkg")
EOF

echo "==> [python] capturing an editable pip install of the fixture package"
# The source is copied into the image first so the editable .pth resolves in the derived
# image — the same shape as the real codegen flow (copy source, pip install -e).
REF=$(capture capture-python-base \
    'cp -r /workspace/forgepkg /opt/forgepkg && pip install --no-cache-dir -e /opt/forgepkg' \
    capture-python-derived \
    /fixture/forgepkg:/workspace/forgepkg)
echo "==> [python] derived: $REF"

echo "==> [python] verifying import + console script in the derived image"
OUT=$(run_derived capture-python-derived "${REF##*@}" forge-e2e-python-derived \
    sh -c 'python -c "import forgepkg; forgepkg.main()" && forgepkg-hello')
echo "$OUT"
if [ "$OUT" != $'hello-from-forgepkg\nhello-from-forgepkg' ]; then
    echo "FAIL: [python] derived image did not carry the pip install"
    exit 1
fi
echo "PASS: [python] editable pip install round-tripped (import + console script work)"

echo "PASS: all capture e2e cases"
