# shellcheck shell=bash
# Sourced by scripts/kind-e2e.sh as the lease shard.
# ---- scenario O ----------------------------------------------------------------------------
CONTROLLER_ENV=(CONTROLLER_PLAYBOOK_EXECUTOR=local CONTROLLER_MAX_CONCURRENT_PODS=1)
start_controller
log "[O] running stored trees with the local executor"
small_pack "$WORK/localblock" "sleep 20
$OK_JSON"
small_pack "$WORK/localsteer" "echo local-$ID
test \"\$(head -n 1 STEER.md)\" = frozen
grep -qx kind-steer-marker STEER.md
$OK_JSON" '["check.sh", "STEER.md"]'
echo frozen >"$WORK/localsteer/STEER.md"
new_draft "$DRAFT-lblock" "$WORK/localblock"
new_draft "$DRAFT-lsteer" "$WORK/localsteer"
watch_start O
launch O-block draft-launch "$DRAFT-lblock"
BLOCK="$KEY"
wait_for 60 "the local blocker to run" is_running "$BLOCK"
launch O draft-launch "$DRAFT-lsteer"
LOCAL_KEY="$KEY"
[ "$(crux playbook-run "$LOCAL_KEY" | jq -r '.launch.status + " " + (.runs | length | tostring)')" = "new 0" ] ||
    fail "[O] $LOCAL_KEY dispatched while the blocker held the only slot"
insert_steering "$LOCAL_KEY" 1 kind-steer-marker
settle O-block "$BLOCK"
expect_finished O-block
settle O "$LOCAL_KEY"
expect_finished O
small_pack "$WORK/localtamper" "echo local-tamper-$ID
$OK_JSON"
new_draft "$DRAFT-ltamper" "$WORK/localtamper"
crux draft-publish "$DRAFT-ltamper" --playbook "$DRAFT-ltpub" --json >"$WORK/publish-O.json"
LOCAL_TAMPER=$(playbook_tree "$DRAFT-ltpub")
tamper_file "$LOCAL_TAMPER" check.sh
launch O-tamper launch "$DRAFT-ltpub"
settle O-tamper "$KEY"
watch_stop
expect_parked O-tamper "$LOCAL_TAMPER no longer matches its stored files"
for key in "$BLOCK" "$LOCAL_KEY" "$KEY"; do
    [ "$(pods_seen O "$key")" = 0 ] || fail "[O] the local executor created a pod for $key"
done
LOCAL_PACK=$(ls -d "$WORK/scratch/local-runs/$(slug "$LOCAL_KEY")"-*/pack)
[ "$(cat "$LOCAL_PACK/STEER.md")" = "frozen
$STEER_STAMP
kind-steer-marker" ] || fail "[O] the local run's STEER.md is: $(cat "$LOCAL_PACK/STEER.md")"
pass "[O] the local executor ran the stored tree with its steering and parked the tampered one"

# ---- scenario J ----------------------------------------------------------------------------
# rerun_pack <dir> <check command>: produce -> edit -> check -> consume, each appending its name to
# a ledger outside the run so a task dispatched twice shows up twice.
J_LEDGER="$WORK/rerun-ran.log"
rerun_pack() {
    mkdir -p "$1"
    sed 's|^inject = .*|inject = []|' "$FIX/packs/deliver/crucible.toml" >"$1/crucible.toml"
    cat >"$1/workflow.star" <<STAR
produce = command(name = "produce", run = "echo produce >> $J_LEDGER && echo carried > out.txt && echo '{}'", emits_files = ["out.txt"])
edit = command(name = "edit", run = "echo edit >> $J_LEDGER && echo kept > edited.txt && echo '{}'", depends_on = [produce])
check = command(name = "check", run = "echo check >> $J_LEDGER && $2", depends_on = [edit])
consume = command(name = "consume", run = "echo consume >> $J_LEDGER && cp inputs/produce/out.txt $WORK/rerun-consumed && echo '{}'", depends_on = [check])
workflow(type = "playbook", tasks = [produce, edit, check, consume])
STAR
}
ledger() { paste -sd' ' "$J_LEDGER"; }

log "[J] a draft run that fails at its last edited task"
rerun_pack "$WORK/rerun" "exit 1"
new_draft "$DRAFT-rerun" "$WORK/rerun"
launch J-parent draft-launch "$DRAFT-rerun"
settle J-parent "$KEY"
PARENT_RUN=$(jq -r '.runs[0].run_id' "$WORK/run-J-parent.json")
[ "$(ledger)" = "produce edit check" ] || fail "[J] the parent ran: $(ledger)"

log "[J] fixing check and rerunning from it"
rerun_pack "$WORK/rerun" "cat edited.txt > $WORK/rerun-seen && echo '{}'"
crux draft-push "$DRAFT-rerun" "$WORK/rerun" --base-version 2 --json >"$WORK/push-J.json"
api GET "/api/runs/$PARENT_RUN/rerun?from=check"
[ "$HTTP" = 200 ] || fail "[J] preview answered $HTTP: $(cat "$WORK/api.json")"
[ "$(jq -c '[[.reused[].task], .rerun, .changed]' "$WORK/api.json")" = '[["produce","edit"],["check","consume"],[]]' ] ||
    fail "[J] preview: $(cat "$WORK/api.json")"
launch J draft-launch "$DRAFT-rerun" --rerun-of "$PARENT_RUN" --from check
RERUN_KEY="$KEY"
settle J "$RERUN_KEY"
expect_finished J
[ "$(ledger)" = "produce edit check check consume" ] || fail "[J] the rerun dispatched: $(ledger)"
[ "$(cat "$WORK/rerun-seen")" = kept ] || fail "[J] the reused edit's commit is not in the rerun's workspace"
[ "$(cat "$WORK/rerun-consumed")" = carried ] || fail "[J] the reused producer's file did not reach consume"
J_SESSION=$(ls "$WORK/scratch/local-runs/$(slug "$RERUN_KEY")"-*/pack/state/session.jsonl)
[ "$(jq -sc --arg p "$PARENT_RUN" '[.[] | select(.kind == "task_result" and .reused_from == $p) | .task]' "$J_SESSION")" = '["produce","edit"]' ] ||
    fail "[J] reused rows: $(jq -c 'select(.kind == "task_result")' "$J_SESSION")"
[ "$(crux playbook-run "$RERUN_KEY" | jq -r '.rerun.run_id')" = "$PARENT_RUN" ] ||
    fail "[J] the rerun launch does not name its parent"

log "[J] an edit upstream of the selection"
sed -i.bak 's/echo carried > out.txt/echo changed > out.txt/' "$WORK/rerun/workflow.star"
rm "$WORK/rerun/workflow.star.bak"
crux draft-push "$DRAFT-rerun" "$WORK/rerun" --base-version 3 --json >"$WORK/push-J2.json"
if crux draft-launch "$DRAFT-rerun" --rerun-of "$PARENT_RUN" --from check --max-cost 1 --max-time 5m \
    --json >"$WORK/launch-J-refused.json" 2>&1; then
    fail "[J] a rerun reusing an edited task launched: $(cat "$WORK/launch-J-refused.json")"
fi
grep -qF "produce changed since the parent run" "$WORK/launch-J-refused.json" ||
    fail "[J] the refusal does not name produce: $(cat "$WORK/launch-J-refused.json")"
[ "$(ledger)" = "produce edit check check consume" ] || fail "[J] the refused rerun dispatched: $(ledger)"
pass "[J] a rerun reused upstream work, its files and commits, and refused an edit it would have reused"

# ---- scenario N ----------------------------------------------------------------------------
N_BOUND=10
LEASE=crucible-controller-leader
lease_is() { [ "$(kubectl -n "$NS" get lease "$LEASE" -o jsonpath='{.spec.holderIdentity}' 2>/dev/null)" = "$1" ]; }
iso() { jq -nr --argjson t "$1" '$t | todateiso8601'; }
discovery_secs() { curl -sf "$1/api/config" | jq -r '.knobs[] | select(.name == "discovery_secs") | .value'; }

# pod_created <watch label> <key>: the epoch second of the launch's first pod, empty before one.
pod_created() {
    jq -rs --arg key "$(label "$2")" '[.[] | .object | select(.metadata.labels["crucible.dev/issue-key"] == $key) | .metadata.creationTimestamp | fromdateiso8601] | min // empty' "$WORK/pods-$1.json"
}
has_pod() { [ -n "$(pod_created "$1" "$2")" ]; }

# pod_count <watch label> <key>: how many distinct pods the watch saw for the launch.
pod_count() {
    jq -s --arg key "$(label "$2")" '[.[] | .object | select(.metadata.labels["crucible.dev/issue-key"] == $key) | .metadata.uid] | unique | length' "$WORK/pods-$1.json"
}

# expect_prompt <label> <key> <epoch> <what>: the launch's pod was created no earlier than the
# epoch and at most N_BOUND seconds after it. The latency lands in LATENCY.
expect_prompt() {
    wait_for 90 "$2's pod" has_pod N "$2"
    LATENCY=$(($(pod_created N "$2") - $3))
    [ "$LATENCY" -ge 0 ] || fail "[$1] $2's pod was created ${LATENCY#-}s before $4"
    [ "$LATENCY" -le "$N_BOUND" ] || fail "[$1] $2's pod was created ${LATENCY}s after $4, over ${N_BOUND}s"
    pass "[$1] $2's pod was created ${LATENCY}s after $4"
}

# sign <secret> <body>: the x-hub-signature-256 value for the body.
sign() { printf 'sha256=%s' "$(printf '%s' "$2" | openssl dgst -sha256 -hmac "$1" | awk '{ print $NF }')"; }

stop_controller
log "[N] electing the controller and a peer through the Lease, with the discovery tick at its default"
DISCOVERY_SECS=""
N_WEBHOOK_KEY=$(openssl rand -base64 32)
CONTROLLER_ENV=(POD_NAME=kind-a POD_NAMESPACE="$NS" CONTROLLER_WEBHOOK_KEY="$N_WEBHOOK_KEY")
start_controller
wait_for 30 "kind-a to hold the lease" lease_is kind-a
CONTROLLER_ENV=(POD_NAME=kind-b POD_NAMESPACE="$NS" CONTROLLER_WEBHOOK_KEY="$N_WEBHOOK_KEY")
start_peer
sleep 4
grep -q 'leading' "$WORK/controller-peer.log" && fail "[N] the peer leads beside kind-a"
lease_is kind-a || fail "[N] the lease moved off kind-a when the peer started"
[ "$(discovery_secs "$CONTROLLER_URL") $(discovery_secs "$PEER_URL")" = "300 300" ] ||
    fail "[N] the replicas run discovery every $(discovery_secs "$CONTROLLER_URL")s and $(discovery_secs "$PEER_URL")s, not the 300s default"
pass "[N] kind-a leads, the peer stands by, both on the 300s discovery tick"

t0=$(date +%s)
kubectl -n "$NS" create configmap kind-e2e-clock >/dev/null
t1=$(date +%s)
created=$(kubectl -n "$NS" get configmap kind-e2e-clock -o json | jq '.metadata.creationTimestamp | fromdateiso8601')
kubectl -n "$NS" delete configmap kind-e2e-clock >/dev/null
[ "$created" -ge $((t0 - 1)) ] && [ "$created" -le $((t1 + 1)) ] ||
    fail "[N] the cluster clock read $created between host seconds $t0 and $t1"
pass "[N] the cluster clock agrees with the host's to the second"

small_pack "$WORK/loop" "$OK_JSON"
new_draft "$DRAFT-loop" "$WORK/loop"
crux draft-publish "$DRAFT-loop" --playbook e2e-lpub --json >"$WORK/publish-N.json"
watch_start N

log "[N] an every-minute schedule, due while the standby launch and a one-shot run"
api POST /api/schedules '{"playbook":"e2e-lpub","cron_expr":"* * * * *","tz":"UTC","max_cost":1,"max_time":"5m"}'
expect_http N-cron 201 '"playbook":"e2e-lpub"'
N_SCHEDULE=$(jq -r .id "$WORK/api.json")
N_CRON_DUE=$(sql -c "SELECT extract(epoch FROM next_due_at::timestamptz)::bigint FROM playbook_schedules WHERE id = '$N_SCHEDULE'")
[ $((N_CRON_DUE % 60)) = 0 ] || fail "[N] the schedule is first due at $(iso "$N_CRON_DUE"), not on a minute boundary"

log "[N] launching through the peer's API"
N_START=$(date +%s)
CONTROLLER_URL="$PEER_URL" launch N-standby launch e2e-lpub
expect_prompt N-standby "$KEY" "$N_START" "the peer accepted it"
N_LAT_STANDBY=$LATENCY
settle N-standby "$KEY"
expect_finished N-standby

log "[N] a one-shot due in 8s, created on the peer"
N_DUE=$(($(date +%s) + 8))
CONTROLLER_URL="$PEER_URL" api POST /api/one-shots "{\"playbook\":\"e2e-lpub\",\"max_cost\":1,\"max_time\":\"5m\",\"fire_at\":\"$(iso "$N_DUE")\"}"
expect_http N-once 201 '"playbook":"e2e-lpub"'
wait_for 60 "the one-shot's launch" new_launch e2e-lpub deferred ""
KEY=$(newest_launch e2e-lpub deferred)
expect_prompt N-once "$KEY" "$N_DUE" "its fire_at"
N_LAT_ONCE=$LATENCY
N_ONCE_KEY=$KEY
settle N-once "$KEY"
expect_finished N-once

wait_for 90 "the schedule's launch" new_launch e2e-lpub schedule ""
api DELETE "/api/schedules/$N_SCHEDULE"
[ "$HTTP" = 204 ] || fail "[N] deleting the schedule: $HTTP $(cat "$WORK/api.json")"
KEY=$(newest_launch e2e-lpub schedule)
expect_prompt N-cron "$KEY" "$N_CRON_DUE" "the minute boundary $(iso "$N_CRON_DUE")"
N_LAT_CRON=$LATENCY
settle N-cron "$KEY"
expect_finished N-cron
[ "$(sql -c "SELECT count(*) FROM playbook_launches WHERE playbook = 'e2e-lpub' AND origin = 'schedule'")" = 1 ] ||
    fail "[N] the schedule fired $(sql -c "SELECT count(*) FROM playbook_launches WHERE playbook = 'e2e-lpub' AND origin = 'schedule'") times"

log "[N] signed webhook deliveries to each replica"
jq -n '{playbook: "e2e-lpub", verifier: "hmac_sha256", header: "x-hub-signature-256", dedupe: "string(body.n)", max_launches_per_hour: 10, max_cost: 1, max_time: "5m"}' >"$WORK/webhook-N.json"
crux webhook-create --file "$WORK/webhook-N.json" >"$WORK/webhook-created-N.json"
N_HOOK=$(jq -r .webhook.id "$WORK/webhook-created-N.json")
N_SECRET=$(jq -r .secret "$WORK/webhook-created-N.json")
[ -n "$N_HOOK" ] && [ "$N_HOOK" != null ] || fail "[N] webhook-create answered $(cat "$WORK/webhook-created-N.json")"
N_LAT_HOOK=""
n=0
for target in "peer $PEER_HOOKS_URL" "kind-a $HOOKS_URL"; do
    read -r who url <<<"$target"
    n=$((n + 1))
    body="{\"n\":$n}"
    prev=$(newest_launch e2e-lpub webhook)
    N_START=$(date +%s)
    if [ "$(curl -s -o "$WORK/hook-N.json" -w '%{http_code}' -X POST -H 'content-type: application/json' \
        -H "x-hub-signature-256: $(sign "$N_SECRET" "$body")" --data "$body" "$url/hooks/$N_HOOK")" != 202 ]; then
        fail "[N] a delivery to $who was refused: $(cat "$WORK/hook-N.json")"
    fi
    wait_for 60 "the delivery to $who's launch" new_launch e2e-lpub webhook "$prev"
    KEY=$(newest_launch e2e-lpub webhook)
    expect_prompt "N-hook-$who" "$KEY" "$N_START" "$who accepted the delivery"
    N_LAT_HOOK="$N_LAT_HOOK $who=${LATENCY}s"
    settle "N-hook-$who" "$KEY"
    expect_finished "N-hook-$who"
done
if [ "$(curl -s -o "$WORK/hook-N.json" -w '%{http_code}' -X POST -H 'content-type: application/json' \
    -H "x-hub-signature-256: sha256=00" --data '{"n":9}' "$PEER_HOOKS_URL/hooks/$N_HOOK")" = 202 ]; then
    fail "[N] a delivery with a wrong signature was accepted"
fi
pass "[N] a delivery with a wrong signature is refused"

log "[N] killing the leader with a one-shot due 30s later"
N_DUE=$(($(date +%s) + 30))
CONTROLLER_URL="$PEER_URL" api POST /api/one-shots "{\"playbook\":\"e2e-lpub\",\"max_cost\":1,\"max_time\":\"5m\",\"fire_at\":\"$(iso "$N_DUE")\"}"
expect_http N-failover 201 '"playbook":"e2e-lpub"'
N_ONCE_ID=$(jq -r .id "$WORK/api.json")
kill -9 "$CONTROLLER_PID"
wait "$CONTROLLER_PID" 2>/dev/null || true
CONTROLLER_PID=""
N_KILL=$(date +%s)
wait_for 60 "the peer to take the lease" lease_is kind-b
N_HANDOVER=$(($(date +%s) - N_KILL))
wait_for 60 "the peer to finish its startup" grep -q 'issues re-enqueued at startup' "$WORK/controller-peer.log"
N_READY=$(($(date +%s) - N_KILL))
[ "$N_READY" -lt 30 ] || fail "[N] the peer took ${N_READY}s to lead, past the one-shot's due time"
wait_for 60 "the failover one-shot's launch" new_launch e2e-lpub deferred "$N_ONCE_KEY"
KEY=$(newest_launch e2e-lpub deferred)
expect_prompt N-failover "$KEY" "$N_DUE" "its fire_at; the peer took the lease ${N_HANDOVER}s and led ${N_READY}s after the kill"
N_LAT_FAILOVER=$LATENCY
CONTROLLER_URL="$PEER_URL" settle N-failover "$KEY"
expect_finished N-failover
sleep 5
[ "$(sql -c "SELECT count(*) FROM playbook_launches WHERE playbook = 'e2e-lpub' AND origin = 'deferred' AND key > '$N_ONCE_KEY'")" = 1 ] ||
    fail "[N] the failover one-shot launched $(sql -c "SELECT count(*) FROM playbook_launches WHERE playbook = 'e2e-lpub' AND origin = 'deferred' AND key > '$N_ONCE_KEY'") times"
[ "$(pod_count N "$KEY")" = 1 ] || fail "[N] the failover one-shot ran $(pod_count N "$KEY") pods"
pass "[N] the failover one-shot $N_ONCE_ID fired once, one launch and one pod"
watch_stop
stop_peer
pass "[N] latency to pod creation: standby launch ${N_LAT_STANDBY}s, one-shot ${N_LAT_ONCE}s, schedule ${N_LAT_CRON}s, webhook${N_LAT_HOOK}, failover one-shot ${N_LAT_FAILOVER}s (lease handover ${N_HANDOVER}s, leading ${N_READY}s after the kill), bound ${N_BOUND}s against the 300s discovery tick"

