#!/usr/bin/env bash
set -euo pipefail

if [ ! -e /usr/local/cuda ]; then
    ln -s "$(ls -d /usr/local/cuda-[0-9]* | sort -V | tail -1)" /usr/local/cuda
fi
/usr/local/cuda/bin/nvcc --version
