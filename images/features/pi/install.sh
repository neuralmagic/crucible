#!/usr/bin/env bash
set -euo pipefail

npm install -g "@earendil-works/pi-coding-agent@${PIN_PI}"
pi --version
