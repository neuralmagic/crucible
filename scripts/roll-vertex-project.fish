#!/usr/bin/env fish
# Roll prod crucible onto the crucible-729940 Vertex project.
#
#   scripts/roll-vertex-project.fish sa       # 1. service account + key + live proof
#   scripts/roll-vertex-project.fish secrets  # 2. replace crucible-vertex-adc in both clusters
#   scripts/roll-vertex-project.fish sync     # 3. after crucible-domains #451 is merged: Argo sync + wait
#   scripts/roll-vertex-project.fish restart  # 4. rollout restart, prints the project the controller runs with
#   scripts/roll-vertex-project.fish adopt    # 5. re-adopt the selfhost pack (needs the 8899 port-forward)
#
# Each step is idempotent and stops on the first failure.

set -l P crucible-729940
set -l SA crucible-agent@$P.iam.gserviceaccount.com
set -l KEY /tmp/claude-501/vertex/adc.json
set -l MPP "oc --context mpp-crucible-deployer -n crucible--runtime-ext"
set -l WAL "kubectl --context coreweave-waldorf -n crucible-system"

function step
    set_color -o cyan; echo "== $argv"; set_color normal
end

switch "$argv[1]"
    case sa
        step "service account in $P"
        if not gcloud iam service-accounts describe $SA --project $P >/dev/null 2>&1
            gcloud iam service-accounts create crucible-agent --project $P \
                --display-name "crucible agent (Vertex Claude for loop/turn pods and the controller ranker)"; or exit 1
        end
        step "roles/aiplatform.user"
        gcloud projects add-iam-policy-binding $P --member serviceAccount:$SA \
            --role roles/aiplatform.user --condition=None >/dev/null; or exit 1
        step "key -> $KEY"
        mkdir -p (dirname $KEY); and chmod 700 (dirname $KEY); or exit 1
        if test -s $KEY
            echo "key already exists, keeping it"
        else
            gcloud iam service-accounts keys create $KEY --iam-account $SA --project $P; or exit 1
        end
        step "live proof with the key (claude-opus-4-6 on $P)"
        env GOOGLE_APPLICATION_CREDENTIALS=$KEY CLAUDE_CODE_USE_VERTEX=1 \
            ANTHROPIC_VERTEX_PROJECT_ID=$P CLOUD_ML_REGION=global \
            claude -p "reply with the single word ok" --model claude-opus-4-6; or exit 1

    case secrets
        test -s $KEY; or begin; echo "no key at $KEY, run the sa step first"; exit 1; end
        step "MPP crucible--runtime-ext/crucible-vertex-adc"
        eval $MPP create secret generic crucible-vertex-adc --from-file=adc.json=$KEY \
            --dry-run=client -o yaml | eval $MPP apply -f -; or exit 1
        step "waldorf crucible-system/crucible-vertex-adc"
        eval $WAL create secret generic crucible-vertex-adc --from-file=adc.json=$KEY \
            --dry-run=client -o yaml | eval $WAL apply -f -; or exit 1
        echo
        echo "next: merge crucible-domains #451, sync the Argo Application, then run: restart"

    case sync
        step "Argo sync of Application/crucible-controller from main"
        set -l patch '{"operation":{"initiatedBy":{"username":"weaton"},"sync":{"revision":"main","prune":false}}}'
        oc --context mpp-crucible-deployer -n crucible--runtime-ext patch application crucible-controller \
            --type merge -p "$patch"; or exit 1
        for i in (seq 1 60)
            set -l st (oc --context mpp-crucible-deployer -n crucible--runtime-ext get application crucible-controller -o json | jq -r '[.status.operationState.phase // "?", .status.sync.status // "?"] | join(" ")')
            echo "  $st"
            string match -q "Succeeded Synced" -- $st; and break
            sleep 5
        end
        echo "next: restart"

    case restart
        step "controller rollout"
        eval $MPP rollout restart deploy/crucible-crucible-controller; or exit 1
        eval $MPP rollout status deploy/crucible-crucible-controller --timeout=5m; or exit 1
        step "controller sees the new project"
        oc --context mpp-crucible-deployer -n crucible--runtime-ext get deploy crucible-crucible-controller -o json \
            | jq -r '.spec.template.spec.containers[0].env[] | select(.name == "ANTHROPIC_VERTEX_PROJECT_ID") | .value'
        echo
        echo "when that prints $P you can delete the key: rm $KEY"

    case knobs
        step "run_iterations=10 run_max_cost=40 in the overrides ConfigMap (Argo sync resets these)"
        set -l f /tmp/claude-501/overrides.json
        test -s $f; or begin; echo "no $f"; exit 1; end
        oc --context mpp-crucible-deployer -n crucible--runtime-ext create cm crucible-controller-overrides \
            --from-file=overrides.json=$f --dry-run=client -o yaml \
            | oc --context mpp-crucible-deployer -n crucible--runtime-ext apply -f -; or exit 1

    case park
        set -l T (oc --context mpp-crucible-deployer -n crucible--runtime-ext get secret crucible-api-token -o jsonpath='{.data.token}' | base64 -d)
        test -n "$T"; or begin; echo "no API token"; exit 1; end
        set -l key (string escape --style=url "$argv[2]")
        test -n "$argv[2]"; or begin; echo "usage: park <issue-key>"; exit 2; end
        step "park $argv[2]"
        curl -sS -X POST "http://127.0.0.1:8899/api/issues/$key/park" \
            -H "authorization: Bearer $T" -H "x-auth-request-user: weaton" \
            -H "content-type: application/json" \
            -d '{"reason":"adopted with a git_ref the pinned engine cannot clone at (no --repo-ref on render-turn); superseded by a default-branch adoption"}'
        echo

    case adopt
        set -l T (oc --context mpp-crucible-deployer -n crucible--runtime-ext get secret crucible-api-token -o jsonpath='{.data.token}' | base64 -d)
        test -n "$T"; or begin; echo "no API token"; exit 1; end
        curl -s -m 5 -o /dev/null -H "authorization: Bearer $T" -H "x-auth-request-user: weaton" http://127.0.0.1:8899/api/config
        or begin; echo "port-forward on 8899 is down (the pod restarted); rerun: oc --context mpp-crucible-deployer -n crucible--runtime-ext port-forward svc/crucible-crucible-controller 8899:8080 &"; exit 1; end
        step "adopt examples/selfhost/scenario.json (pack_path: the pack is validated, not drafted)"
        cat /Users/weaton/git/crucible/examples/selfhost/scenario.json \
            | curl -sS -X POST http://127.0.0.1:8899/api/scenarios \
                -H "authorization: Bearer $T" -H "x-auth-request-user: weaton" \
                -H "content-type: application/json" -d @-
        echo

    case '*'
        echo "usage: roll-vertex-project.fish sa|secrets|sync|restart|knobs|park <key>|adopt"
        exit 2
end
