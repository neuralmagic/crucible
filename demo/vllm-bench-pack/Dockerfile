# vLLM built from source on a vLLM release image: CUDA dev packages and the build requirements,
# then the context's vllm/ tree, compiled in place through ccache.
ARG BASE
FROM ${BASE}
USER root
RUN set -eu; \
    v=$(echo "$CUDA_VERSION" | cut -d. -f1,2 | tr . -); \
    for f in /etc/apt/sources.list /etc/apt/sources.list.d/*; do \
        if [ -f "$f" ]; then sed -i 's|http://|https://|g' "$f"; fi; \
    done; \
    apt-get update --error-on=any; \
    apt-get install -y --no-install-recommends git g++ ccache libdw-dev \
        cuda-nvrtc-dev-$v cuda-cudart-dev-$v libcusparse-dev-$v libcusolver-dev-$v \
        libcublas-dev-$v libcurand-dev-$v libcufft-dev-$v; \
    rm -rf /var/lib/apt/lists/*
COPY vllm/requirements /tmp/requirements
RUN python3 -m pip install --no-cache-dir uv && \
    tv=$(python3 -c "import torch; print(torch.__version__)") && \
    echo "torch==$tv" > /tmp/torch-constraint.txt && \
    grep -v '^torch' /tmp/requirements/build/cuda.txt > /tmp/build-reqs.txt && \
    if [ -s /reqlock/requirements.lock ]; then \
        uv pip install --system -r /reqlock/requirements.lock; \
    else \
        uv pip compile -c /tmp/torch-constraint.txt -o /reqlock/requirements.lock /tmp/build-reqs.txt && \
        uv pip install --system -r /reqlock/requirements.lock; \
    fi
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal >/dev/null && "$HOME/.cargo/bin/rustc" --version
ENV PATH="/root/.cargo/bin:${PATH}"
COPY vllm /src/vllm
WORKDIR /src/vllm
ENV CCACHE_DIR=/root/.cache/ccache
ARG CUDA_ARCH_LIST=10.0
ARG MAX_JOBS=64
RUN TORCH_CUDA_ARCH_LIST="$CUDA_ARCH_LIST" MAX_JOBS="$MAX_JOBS" python3 setup.py build_ext --inplace
ARG ARTIFACT=""
ARG REGISTRY=""
RUN if [ -n "$ARTIFACT" ]; then \
        cd /src/vllm && \
        tar -cf /tmp/artifact.tar --exclude=.deps --exclude=build --exclude=rust/target . && \
        d=sha256:$(sha256sum /tmp/artifact.tar | cut -d" " -f1) && \
        loc=$(curl -sf -X POST "$REGISTRY/v2/artifacts/blobs/uploads/" -o /dev/null -w "%header{location}") && \
        case "$loc" in http*) : ;; /*) loc="$REGISTRY$loc" ;; *) loc="$REGISTRY/$loc" ;; esac && \
        sep=? && case "$loc" in *\?*) sep="&" ;; esac && \
        curl -sf -X PUT -H "Content-Type: application/octet-stream" -T /tmp/artifact.tar "$loc${sep}digest=$d" && \
        printf '{"artifact_digest": "%s", "artifact_bytes": %s}\n' "$d" "$(wc -c < /tmp/artifact.tar)" > /buildit/facts.json && \
        echo "artifact $d at $REGISTRY/v2/artifacts" && \
        rm -f /tmp/artifact.tar; \
    fi
