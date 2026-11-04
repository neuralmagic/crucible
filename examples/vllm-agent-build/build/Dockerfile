# vLLM built from source on a vLLM release image: CUDA dev packages and the build requirements,
# then the workspace's vllm/ tree, compiled in place through ccache.
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
RUN grep -v '^torch' /tmp/requirements/build/cuda.txt > /tmp/build-reqs.txt && \
    python3 -m pip install --no-cache-dir -r /tmp/build-reqs.txt
COPY vllm /src/vllm
WORKDIR /src/vllm
ENV CCACHE_DIR=/root/.cache/ccache
ARG TORCH_CUDA_ARCH_LIST=10.0
ARG MAX_JOBS=64
RUN TORCH_CUDA_ARCH_LIST="$TORCH_CUDA_ARCH_LIST" MAX_JOBS="$MAX_JOBS" python3 setup.py build_ext --inplace
