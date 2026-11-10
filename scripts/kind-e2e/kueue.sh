# shellcheck shell=bash
# Sourced by scripts/kind-e2e.sh as the kueue shard.

# ---- scenario KQ ----------------------------------------------------------------------------
KUEUE_NS=crucible-e2e-kueue
WORKLOAD_NS=crucible-e2e-workloads
QUEUE=crucible-e2e
NONCE=$(openssl rand -hex 12)

# The loop pod and crucible-kueue reach the controller on the host: the pod for its task tokens,
# the server for the keys that verify them.
Q_HOST=$(host_address) || fail "[KQ] no address the kind network reaches the host on"
ISSUER="http://$Q_HOST:$PORT"

log "[KQ] installing Kueue $KUEUE_VERSION"
kubectl apply --server-side -f "https://github.com/kubernetes-sigs/kueue/releases/download/$KUEUE_VERSION/manifests.yaml" >"$WORK/kueue-install.log" 2>&1 ||
    fail "[KQ] installing Kueue: $(tail -n 20 "$WORK/kueue-install.log")"
kubectl -n kueue-system rollout status deploy/kueue-controller-manager --timeout=300s >/dev/null ||
    fail "[KQ] Kueue never became ready: $(kubectl -n kueue-system get pods -o wide)"
KUEUE_API=$(kubectl get crd clusterqueues.kueue.x-k8s.io -o json | jq -r '.spec.versions[] | select(.storage) | .name')

sed "s|$LOOP_IMAGE|$KUEUE_LOOP_IMAGE|g" "$WORK/profile.toml" |
    awk -v url="$ISSUER" '{ print } /^\[cluster\]$/ { print "ingest_url = \"" url "\"" }' >"$WORK/profile-kueue.toml"
cat >>"$WORK/profile-kueue.toml" <<PROFILE

[kueue]
image              = "$KUEUE_IMAGE"
cluster            = "hub"
namespace          = "$KUEUE_NS"
workload_namespace = "$WORKLOAD_NS"
shared_namespaces  = ["$WORKLOAD_NS"]
PROFILE

log "[KQ] deploying crucible-kueue in $KUEUE_NS, trusting $ISSUER"
kubectl create namespace "$KUEUE_NS" >/dev/null
"$BIN/crucible" deploy render --kueue --issuer "$ISSUER" --no-pin --profile "$WORK/profile-kueue.toml" \
    >"$WORK/kueue.yaml" 2>"$WORK/kueue-render.log" || fail "[KQ] rendering crucible-kueue: $(cat "$WORK/kueue-render.log")"
kubectl apply -f "$WORK/kueue.yaml" >/dev/null
# queues: the flavor, ClusterQueue and LocalQueue the run submits to. Kueue's webhook refuses them
# until it serves.
queues() {
    kubectl apply -f - <<QUEUES
apiVersion: kueue.x-k8s.io/$KUEUE_API
kind: ResourceFlavor
metadata:
  name: default
---
apiVersion: kueue.x-k8s.io/$KUEUE_API
kind: ClusterQueue
metadata:
  name: $QUEUE
spec:
  namespaceSelector: {}
  resourceGroups:
    - coveredResources: [cpu, memory]
      flavors:
        - name: default
          resources:
            - { name: cpu, nominalQuota: 4 }
            - { name: memory, nominalQuota: 8Gi }
---
apiVersion: kueue.x-k8s.io/$KUEUE_API
kind: LocalQueue
metadata:
  name: $QUEUE
  namespace: $WORKLOAD_NS
spec:
  clusterQueue: $QUEUE
QUEUES
}
wait_for 120 "Kueue to accept the queues" queues
kubectl -n "$KUEUE_NS" rollout status deploy/crucible-kueue --timeout=300s >/dev/null ||
    fail "[KQ] crucible-kueue never became ready: $(kubectl -n "$KUEUE_NS" get pods -o wide; kubectl -n "$KUEUE_NS" logs deploy/crucible-kueue --tail=20 2>&1)"

# The cluster has no SPIRE: a run's tasks get their servers and tokens without one. The issuer
# refuses to start unless its own namespace is none that pack code reaches, so the host
# controller names one.
openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out "$WORK/agent-signing.pem" 2>/dev/null
export CONTROLLER_API_TOKEN="e2e-$(openssl rand -hex 16)"
CONTROLLER_ENV=(CONTROLLER_API_ADDR="0.0.0.0:$PORT" CONTROLLER_DEV_IDENTITY= CONTROLLER_API_TOKEN_IDENTITY=e2e
    CONTROLLER_AGENT_ISSUER="$ISSUER" CONTROLLER_AGENT_SIGNING_KEY_FILE="$WORK/agent-signing.pem"
    POD_NAMESPACE=crucible-e2e-controller)
start_controller "$WORK/profile-kueue.toml"

# kueue_pack <dir> <servers>: the kueue pack with its placeholders filled, naming those servers.
kueue_pack() {
    cp -R "$FIX/packs/kueue" "$1"
    sed -i.bak -e "s|@IMAGE@|$KUEUE_LOOP_IMAGE|g" -e "s|@NONCE@|$NONCE|g" -e "s|@QUEUE@|$QUEUE|g" "$1/probe.py" "$1/serve.py"
    sed -i.bak "s|^servers .*|servers = $2|" "$1/crucible.toml"
    rm "$1"/*.bak
}

log "[KQ] a command task submits a Job through crucible-kueue with its task token"
kueue_pack "$WORK/kueue-pack" '["kueue"]'
new_draft "$DRAFT-kueue" "$WORK/kueue-pack"
watch_start KQ
launch KQ draft-launch "$DRAFT-kueue"
settle KQ "$KEY"
watch_stop
RUN=$(jq -r '.runs[0].run_id' "$WORK/run-KQ.json")
POD=$(sql -c "SELECT pod FROM runs WHERE run_id = '$RUN'")
api GET "/api/runs/$RUN/log"
jq -r '.text // empty' "$WORK/api.json" >"$WORK/engine-KQ.log"
[ "$STATUS" = "done" ] || fail "[KQ] launch ended '$STATUS': $(jq -c '.launch' "$WORK/run-KQ.json"); the engine said: $(tail -n 30 "$WORK/engine-KQ.log")"
expect_finished KQ

kubectl -n "$WORKLOAD_NS" get job task-probe -o json >"$WORK/job-KQ.json" ||
    fail "[KQ] no Job task-probe in $WORKLOAD_NS"
ACTOR="crucible:run:$POD:task:probe"
jq -e --arg run "$POD" --arg actor "$ACTOR" --arg queue "$QUEUE" '
    .metadata.labels as $l
    | $l["crucible.dev/run-id"] == $run and $l["crucible.dev/launched-by"] == "e2e"
      and $l["crucible.dev/owner-kind"] == "run" and $l["kueue.x-k8s.io/queue-name"] == $queue
      and .metadata.annotations["crucible.dev/actor"] == $actor
      and .status.succeeded == 1' "$WORK/job-KQ.json" >/dev/null ||
    fail "[KQ] the Job is not the run's, by $ACTOR: $(jq -c '{labels: .metadata.labels, annotations: .metadata.annotations, status}' "$WORK/job-KQ.json")"
kubectl -n "$WORKLOAD_NS" logs job/task-probe | grep -qx "$NONCE" || fail "[KQ] the Job's log does not carry the nonce"
pass "[KQ] the Job ran on Kueue, labelled with the run, its launcher and the task, and printed the nonce"

grep -F "task token issued" "$WORK/controller-$BOOT.log" | grep -F "crucible:run:$POD" >"$WORK/issued-KQ.log" ||
    fail "[KQ] the controller issued no task token for $POD"
grep -qF 'Task("probe")' "$WORK/issued-KQ.log" || fail "[KQ] the token does not name the task: $(cat "$WORK/issued-KQ.log")"
grep -qF "agent token issued" "$WORK/controller-$BOOT.log" && fail "[KQ] a sandbox was issued a token"
pass "[KQ] the controller issued the token to the run for task probe, and none to a sandbox"

api GET "/api/runs/$RUN/graph"
[ "$(jq -r '.results[] | select(.task == "probe") | .status' "$WORK/api.json")" = pass ] ||
    fail "[KQ] probe did not pass: $(jq -c '.results' "$WORK/api.json")"
grep -qF "\"kube_user\": \"crucible:run:$POD\"" "$WORK/engine-KQ.log" && grep -qF '"nonce_logged": true' "$WORK/engine-KQ.log" ||
    fail "[KQ] the task's whoami and log read are not in the engine log: $(tail -n 30 "$WORK/engine-KQ.log")"
pass "[KQ] the task acted as crucible:run:$POD and read the Job's log through the server"

SERVE_HOST="task-serve.$WORKLOAD_NS.svc.cluster.local"
grep '^\[serve\] {' "$WORK/engine-KQ.log" | sed 's/^\[serve\] //' >"$WORK/serve-KQ.json"
jq -e --arg host "$SERVE_HOST" '
    .url == "http://\($host):8000" and .resubmitted == false
    and .status_service == {name: "task-serve", host: $host, ports: [8000]}
    and .from_task and .from_job and .batch_refused and .gone
    and .ticks == ["tick-1", "tick-2", "tick-3", "tick-4", "tick-5", "tick-6"]
    and .raised.failed.phase == "finished" and (.raised.failed.reason | length > 0)
    and .raised.failed.reason != "Succeeded" and .raised.stuck.problem_pods >= 1' "$WORK/serve-KQ.json" >/dev/null ||
    fail "[KQ] serve did not reach its workload and retire it: $(cat "$WORK/serve-KQ.json"); the engine said: $(tail -n 20 "$WORK/engine-KQ.log")"
wait_for 60 "the exposed Service to be collected with its Deployment" \
    sh -c "! kubectl -n $WORKLOAD_NS get service task-serve"
[ "$(kubectl -n "$WORKLOAD_NS" get services -o json | jq '.items | length')" = 0 ] ||
    fail "[KQ] a Service outlived its workload: $(kubectl -n "$WORKLOAD_NS" get services -o name)"
pass "[KQ] a task served an image at $SERVE_HOST, reached it from itself and from a Job, and its Service went with it"
pass "[KQ] a task followed a running Job's log in two-second windows and read each line once"
pass "[KQ] a Job that exits nonzero raised WorkloadFailed ($(jq -r .raised.failed.reason "$WORK/serve-KQ.json")) and one that cannot pull raised WorkloadStuck"

# A JWT's header and payload start with base64 of {" .
jq -s --arg n "$POD" '[.[] | .object | select(.metadata.name == $n)]' "$WORK/pods-KQ.json" >"$WORK/pod-KQ.json"
jq -e '[.[].spec.containers[0].env[].name] | index("CRUCIBLE_RUN_SERVERS") != null and index("CRUCIBLE_AGENT_SERVERS") == null' "$WORK/pod-KQ.json" >/dev/null ||
    fail "[KQ] without SPIRE the run pod should carry its tasks' servers and no sandbox servers: $(jq -c '[.[].spec.containers[0].env[].name | select(startswith("CRUCIBLE_"))] | unique' "$WORK/pod-KQ.json")"
grep -qE 'eyJ[A-Za-z0-9_-]{10,}\.eyJ' "$WORK/pod-KQ.json" && fail "[KQ] a token rides the run pod's spec"
grep -qF CRUCIBLE_TASK_SERVERS "$WORK/pod-KQ.json" && fail "[KQ] the task's servers ride the run pod's spec"
grep -qE 'eyJ[A-Za-z0-9_-]{10,}\.eyJ' "$WORK/engine-KQ.log" "$WORK/controller-$BOOT.log" &&
    fail "[KQ] a token is in the engine's or the controller's log"
kubectl -n "$KUEUE_NS" logs deploy/crucible-kueue >"$WORK/kueue-server-KQ.log" 2>&1
grep -qE 'eyJ[A-Za-z0-9_-]{10,}\.eyJ' "$WORK/kueue-server-KQ.log" && fail "[KQ] a token is in crucible-kueue's log"
pass "[KQ] the token is in no pod spec and no log"

log "[KQ] a pack naming a server the run was not granted fails its task"
kueue_pack "$WORK/kueue-pack-ungranted" '["notes"]'
new_draft "$DRAFT-kueue-ungranted" "$WORK/kueue-pack-ungranted"
launch KQ-ungranted draft-launch "$DRAFT-kueue-ungranted"
settle KQ-ungranted "$KEY"
UNGRANTED=$(jq -r '.runs[0].run_id' "$WORK/run-KQ-ungranted.json")
api GET "/api/runs/$UNGRANTED/graph"
jq -e '.results[] | select(.task == "probe") | .status != "pass"' "$WORK/api.json" >/dev/null ||
    fail "[KQ] probe passed without its server: $(jq -c '.results' "$WORK/api.json")"
jq -r '.results[] | select(.task == "probe") | .note // ""' "$WORK/api.json" |
    grep -qF '[workflow] servers names "notes", but the controller granted this run ["kueue"]' ||
    fail "[KQ] the failure does not name the servers the run was granted: $(jq -c '.results' "$WORK/api.json")"
[ "$(kubectl -n "$WORKLOAD_NS" get jobs -o json | jq '.items | length')" = 4 ] ||
    fail "[KQ] the ungranted pack left a Job"
pass "[KQ] a server the run was not granted fails the task naming the granted ones, and submits nothing"
stop_controller
