#!/bin/sh
# One agent turn: pi in a container that sees only this workspace. The model comes from a
# gateway that speaks the OpenAI chat completions API.
set -eu
: "${VIEW_AGENT_URL:?the model gateway's base URL}"
: "${VIEW_AGENT_KEY:?the model gateway's key}"
: "${VIEW_AGENT_MODEL:?the model to ask for}"

podman run --rm --volume "$PWD":/work --workdir /work \
    --env CRUCIBLE_PROMPT --env VIEW_AGENT_URL --env VIEW_AGENT_KEY --env VIEW_AGENT_MODEL \
    "${VIEW_AGENT_IMAGE:-localhost/crucible-view-agent}" turn > agent.log 2>&1 || true

if [ ! -s report.view.js ]; then
    tail -n 40 agent.log >&2
    exit 1
fi
printf '{"wrote": "report.view.js", "bytes": %s}\n' "$(wc -c < report.view.js | tr -d ' ')" \
    | tee PLAN_TASK_RESULT.json
