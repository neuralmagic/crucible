#!/usr/bin/env bash
set -euo pipefail

uv venv --python python3.12 /opt/vllm
uv pip install --python /opt/vllm/bin/python --no-cache-dir "vllm==${PIN_VLLM}"
/opt/vllm/bin/python -c "import vllm; print(vllm.__version__)"
