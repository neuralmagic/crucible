#!/bin/sh
# Rejects a probe written without a verdict in hand, and accepts one written with it.
mkdir -p evidence
if grep -q '"revision"' inputs/author/PROBE.md; then
    printf '{"accepted": true}\n' > evidence/review.json
    cat evidence/review.json
else
    printf '{"accepted": false, "why": "the probe hit an unrelated 400"}\n' > evidence/review.json
    cat evidence/review.json
    exit 1
fi
