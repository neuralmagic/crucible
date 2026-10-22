# shellcheck shell=bash
# Sourced by scripts/kind-e2e.sh as the pods shard.
start_controller

# ---- scenario H ----------------------------------------------------------------------------
SA_REF="system:serviceaccount:$NS:crucible-loop"
TLS_PREFIX=crucible-openshell-client-tls-

# secrets_watch_start <watch label>: record Secret events in $WORK/secrets-<label>.json until
# watch_stop.
secrets_watch_start() {
    kubectl -n "$NS" get secrets -w --output-watch-events -o json >"$WORK/secrets-$1.json" 2>/dev/null &
    BG_PIDS+=($!)
}

# tls_secrets <watch label>: every client-TLS Secret the watch saw created, as one JSON array.
tls_secrets() {
    jq -cs --arg p "$TLS_PREFIX" '[.[] | select(.type == "ADDED") | .object | select(.metadata.name | startswith($p))] | unique_by(.metadata.uid)' "$WORK/secrets-$1.json"
}

# tls_secret_published <watch label>: the watch saw a client-TLS Secret created.
tls_secret_published() { [ "$(tls_secrets "$1" | jq length)" -gt 0 ]; }

# tls_secret_count <n>: exactly n client-TLS Secrets exist.
tls_secret_count() {
    [ "$(kubectl -n "$NS" get secrets -o json | jq --arg p "$TLS_PREFIX" '[.items[] | select(.metadata.name | startswith($p))] | length')" = "$1" ]
}

# pod_uid <watch label> <pod>: the uid of the one pod of that name the watch saw.
pod_uid() {
    jq -rs --arg n "$2" '[.[] | .object | select(.metadata.name == $n) | .metadata.uid] | unique | if length == 1 then .[0] else empty end' "$WORK/pods-$1.json"
}

# expect_pod_secrets <label> <pod> <watch label>: every client-TLS Secret the watch saw is named
# for the pod and a distinct boot (each transport retry boots again), owned by the pod, and carries
# the mTLS keys the driver mounts.
expect_pod_secrets() {
    local s uid
    s=$(tls_secrets "$3")
    jq -e --arg n "$TLS_PREFIX$2-" 'length > 0 and all(.[]; .metadata.name | startswith($n) and (ltrimstr($n) | test("^[0-9a-f]{12}$")))' <<<"$s" >/dev/null ||
        fail "[$1] a Secret is not named for pod $2 and one boot: $(jq -c 'map(.metadata.name)' <<<"$s")"
    [ "$(jq '[.[].metadata.name] | unique | length' <<<"$s")" = "$(jq length <<<"$s")" ] ||
        fail "[$1] two boots published the same Secret name: $(jq -c 'map(.metadata.name)' <<<"$s")"
    uid=$(pod_uid "$3" "$2")
    [ -n "$uid" ] || fail "[$1] the watch did not see exactly one pod $2"
    jq -e --arg pod "$2" --arg uid "$uid" 'all(.[]; [.metadata.ownerReferences[] | [.kind, .name, .uid]] == [["Pod", $pod, $uid]])' <<<"$s" >/dev/null ||
        fail "[$1] a Secret is not owned by pod $2 ($uid): $(jq -c 'map(.metadata.ownerReferences)' <<<"$s")"
    jq -e 'all(.[]; (.data | keys) == ["ca.crt", "tls.crt", "tls.key"])' <<<"$s" >/dev/null ||
        fail "[$1] a Secret carries other keys: $(jq -c 'map(.data | keys)' <<<"$s")"
    pass "[$1] pod $2 published $(jq -r 'map(.metadata.name) | join(", ")' <<<"$s"), owned by the pod"
}

log "[H] granting the loop SA the rendered sandbox Role and nothing else"
GW_PACK="$WORK/gateway"
cp -R "$FIX/packs/gateway" "$GW_PACK"
"$BIN/crucible" deploy render --manifest "$GW_PACK/crucible.toml" --profile "$WORK/profile.toml" \
    --no-pin --playbook --max-cost 1 --max-time 5m >"$WORK/render-H.yaml"
kubectl create --dry-run=client -o json -f "$WORK/render-H.yaml" |
    jq -s '{apiVersion: "v1", kind: "List", items: [.[] | if .kind == "List" then .items[] else . end
        | select((.kind == "Role" or .kind == "RoleBinding") and .metadata.name == "crucible-loop-sandbox")]}' |
    kubectl apply -f - >/dev/null
for verb in get patch update; do
    [ "$(kubectl auth can-i "$verb" secrets -n "$NS" --as "$SA_REF" 2>/dev/null)" = no ] ||
        fail "[H] the loop SA can $verb secrets"
done
[ "$(kubectl auth can-i create secrets -n "$NS" --as "$SA_REF")" = yes ] || fail "[H] the loop SA cannot create secrets"
pass "[H] the loop SA may create Secrets and may not get, patch, or update them"

log "[H] a controller-dispatched run pod boots the gateway"
new_draft "$DRAFT-gateway" "$GW_PACK"
watch_start H
secrets_watch_start H
launch H draft-launch "$DRAFT-gateway"
settle H "$KEY"
watch_stop
tls_secret_published H || fail "[H] the run pod's gateway published no client-TLS Secret"
H_POD=$(tls_secrets H | jq -r '.[0].metadata.ownerReferences[0].name')
[ "$(jq -rs --arg n "$H_POD" '[.[] | .object | select(.metadata.name == $n) | .metadata.labels["crucible.dev/issue-key"]] | unique | join(" ")' "$WORK/pods-H.json")" = "$(label "$KEY")" ] ||
    fail "[H] the Secret's owner $H_POD is not the launch's pod"
expect_pod_secrets H "$H_POD" H
reason=$(jq -r '.launch.parked_reason // empty' "$WORK/run-H.json")
[ "$STATUS" = parked ] && grep -q 'gateway' <<<"$reason" || fail "[H] expected a park at the gateway, got '$STATUS': $reason"
grep -qi -e forbidden -e 'publishing Secret' <<<"$reason" && fail "[H] the publish was refused: $reason"
pass "[H] the run got past the publish and stopped at the stand-in gateway"
wait_for 90 "the run pod to be collected" no_run_objects
wait_for 90 "the run pod's Secret to be collected with it" tls_secret_count 0
pass "[H] the Secret was collected with its pod"

log "[H] a rendered turn pod boots the gateway"
kubectl -n "$NS" run crucible-e2e-gitd --image "$LOOP_IMAGE" --port 9418 --restart Never --command -- sh -c \
    'mkdir -p /srv/code && cd /srv/code && git init -q && git -c user.name=e2e -c user.email=e2e@example.com commit -q --allow-empty -m init && exec git daemon --base-path=/srv --export-all --reuseaddr /srv' >/dev/null
kubectl -n "$NS" expose pod crucible-e2e-gitd --port 9418 >/dev/null
kubectl -n "$NS" wait --for=condition=Ready pod/crucible-e2e-gitd --timeout=120s >/dev/null
echo "Boot the gateway." >"$WORK/goal-H.txt"
H_TURN=crucible-turn-e2e-tls
"$BIN/crucible" deploy render-turn --profile "$WORK/profile.toml" --name "$H_TURN" --issue e2e/tls#1 \
    --turn-kind scope --goal-file "$WORK/goal-H.txt" --repo-url "git://crucible-e2e-gitd.$NS.svc:9418/code" \
    --sandbox-image ghcr.io/org/sandbox:latest --no-pin >"$WORK/turn-H.yaml"
watch_start H-turn
secrets_watch_start H-turn
kubectl apply -f "$WORK/turn-H.yaml" >/dev/null
wait_for 120 "the turn pod to finish" pods_in ".metadata.name == \"$H_TURN\" and (.status.phase == \"Succeeded\" or .status.phase == \"Failed\")" 1
watch_stop
expect_pod_secrets H-turn "$H_TURN" H-turn
kubectl -n "$NS" delete pod "$H_TURN" crucible-e2e-gitd --grace-period=1 --wait=true >/dev/null
kubectl -n "$NS" delete service crucible-e2e-gitd >/dev/null
wait_for 90 "the turn pod's Secret to be collected with it" tls_secret_count 0
pass "[H] the turn pod's Secret was collected with it"

# ---- scenario R ----------------------------------------------------------------------------
small_pack "$WORK/short" "sleep 10
$OK_JSON"
small_pack "$WORK/long" "sleep 45
$OK_JSON"
new_draft "$DRAFT-short" "$WORK/short"
new_draft "$DRAFT-long" "$WORK/long"
launch R-short draft-launch "$DRAFT-short"
SHORT="$KEY"
launch R-long draft-launch "$DRAFT-long"
LONG="$KEY"
wait_for 120 "both run pods to be running" pods_in '.status.containerStatuses[]?.state.running' 2
SHORT_RUN=$(crux playbook-run "$SHORT" | jq -r '.runs[0].run_id')
LONG_RUN=$(crux playbook-run "$LONG" | jq -r '.runs[0].run_id')
stop_controller
log "[R] controller down; waiting for the short run to finish"
wait_for 60 "the short run's pod to succeed" pods_in '.status.phase == "Succeeded"' 1
pods_in '.status.containerStatuses[]?.state.running' 1 || fail "[R] the long run finished before the controller came back"
start_controller
for run in "R-short $SHORT $SHORT_RUN" "R-long $LONG $LONG_RUN"; do
    read -r label key run_id <<<"$run"
    settle "$label" "$key"
    expect_finished "$label"
    [ "$(jq -r '[.runs[].run_id] | join(" ")' "$WORK/run-$label.json")" = "$run_id" ] ||
        fail "[$label] expected only run $run_id, got $(jq -c '[.runs[].run_id]' "$WORK/run-$label.json")"
done
pass "[R] both runs were collected, not re-dispatched"

# ---- scenario S ----------------------------------------------------------------------------
stop_controller
CONTROLLER_ENV=(CONTROLLER_MAX_CONCURRENT_PODS=1)
start_controller
log "[S] holding the only run slot while three launches wait for steering"
small_pack "$WORK/blocker" "sleep 30
$OK_JSON"
small_pack "$WORK/steer" "test \"\$(head -n 1 STEER.md)\" = frozen
grep -qx kind-steer-marker STEER.md
$OK_JSON" '["check.sh", "STEER.md"]'
echo frozen >"$WORK/steer/STEER.md"
small_pack "$WORK/heavysteer" "$OK_JSON"
for f in 1 2; do head -c 337500 /dev/urandom | base64 | tr -d '\n' >"$WORK/heavysteer/blob$f.txt"; done
small_pack "$WORK/bare" "grep -qx kind-steer-marker STEER.md
$OK_JSON" '["check.sh", "STEER.md"]'
for d in blocker steer heavysteer bare; do new_draft "$DRAFT-$d" "$WORK/$d"; done

watch_start S
launch S-block draft-launch "$DRAFT-blocker"
BLOCK="$KEY"
wait_for 120 "the blocker's pod to run" pods_in '.status.containerStatuses[]?.state.running' 1
launch S draft-launch "$DRAFT-steer"
STEERED="$KEY"
launch S-budget draft-launch "$DRAFT-heavysteer"
HEAVY_KEY="$KEY"
launch S-bare draft-launch "$DRAFT-bare"
BARE="$KEY"
for key in "$STEERED" "$HEAVY_KEY" "$BARE"; do
    [ "$(crux playbook-run "$key" | jq -r '.launch.status + " " + (.runs | length | tostring)')" = "new 0" ] ||
        fail "[S] $key dispatched while the blocker held the only slot"
done
insert_steering "$STEERED" 1 kind-steer-marker
insert_steering "$HEAVY_KEY" 1 "$(head -c 300000 /dev/urandom | base64 | tr -d '\n')"
insert_steering "$BARE" 1 kind-steer-marker
settle S-block "$BLOCK"
expect_finished S-block
settle S "$STEERED"
expect_finished S
settle S-budget "$HEAVY_KEY"
expect_parked S-budget "delivery budget"
settle S-bare "$BARE"
expect_finished S-bare
watch_stop
delivered S "$STEERED" inputs S
[ "$(tar -xzOf "$WORK/delivered-S.tar.gz" STEER.md)" = frozen ] || fail "[S] the pack key's STEER.md is not the frozen one"
[ "$(listing "$WORK/inputs-S.tar.gz")" = STEER.md ] || fail "[S] the inputs key lists $(listing "$WORK/inputs-S.tar.gz")"
[ "$(tar -xzOf "$WORK/inputs-S.tar.gz" STEER.md)" = "frozen
$STEER_STAMP
kind-steer-marker" ] || fail "[S] the inputs key's STEER.md is: $(tar -xzOf "$WORK/inputs-S.tar.gz" STEER.md)"
pass "[S] steering rode the inputs key over the frozen STEER.md, leaving the pack key the pack's own"
delivered S-bare "$BARE" inputs S
tree_paths "$LAUNCH_TREE" | grep -qx STEER.md && fail "[S] the bare pack ships a STEER.md"
pass "[S] an inject of STEER.md the pack does not ship is satisfied by the inputs key"
expect_no_pod S-budget "$HEAVY_KEY" S

# ---- scenario K ----------------------------------------------------------------------------
log "[K] firing schedules whose cursor files were advanced"
small_pack "$WORK/cursor" "test \"\$(cat cursor.json)\" = '{\"n\":7}'
$OK_JSON" '["check.sh", "cursor.json"]'
echo '{"n":0}' >"$WORK/cursor/cursor.json"
small_pack "$WORK/statecursor" "test \"\$(cat state/cursor.json)\" = '{\"n\":7}'
$OK_JSON" '["check.sh", "state/cursor.json"]'
small_pack "$WORK/runstate" "test \"\$(cat ../state/cursor.json)\" = '{\"n\":7}'
$OK_JSON"
for d in cursor statecursor runstate; do
    new_draft "$DRAFT-$d" "$WORK/$d"
    crux draft-publish "$DRAFT-$d" --playbook "k-$d" --json >"$WORK/publish-K-$d.json"
done

# schedule <playbook> <cursor path>: a yearly schedule with a run-file cursor; its id lands in
# SCHEDULE.
schedule() {
    api POST /api/schedules "{\"playbook\":\"$1\",\"cron_expr\":\"0 0 1 1 *\",\"max_cost\":1,\"max_time\":\"5m\",\"cursor\":{\"from\":\"check/out.txt\",\"path\":\"$2\"}}"
    expect_http K 201 "\"playbook\":\"$1\""
    SCHEDULE=$(jq -r .id "$WORK/api.json")
}

# fired <label> <playbook> <schedule> <mode>: leave the schedule's cursor where a finished firing
# would, make it due, and expect the launch it fires to succeed with that delivery.
fired() {
    local prev
    prev=$(newest_launch "$2" schedule)
    watch_start "$1"
    sql -c "UPDATE playbook_schedules SET cursor_value = '{\"n\":7}', next_due_at = '2000-01-01T00:00:00Z' WHERE id = '$3'"
    wait_for 60 "$2's scheduled launch" new_launch "$2" schedule "$prev"
    KEY=$(newest_launch "$2" schedule)
    settle "$1" "$KEY"
    expect_finished "$1"
    delivered "$1" "$KEY" "$4"
    watch_stop
}

api POST /api/schedules '{"playbook":"k-cursor","cron_expr":"0 0 1 1 *","max_cost":1,"max_time":"5m","cursor":{"from":"check/out.txt","path":"a\nb.json"}}'
expect_http K 422 '"field":"cursor.path"'
pass "[K] a cursor path that is not a pack path is refused at save"

schedule k-cursor cursor.json
[ "$(jq -r .adopted_tree_digest "$WORK/api.json")" = "$(playbook_tree k-cursor)" ] ||
    fail "[K] the schedule names adopted tree $(jq -r .adopted_tree_digest "$WORK/api.json")"
pass "[K] the schedule names the adopted tree_digest"
fired K k-cursor "$SCHEDULE" inputs
[ "$LAUNCH_TREE" = "$(playbook_tree k-cursor)" ] || fail "[K] the firing ran $LAUNCH_TREE, not the registered tree"
[ "$(listing "$WORK/inputs-K.tar.gz")" = cursor.json ] && [ "$(tar -xzOf "$WORK/inputs-K.tar.gz" cursor.json)" = '{"n":7}' ] ||
    fail "[K] the inputs key holds $(listing "$WORK/inputs-K.tar.gz")"
[ "$(tar -xzOf "$WORK/delivered-K.tar.gz" cursor.json)" = '{"n":0}' ] || fail "[K] the pack key's cursor.json is not the pack's default"
pass "[K] the cursor file rode the inputs key over the pack's default"

schedule k-statecursor state/cursor.json
fired K-state k-statecursor "$SCHEDULE" inputs
[ "$(listing "$WORK/inputs-K-state.tar.gz")" = state/cursor.json ] || fail "[K] the inputs key holds $(listing "$WORK/inputs-K-state.tar.gz")"
pass "[K] an injected cursor under state/ reached the run"

schedule k-runstate state/cursor.json
RUNSTATE_SCHEDULE="$SCHEDULE"
fired K-runstate k-runstate "$SCHEDULE" inputs
pass "[K] a cursor under state/ reached the run's state dir"

log "[K] firing a draft-head schedule"
api PUT /api/config/overrides '{"allow_draft_head_schedules":true,"justification":"kind e2e"}'
[ "$HTTP" = 200 ] || fail "[K] enabling draft-head schedules: $HTTP $(cat "$WORK/api.json")"
echo '# version 3' >>"$WORK/cursor/check.sh"
crux draft-push "$DRAFT-cursor" "$WORK/cursor" --base-version 2 --json >"$WORK/push-K-dh.json"
[ "$(jq -r .version "$WORK/push-K-dh.json")" = 3 ] || fail "[K] draft-push: $(cat "$WORK/push-K-dh.json")"
api POST /api/schedules "{\"playbook\":\"$DRAFT-cursor\",\"target_kind\":\"draft_head\",\"cron_expr\":\"0 0 1 1 *\",\"max_cost\":1,\"max_time\":\"5m\",\"cursor\":{\"from\":\"check/out.txt\",\"path\":\"cursor.json\"}}"
expect_http K 201 '"target_kind":"draft_head"'
HEAD_SCHEDULE=$(jq -r .id "$WORK/api.json")
[ "$(jq -r .adopted_tree_digest "$WORK/api.json")" = null ] || fail "[K] the draft-head schedule adopted a tree"
mkdir -p "$WORK/broken"
cp -R "$WORK/cursor/." "$WORK/broken/"
echo 'this is not starlark (' >"$WORK/broken/workflow.star"
crux draft-push "$DRAFT-cursor" "$WORK/broken" --base-version 3 --json >"$WORK/push-K-broken.json"
[ "$(jq -r .version "$WORK/push-K-broken.json")" = 4 ] || fail "[K] draft-push: $(cat "$WORK/push-K-broken.json")"
[ "$(sql -c "SELECT schema_digest IS NULL FROM playbook_draft_versions WHERE draft_id = '$DRAFT-cursor' AND version = 4")" = t ] ||
    fail "[K] version 4 compiled"
fired K-head "$DRAFT-cursor" "$HEAD_SCHEDULE" inputs
[ "$LAUNCH_TREE" = "$(draft_tree "$DRAFT-cursor" 3)" ] || fail "[K] the draft-head firing ran $LAUNCH_TREE, not version 3's tree"
pass "[K] the draft-head firing ran the newest compiled tree, not the newer save that does not compile"

stop_controller
log "[K] firing the state/ cursor schedule over a run-state claim"
kubectl -n "$NS" apply -f - >/dev/null <<'YAML'
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: crucible-e2e-state
spec:
  accessModes: [ReadWriteOnce]
  storageClassName: standard
  resources:
    requests:
      storage: 64Mi
YAML
CONTROLLER_ENV=()
start_controller "$WORK/profile-state.toml"
fired K-pvc k-runstate "$RUNSTATE_SCHEDULE" inputs
pass "[K] a cursor under state/ reached the run over a run-state claim"

# ---- scenario Y ----------------------------------------------------------------------------
stop_controller
CONTROLLER_ENV=()
start_controller
log "[Y] saving examples/schema-repair and reading its compiled graph"
new_draft schema-repair "$ROOT/examples/schema-repair"
api GET /api/playbook-drafts/schema-repair/preview
[ "$HTTP" = 200 ] || fail "[Y] the draft preview answered $HTTP: $(cat "$WORK/api.json")"
[ "$(jq -c '[.graph.nodes[] | {(.name): .emits_files}] | add' "$WORK/api.json")" = \
    '{"probe":["PROBE.json"],"plan":["PLAN.json"],"audit":["AUDIT.json"]}' ] ||
    fail "[Y] the compiled graph's declared files: $(jq -c '{graph, diagnostics}' "$WORK/api.json")"
pass "[Y] the controller reduced a plan with schema-typed fields and files to its graph"

launch Y draft-launch schema-repair
settle Y "$KEY"
expect_finished Y
RUN=$(jq -r '.runs[0].run_id' "$WORK/run-Y.json")
sql -c "SELECT graph_json FROM run_plans WHERE run_id = '$RUN'" >"$WORK/plan-Y.json"
jq -e '.[] | select(.name == "plan")
    | (.emits[0].type.schema | contains("\"items\""))
      and (.emits_files[] | select(.path == "PLAN.json") | .schema | contains("\"required\""))' \
    "$WORK/plan-Y.json" >/dev/null || fail "[Y] the admitted plan does not carry the schema text: $(cat "$WORK/plan-Y.json")"
api GET "/api/runs/$RUN/graph"
[ "$HTTP" = 200 ] || fail "[Y] the run graph answered $HTTP: $(cat "$WORK/api.json")"
pass "[Y] the admitted plan carries each schema's text and the run graph decodes it"

y_result() { jq -c --arg t "$1" '.results[] | select(.task == $t) | '"$2" "$WORK/api.json"; }
[ "$(y_result plan '[.status, .cost_usd, (.repairs | map([.label, .round, .of, .cost_usd]))]')" = \
    '["pass",0.375,[["plan repair 1/1",1,1,0.125]]]' ] ||
    fail "[Y] plan: $(y_result plan .)"
[ "$(y_result plan '.repairs[0].notes')" = \
    '["declared file \"PLAN.json\" does not match its schema: /ok: value is not of type \"boolean\""]' ] ||
    fail "[Y] plan's repair notes: $(y_result plan .repairs)"
pass "[Y] the agent passed after one repair turn, recorded with its cost and notes"
[ "$(y_result 'audit[a]' '[.status, .repairs]')" = '["pass",[]]' ] || fail "[Y] audit[a]: $(y_result 'audit[a]' .)"
[ "$(y_result 'audit[b]' '[.status, .note, (.repairs | length)]')" = \
    '["fail","declared file \"AUDIT.json\" does not match its schema: /ok: value is not of type \"boolean\"",1]' ] ||
    fail "[Y] audit[b]: $(y_result 'audit[b]' .)"
grep -q hunter2 "$WORK/api.json" && fail "[Y] a task result repeats the refused value"
pass "[Y] the instance that never fixed its file failed after its repair, with a masked note"
api GET "/api/runs/$RUN"
[ "$(jq -r '.run.cost_usd' "$WORK/api.json")" = 0.5625 ] ||
    fail "[Y] the run spent $(jq -r '.run.cost_usd' "$WORK/api.json"), not 0.5625 with its repairs"
pass "[Y] the run's spend includes the repair turns"
stop_controller


# ---- scenario JP ----------------------------------------------------------------------------
# The run pod reaches the controller on the host, by the address the kind node resolves the
# host alias to, else the kind network's gateway. A controller bound past loopback needs its
# bearer guard on.
J_HOST=$(docker exec "$CLUSTER-control-plane" getent ahostsv4 host.docker.internal host.containers.internal 2>/dev/null | awk 'NR == 1 { print $1 }')
[ -n "$J_HOST" ] || J_HOST=$(docker network inspect kind --format '{{range .IPAM.Config}}{{.Gateway}} {{end}}' | tr ' ' '\n' | grep -m1 '\.')
[ -n "$J_HOST" ] || fail "[JP] no address the kind network reaches the host on"
awk -v url="http://$J_HOST:$PORT" '{ print } /^\[cluster\]$/ { print "ingest_url = \"" url "\"" }' \
    "$WORK/profile.toml" >"$WORK/profile-evidence.toml"
export CONTROLLER_API_TOKEN="e2e-$(openssl rand -hex 16)"
CONTROLLER_ENV=(CONTROLLER_API_ADDR="0.0.0.0:$PORT" CONTROLLER_DEV_IDENTITY= CONTROLLER_API_TOKEN_IDENTITY=e2e
    CONTROLLER_PROXY_TOKEN="proxy-$(openssl rand -hex 16)")
start_controller "$WORK/profile-evidence.toml"

# run_artifacts <run>: the kinds of artifact the run owns, sorted.
run_artifacts() { sql -c "SELECT string_agg(kind, ' ' ORDER BY kind) FROM artifacts WHERE owner_kind = 'run-session' AND owner_id = '$1'"; }

# token_state <run>: whether the run's one token is revoked, and the run it reads.
token_state() { sql -c "SELECT count(*) || ' ' || bool_and(revoked_at IS NOT NULL) || ' ' || coalesce(max(reads_run), '-') FROM pod_tokens WHERE run_id = '$1'"; }

# evidence <run> <kind> <out>: GET the run's evidence as a person, printing the status.
evidence() {
    curl -s -o "$3" -w '%{http_code}' -H "authorization: Bearer $CONTROLLER_API_TOKEN" \
        "$CONTROLLER_URL/api/runs/$1/evidence/$2"
}

log "[JP] a run pod uploads its evidence to its run under the run token"
cp -R "$FIX/packs/rerun" "$WORK/rerun"
new_draft "$DRAFT-rerun" "$WORK/rerun"
watch_start JP
launch JP draft-launch "$DRAFT-rerun" --params '{"mode":"broken"}'
PARENT_KEY="$KEY"
settle JP "$PARENT_KEY"
PARENT=$(jq -r '.runs[0].run_id' "$WORK/run-JP.json")
[ -n "$PARENT" ] && [ "$PARENT" != null ] || fail "[JP] the parent launch recorded no run: $(cat "$WORK/run-JP.json")"
# engine_lines <run>: the engine's own lines from the run's kept log.
engine_lines() {
    api GET "/api/runs/$1/log"
    jq -r '.text // empty' "$WORK/api.json" | grep '^\[crucible\]' || true
}
[ "$(run_artifacts "$PARENT")" = "run-commits run-engine-log run-files run-session" ] ||
    fail "[JP] the parent run owns: $(run_artifacts "$PARENT"); its engine said: $(engine_lines "$PARENT")"
[ "$(sql -c "SELECT count(*) FROM artifacts WHERE owner_kind = 'pod-evidence'")" = 0 ] ||
    fail "[JP] a pod-keyed artifact was stored"
pass "[JP] the pod's session, files and git memory landed on its run, with nothing keyed by the pod"
PARENT_POD=$(sql -c "SELECT pod FROM runs WHERE run_id = '$PARENT'")
jq -se --arg n "$PARENT_POD" '[.[] | .object | select(.metadata.name == $n) | .spec.containers[0].env[]
    | select(.name == "CRUCIBLE_POD_TOKEN")] | length > 0 and all(.[]; .value == null
    and .valueFrom.secretKeyRef.name == ($n + "-secrets"))' "$WORK/pods-JP.json" >/dev/null ||
    fail "[JP] the run token does not ride the run's Secret: $(jq -sc --arg n "$PARENT_POD" '[.[] | .object | select(.metadata.name == $n) | .spec.containers[0].env[] | select(.name | startswith("CRUCIBLE_"))] | unique' "$WORK/pods-JP.json")"
jq -se '[.[] | .object.spec.volumes[]?.projected.sources[]?.serviceAccountToken | select(.audience == "crucible-ingest")] | length == 0' "$WORK/pods-JP.json" >/dev/null ||
    fail "[JP] a run pod still projects the drop-box's ServiceAccount token"
[ "$(token_state "$PARENT")" = "1 true -" ] || fail "[JP] the parent's token: $(token_state "$PARENT")"
pass "[JP] the token rode the run's Secret, no drop-box token was projected, and completion revoked it"

[ "$(evidence "$PARENT" run-session "$WORK/JP-session.gz")" = 200 ] || fail "[JP] a person cannot read the parent's session"
gunzip -c "$WORK/JP-session.gz" | grep -q '"kind":"shutdown"' || fail "[JP] the fetched session is not the parent's"
[ "$(evidence "$PARENT" scope-pack "$WORK/JP-nope")" = 404 ] || fail "[JP] a scope kind was served as run evidence"
pass "[JP] a person reads the parent's evidence over the API"

log "[JP] a rerun pod fetches its parent's evidence over its run token"
api POST "/api/playbook-drafts/$DRAFT-rerun/launch" \
    "{\"params\":{\"mode\":\"fixed\"},\"max_cost\":1,\"max_time\":\"5m\",\"rerun\":{\"of\":\"$PARENT\",\"from\":[\"second\"]}}"
[ "$HTTP" = 201 ] || [ "$HTTP" = 200 ] || fail "[JP] the rerun launch answered $HTTP: $(cat "$WORK/api.json")"
KEY=$(jq -r .key "$WORK/api.json")
settle JP-rerun "$KEY"
watch_stop
expect_finished JP-rerun
RERUN=$(jq -r '.runs[0].run_id' "$WORK/run-JP-rerun.json")
RERUN_POD=$(sql -c "SELECT pod FROM runs WHERE run_id = '$RERUN'")
jq -rs --arg n "$RERUN_POD" '[.[] | .object | select(.metadata.name == $n) | .spec.containers[0].args[]?] | unique | join("\n")' \
    "$WORK/pods-JP.json" | grep -qF -- "--rerun-of \"\${CRUCIBLE_CONTROLLER_URL}/api/runs/$PARENT\" --from 'second'" ||
    fail "[JP] the rerun pod does not name its parent on the controller"
[ "$(token_state "$RERUN")" = "1 true $PARENT" ] || fail "[JP] the rerun's token: $(token_state "$RERUN")"
[ "$(evidence "$RERUN" run-session "$WORK/JP-rerun.gz")" = 200 ] || fail "[JP] the rerun uploaded no session"
gunzip -c "$WORK/JP-rerun.gz" | jq -se --arg p "$PARENT" \
    '[.[] | select(.kind == "task_result" and .task == "first")] | length == 1 and .[0].reused_from == $p' >/dev/null ||
    fail "[JP] the rerun did not reuse first from $PARENT: $(gunzip -c "$WORK/JP-rerun.gz" | grep '"task_result"')"
pass "[JP] the rerun pod read its parent's session, files and commits, reused first, and passed second"
[ "$(run_artifacts "$RERUN")" = "run-commits run-engine-log run-files run-session" ] ||
    fail "[JP] the rerun owns: $(run_artifacts "$RERUN")"

# ---- scenario JT ----------------------------------------------------------------------------
log "[JT] a scope pod uploads its pack and transcript on its turn's pod token"
JT_POD=crucible-turn-e2e-jt
JT_ID=$(openssl rand -hex 16)
JT_SECRET=$(openssl rand -hex 32)
JT_HASH=$(printf %s "$JT_SECRET" | openssl dgst -sha256 -binary | openssl base64 -A | tr '+/' '-_' | tr -d '=')
sql -c "INSERT INTO pod_tokens (id, turn_pod, secret_hash, created_at, expires_at)
        VALUES ('$JT_ID', '$JT_POD', '$JT_HASH', '2026-01-01T00:00:00Z', '2099-01-01T00:00:00Z')" >/dev/null
JT_TOKEN="crr_${JT_ID}_${JT_SECRET}"
kubectl -n "$NS" create configmap "$JT_POD-pack" --from-file="$FIX/packs/scope/workflow.star" \
    --from-file="$FIX/packs/scope/measure.sh" --from-file="$FIX/packs/scope/SCOPE.md" >/dev/null
kubectl -n "$NS" apply -f - >/dev/null <<YAML
apiVersion: v1
kind: Pod
metadata:
  name: $JT_POD
spec:
  restartPolicy: Never
  containers:
  - name: scope
    image: $LOOP_IMAGE
    env:
    - { name: CRUCIBLE_CONTROLLER_URL, value: "http://$J_HOST:$PORT" }
    - { name: CRUCIBLE_POD_NAME, valueFrom: { fieldRef: { fieldPath: metadata.name } } }
    - { name: CRUCIBLE_POD_TOKEN, value: "$JT_TOKEN" }
    command: [sh, -c]
    args:
    - |
      set -e
      mkdir -p /tmp/code /tmp/pack && cd /tmp/code && git init -q && echo code >README
      git -c user.name=e2e -c user.email=e2e@example.com add -A
      git -c user.name=e2e -c user.email=e2e@example.com commit -qm code
      cp /fixture/* /tmp/pack/ && chmod +x /tmp/pack/measure.sh
      printf '[repo]\npath = "/tmp/code"\n\n[workspace]\ninject = ["measure.sh"]\n\n[agent]\nbackend = "openshell"\ngoal = "Freeze the pack."\nsandbox_image = "ghcr.io/org/sandbox:latest"\n\n[judge]\nmeasure_cmd = "./measure.sh"\ndirection = "higher"\nobjective = "score"\n\n[workflow]\ntype = "autoresearch"\nfile = "workflow.star"\n' >/tmp/pack/crucible.toml
      crucible scope --pack /tmp/pack --json --marker >/dev/null
    volumeMounts: [{ name: fixture, mountPath: /fixture }]
  volumes: [{ name: fixture, configMap: { name: $JT_POD-pack } }]
YAML
wait_for 120 "the scope pod to finish" pods_in ".metadata.name == \"$JT_POD\" and (.status.phase == \"Succeeded\" or .status.phase == \"Failed\")" 1
kubectl -n "$NS" get pod "$JT_POD" -o jsonpath='{.status.containerStatuses[0].state.terminated.message}' >"$WORK/JT-message.json"
jt_manifest() { jq -r --arg k "$1" '.artifacts[] | select(.kind == $k) | .digest + " " + (.delivered | tostring)' "$WORK/JT-message.json"; }
for kind in scope-pack scope-transcript; do
    [ "$(jt_manifest "$kind")" = "$(sql -c "SELECT digest FROM pod_artifacts WHERE pod = '$JT_POD' AND kind = '$kind'") true" ] ||
        fail "[JT] $kind: the manifest says '$(jt_manifest "$kind")'; the controller holds '$(sql -c "SELECT digest FROM pod_artifacts WHERE pod = '$JT_POD' AND kind = '$kind'")'; the pod said: $(kubectl -n "$NS" logs "$JT_POD" 2>&1 | tail -5)"
done
[ "$(sql -c "SELECT count(*) FROM artifacts WHERE owner_kind = 'pod-evidence' AND owner_id = '$JT_POD'")" = 2 ] ||
    fail "[JT] the pack and transcript were not stored under the pod"
pass "[JT] the pod's pack and transcript landed under its name, matching the digests its manifest reports"

# jt_put <path> <bearer>: PUT a body to an evidence route, printing the status.
jt_put() {
    curl -s -o /dev/null -w '%{http_code}' -X PUT -H "authorization: Bearer $2" \
        -H 'content-type: application/gzip' --data-binary @"$WORK/JT-message.json" "$CONTROLLER_URL$1"
}
[ "$(jt_put "/api/turns/crucible-turn-other/evidence/scope-pack" "$JT_TOKEN")" = 403 ] ||
    fail "[JT] a turn's token wrote another pod's pack"
[ "$(jt_put "/api/runs/$PARENT/evidence/run-session" "$JT_TOKEN")" = 403 ] ||
    fail "[JT] a turn's token wrote a run's evidence"
[ "$(jt_put "/api/turns/$JT_POD/evidence/scope-pack" "$CONTROLLER_API_TOKEN")" = 403 ] ||
    fail "[JT] a person wrote a turn's pack"
sql -c "UPDATE pod_tokens SET revoked_at = '2026-01-01T00:00:00Z' WHERE id = '$JT_ID'" >/dev/null
[ "$(jt_put "/api/turns/$JT_POD/evidence/scope-pack" "$JT_TOKEN")" = 401 ] ||
    fail "[JT] a revoked turn token still uploaded"
kubectl -n "$NS" delete pod "$JT_POD" --grace-period=1 --wait=true >/dev/null
kubectl -n "$NS" delete configmap "$JT_POD-pack" >/dev/null
pass "[JT] the turn's token writes only its own pod's pack and transcript, and not once revoked"
stop_controller
unset CONTROLLER_API_TOKEN
CONTROLLER_ENV=()
