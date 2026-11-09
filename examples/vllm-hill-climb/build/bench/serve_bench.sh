#!/bin/sh
# Serves /bench/model with dummy weights on one GPU, runs `vllm bench serve` against it, and prints
# one line: hill-climb-serve {"output_throughput": ..., "median_tpot_ms": ..., ...}
set -eu
PROMPTS=${SERVE_PROMPTS:-200}
CONCURRENCY=${SERVE_CONCURRENCY:-32}
mkdir -p /tmp/serve
python3 -m vllm.entrypoints.cli.main serve /bench/model \
    --load-format dummy --max-model-len 4096 --gpu-memory-utilization 0.92 \
    --port 8000 > /tmp/serve/server.log 2>&1 &
server=$!
trap 'kill $server 2> /dev/null || true' EXIT
up=0
for _ in $(seq 1 180); do
    if python3 -c "import urllib.request; urllib.request.urlopen('http://127.0.0.1:8000/health', timeout=2)" 2> /dev/null; then
        up=1
        break
    fi
    if ! kill -0 $server 2> /dev/null; then
        break
    fi
    sleep 5
done
if [ "$up" != 1 ]; then
    tail -n 80 /tmp/serve/server.log
    echo 'hill-climb-serve {"ok": false}'
    exit 1
fi
python3 -m vllm.entrypoints.cli.main bench serve --backend vllm --model /bench/model \
    --base-url http://127.0.0.1:8000 --dataset-name random --random-input-len 1024 \
    --random-output-len 256 --num-prompts "$PROMPTS" --max-concurrency "$CONCURRENCY" \
    --seed 20261109 --save-result --result-dir /tmp/serve --result-filename result.json
python3 - <<'EOF'
import json
r = json.load(open("/tmp/serve/result.json"))
keys = ["completed", "output_throughput", "request_throughput", "median_ttft_ms",
        "median_tpot_ms", "median_itl_ms", "duration"]
print("hill-climb-serve " + json.dumps({"ok": True, **{k: r.get(k) for k in keys}}, sort_keys=True))
EOF
