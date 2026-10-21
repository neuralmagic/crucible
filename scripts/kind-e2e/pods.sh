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

