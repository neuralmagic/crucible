#!/usr/bin/env bash
# Set up the quay.io organisation Crucible's images live in (ADR-0080):
#   1. the repositories this repo's CI pushes, public unless listed as private;
#   2. a read team that every repository grants read to, now and when created;
#   3. the read robot's membership of that team, and a CI robot with write on those repositories;
#   4. the CI robot's credentials as this repo's Actions secrets;
#   5. the read robot's dockerconfig in Vault and as the `crucible-pull` Secret.
#
# It reads the organisation, prints what it would change and asks before each stage, so it is
# safe to run again. --dry-run stops after printing stages 1 to 3.
#
# It prompts for a Quay OAuth application token with org:admin, repo:admin and repo:create. No
# credential is written to disk, printed, or passed as a command argument.
#
# Needs curl and jq; cargo for the default private list; gh, vault, kubectl for stages 4 and 5.
#
# Env, each prompted for when unset:
#   QUAY_ORG            organisation                              [crucible-ai]
#   QUAY_READ_TEAM      team that reads every repository          [pull]
#   QUAY_PULL_ROBOT     short name of the existing read robot
#   QUAY_CI_ROBOT       short name of this repo's CI push robot   [crucible_ci]
#   QUAY_PUBLIC_REPOS   space-separated public repositories       [core images, cargo xtask images list]
#   QUAY_PRIVATE_REPOS  space-separated private repositories; a name here is taken off the public list
#   GH_REPO             repository the Actions secrets go to      [gh repo view]
#   KUBECTL             kubectl or oc                             [kubectl, else oc]
set -euo pipefail

API=https://quay.io/api/v1
CORE_REPOS="crucible crucible-controller crucible-buildit crucible-kueue openshell-gateway openshell-supervisor openshell-sandbox"
PULL_SECRET=crucible-pull

DRY_RUN=0
QUAY_TOKEN=
API_STATUS=
API_BODY=

say() { printf '%s\n' "$*" >&2; }
die() {
    say "error: $*"
    exit 1
}

ask() {
    local prompt=$1 default=${2-} answer
    if [ -n "$default" ]; then
        read -r -p "$prompt [$default]: " answer
    else
        read -r -p "$prompt: " answer
    fi
    printf '%s' "${answer:-$default}"
}

confirm() {
    local answer
    read -r -p "$1 [y/N]: " answer
    [ "$answer" = y ] || [ "$answer" = Y ]
}

api() {
    local method=$1 path=$2 body=${3-} out
    local data=()
    [ -z "$body" ] || data=(--data "$body")
    out=$(printf 'header = "Authorization: Bearer %s"\n' "$QUAY_TOKEN" |
        curl -sS -K - -X "$method" -H 'Content-Type: application/json' \
            ${data[@]+"${data[@]}"} -w '\n%{http_code}' "$API$path") ||
        die "$method $path: curl failed"
    API_STATUS=${out##*$'\n'}
    API_BODY=${out%$'\n'*}
}

# Never the raw body: a robot response carries its token.
api_message() {
    printf '%s' "$API_BODY" |
        jq -r '.error_message // .message // .detail // .title // empty' 2>/dev/null || true
}

call() {
    local what=$1
    shift
    api "$@"
    case "$API_STATUS" in
    2*) ;;
    402) die "$what: the organisation's plan has no private repositories left (HTTP 402)" ;;
    *) die "$what: HTTP $API_STATUS $(api_message)" ;;
    esac
}

field() { printf '%s' "$API_BODY" | jq -r "$@"; }

contains() {
    local want=$1 item
    shift
    for item in "$@"; do
        [ "$item" != "$want" ] || return 0
    done
    return 1
}

need() { command -v "$1" >/dev/null 2>&1 || die "$1 is not installed"; }

dockerconfig() {
    printf '%s:%s' "$1" "$2" | base64 | tr -d '\n' |
        jq -Rc --arg registry "quay.io/$QUAY_ORG" '{auths: {($registry): {auth: .}}}'
}

robot_token() {
    call "reading robot $1" GET "/organization/$QUAY_ORG/robots/$1"
    field '.token // empty'
}

load_state() {
    api GET "/organization/$QUAY_ORG/robots?token=false"
    case "$API_STATUS" in
    2*) ;;
    *) die "the token cannot administer quay.io/$QUAY_ORG: HTTP $API_STATUS $(api_message)" ;;
    esac
    ROBOTS=$(field '[.robots[].name | sub("^[^+]*\\+"; "")] | join(" ")')

    call "reading organisation $QUAY_ORG" GET "/organization/$QUAY_ORG"
    TEAMS=$(field '(.teams // {}) | keys | join(" ")')

    REPOS_PUBLIC=
    REPOS_PRIVATE=
    local page=
    while :; do
        call "listing repositories" GET "/repository?namespace=$QUAY_ORG${page:+&next_page=$page}"
        REPOS_PUBLIC="$REPOS_PUBLIC $(field '[.repositories[] | select(.is_public) | .name] | join(" ")')"
        REPOS_PRIVATE="$REPOS_PRIVATE $(field '[.repositories[] | select(.is_public | not) | .name] | join(" ")')"
        page=$(field '(.next_page // empty) | @uri')
        [ -n "$page" ] || break
    done

    call "listing default permissions" GET "/organization/$QUAY_ORG/prototypes"
    # shellcheck disable=SC2016
    READ_PROTOTYPE=$(field --arg team "$QUAY_READ_TEAM" \
        '[.prototypes[] | select(.delegate.kind == "team" and .delegate.name == $team
            and .role == "read" and .activating_user == null)] | length')
}

plan_repos() {
    CREATE_PUBLIC=()
    CREATE_PRIVATE=()
    MAKE_PUBLIC=()
    MAKE_PRIVATE=()
    local repo
    # shellcheck disable=SC2086
    for repo in $QUAY_PUBLIC_REPOS; do
        if contains "$repo" $REPOS_PUBLIC; then
            :
        elif contains "$repo" $REPOS_PRIVATE; then
            MAKE_PUBLIC+=("$repo")
        else
            CREATE_PUBLIC+=("$repo")
        fi
    done
    # shellcheck disable=SC2086
    for repo in $QUAY_PRIVATE_REPOS; do
        if contains "$repo" $REPOS_PRIVATE; then
            :
        elif contains "$repo" $REPOS_PUBLIC; then
            MAKE_PRIVATE+=("$repo")
        else
            CREATE_PRIVATE+=("$repo")
        fi
    done
}

show() {
    local label=$1
    shift
    [ $# -eq 0 ] || say "  $label: $*"
}

stage_repos() {
    say
    say "1. Repositories in quay.io/$QUAY_ORG"
    show "create public" ${CREATE_PUBLIC[@]+"${CREATE_PUBLIC[@]}"}
    show "create private" ${CREATE_PRIVATE[@]+"${CREATE_PRIVATE[@]}"}
    show "private -> public" ${MAKE_PUBLIC[@]+"${MAKE_PUBLIC[@]}"}
    show "public -> private" ${MAKE_PRIVATE[@]+"${MAKE_PRIVATE[@]}"}
    local changes=$((${#CREATE_PUBLIC[@]} + ${#CREATE_PRIVATE[@]} + ${#MAKE_PUBLIC[@]} + ${#MAKE_PRIVATE[@]}))
    if [ "$changes" -eq 0 ]; then
        say "  nothing to change"
        return
    fi
    [ "$DRY_RUN" -eq 0 ] || return 0
    confirm "Apply these $changes repository changes?" || {
        say "  skipped"
        return
    }
    local repo
    for repo in ${CREATE_PUBLIC[@]+"${CREATE_PUBLIC[@]}"}; do
        create_repo "$repo" public
    done
    for repo in ${CREATE_PRIVATE[@]+"${CREATE_PRIVATE[@]}"}; do
        create_repo "$repo" private
    done
    for repo in ${MAKE_PUBLIC[@]+"${MAKE_PUBLIC[@]}"}; do
        call "making $repo public" POST "/repository/$QUAY_ORG/$repo/changevisibility" '{"visibility":"public"}'
        say "  $repo is public"
    done
    for repo in ${MAKE_PRIVATE[@]+"${MAKE_PRIVATE[@]}"}; do
        call "making $repo private" POST "/repository/$QUAY_ORG/$repo/changevisibility" '{"visibility":"private"}'
        say "  $repo is private"
    done
    say "  done"
}

create_repo() {
    local body
    body=$(jq -cn --arg ns "$QUAY_ORG" --arg repo "$1" --arg vis "$2" \
        '{namespace: $ns, repository: $repo, visibility: $vis, description: "", repo_kind: "image"}')
    call "creating $1" POST /repository "$body"
    say "  created $1 ($2)"
}

stage_read_team() {
    # shellcheck disable=SC2086
    local all="$REPOS_PUBLIC $REPOS_PRIVATE $QUAY_PUBLIC_REPOS $QUAY_PRIVATE_REPOS"
    # shellcheck disable=SC2086
    all=$(printf '%s\n' $all | sort -u | tr '\n' ' ')
    say
    say "2. Read team $QUAY_READ_TEAM"
    # shellcheck disable=SC2086
    contains "$QUAY_READ_TEAM" $TEAMS || say "  create team $QUAY_READ_TEAM"
    [ "$READ_PROTOTYPE" -gt 0 ] || say "  default permission: new repositories grant it read"
    # shellcheck disable=SC2086
    say "  grant it read on $(printf '%s\n' $all | wc -l | tr -d ' ') repositories (every one in the organisation)"
    say "  add robot $QUAY_ORG+$QUAY_PULL_ROBOT to it"
    [ "$DRY_RUN" -eq 0 ] || return 0
    confirm "Apply the read team?" || {
        say "  skipped"
        return
    }
    # shellcheck disable=SC2086
    contains "$QUAY_READ_TEAM" $TEAMS ||
        call "creating team $QUAY_READ_TEAM" PUT "/organization/$QUAY_ORG/team/$QUAY_READ_TEAM" \
            '{"role":"member","description":"Reads every repository. Cluster pull robots."}'
    if [ "$READ_PROTOTYPE" -eq 0 ]; then
        call "adding the default permission" POST "/organization/$QUAY_ORG/prototypes" \
            "$(jq -cn --arg team "$QUAY_READ_TEAM" '{role: "read", delegate: {name: $team, kind: "team"}}')"
    fi
    local repo
    for repo in $all; do
        call "granting read on $repo" PUT \
            "/repository/$QUAY_ORG/$repo/permissions/team/$QUAY_READ_TEAM" '{"role":"read"}'
    done
    call "adding $QUAY_PULL_ROBOT to $QUAY_READ_TEAM" PUT \
        "/organization/$QUAY_ORG/team/$QUAY_READ_TEAM/members/$QUAY_ORG+$QUAY_PULL_ROBOT"
    say "  done"
}

stage_ci_robot() {
    say
    say "3. CI robot $QUAY_ORG+$QUAY_CI_ROBOT"
    local repo writes= missing=
    # shellcheck disable=SC2086
    if contains "$QUAY_CI_ROBOT" $ROBOTS; then
        call "reading the robot's permissions" GET \
            "/organization/$QUAY_ORG/robots/$QUAY_CI_ROBOT/permissions"
        writes=$(field '[.permissions[]? | select(.role == "write" or .role == "admin")
            | .repository.name] | join(" ")')
    else
        say "  create it"
    fi
    for repo in $QUAY_PUBLIC_REPOS $QUAY_PRIVATE_REPOS; do
        # shellcheck disable=SC2086
        contains "$repo" $writes || missing="$missing $repo"
    done
    if [ -z "$missing" ]; then
        say "  it already has write on every repository"
        return
    fi
    say "  grant it write on:$missing"
    [ "$DRY_RUN" -eq 0 ] || return 0
    confirm "Apply the CI robot?" || {
        say "  skipped"
        return
    }
    # shellcheck disable=SC2086
    contains "$QUAY_CI_ROBOT" $ROBOTS ||
        call "creating robot $QUAY_CI_ROBOT" PUT "/organization/$QUAY_ORG/robots/$QUAY_CI_ROBOT" \
            "$(jq -cn --arg d "CI push for ${GH_REPO:-this repository}" '{description: $d}')"
    for repo in $missing; do
        call "granting write on $repo" PUT \
            "/repository/$QUAY_ORG/$repo/permissions/user/$QUAY_ORG+$QUAY_CI_ROBOT" '{"role":"write"}'
    done
    say "  done"
}

stage_actions_secrets() {
    say
    say "4. Actions secrets on $GH_REPO"
    say "  QUAY_CRUCIBLE_AI_PUSH_USER and QUAY_CRUCIBLE_AI_PUSH_TOKEN = robot $QUAY_ORG+$QUAY_CI_ROBOT"
    confirm "Set them?" || {
        say "  skipped"
        return
    }
    need gh
    local token
    token=$(robot_token "$QUAY_CI_ROBOT")
    [ -n "$token" ] || die "robot $QUAY_CI_ROBOT has no token to read; run stage 3 first"
    printf '%s' "$QUAY_ORG+$QUAY_CI_ROBOT" | gh secret set QUAY_CRUCIBLE_AI_PUSH_USER --repo "$GH_REPO"
    printf '%s' "$token" | gh secret set QUAY_CRUCIBLE_AI_PUSH_TOKEN --repo "$GH_REPO"
    say "  done"
}

stage_vault() {
    say
    say "5a. Read robot's dockerconfig in Vault (what the hub projects as $PULL_SECRET)"
    say "  The entry is keyed quay.io/$QUAY_ORG. A controller that reads this Secret as its authfile"
    say "  must look keys up by registry/path, or it loses the private loop image. Skip if unsure."
    local path key
    path=$(ask "  Vault KV path, empty to skip")
    [ -n "$path" ] || {
        say "  skipped"
        return
    }
    key=$(ask "  field" config)
    need vault
    confirm "Write field $key at $path?" || {
        say "  skipped"
        return
    }
    local verb=put
    ! vault kv get "$path" >/dev/null 2>&1 || verb="patch"
    dockerconfig "$QUAY_ORG+$QUAY_PULL_ROBOT" "$PULL_TOKEN" | vault kv "$verb" "$path" "$key=-" >/dev/null
    say "  done (vault kv $verb)"
}

stage_clusters() {
    say
    say "5b. Secret $PULL_SECRET on clusters, until the controller writes it"
    local kubectl=${KUBECTL:-}
    local context namespaces namespace keys
    while :; do
        context=$(ask "  kube context, empty to finish")
        [ -n "$context" ] || break
        if [ -z "$kubectl" ]; then
            kubectl=kubectl
            command -v kubectl >/dev/null 2>&1 || kubectl=oc
            need "$kubectl"
        fi
        namespaces=$(ask "  namespaces on $context, space-separated")
        for namespace in $namespaces; do
            keys=$("$kubectl" --context "$context" -n "$namespace" get secret "$PULL_SECRET" \
                -o json 2>/dev/null |
                jq -r '.data[".dockerconfigjson"] | @base64d | fromjson | .auths | keys | join(", ")' \
                    2>/dev/null || true)
            if [ -n "$keys" ]; then
                say "  $namespace/$PULL_SECRET holds entries for: $keys. They are replaced by quay.io/$QUAY_ORG alone."
            else
                say "  $namespace/$PULL_SECRET does not exist"
            fi
            say "  Skip a namespace whose controller reads $PULL_SECRET as its authfile, as above."
            confirm "  Write $PULL_SECRET in $namespace on $context?" || continue
            dockerconfig "$QUAY_ORG+$QUAY_PULL_ROBOT" "$PULL_TOKEN" |
                "$kubectl" --context "$context" -n "$namespace" create secret generic "$PULL_SECRET" \
                    --type=kubernetes.io/dockerconfigjson --from-file=.dockerconfigjson=/dev/stdin \
                    --dry-run=client -o yaml |
                "$kubectl" --context "$context" -n "$namespace" apply -f - >/dev/null
            say "  written"
        done
    done
}

main() {
    case "${1-}" in
    --dry-run) DRY_RUN=1 ;;
    "") ;;
    *) die "usage: $0 [--dry-run]" ;;
    esac
    need curl
    need jq

    QUAY_ORG=${QUAY_ORG:-$(ask "Quay organisation" crucible-ai)}
    QUAY_READ_TEAM=${QUAY_READ_TEAM:-$(ask "Read team" pull)}
    QUAY_CI_ROBOT=${QUAY_CI_ROBOT:-$(ask "CI push robot (short name)" crucible_ci)}
    QUAY_PRIVATE_REPOS=${QUAY_PRIVATE_REPOS:-}
    if [ -z "${QUAY_PUBLIC_REPOS:-}" ]; then
        need cargo
        say "listing the sandbox feedstock (cargo xtask images list)"
        QUAY_PUBLIC_REPOS="$CORE_REPOS $(cd "$(dirname "$0")/.." && cargo xtask images list | tr '\n' ' ')"
    fi
    local repo public=
    for repo in $QUAY_PUBLIC_REPOS; do
        # shellcheck disable=SC2086
        contains "$repo" $QUAY_PRIVATE_REPOS || public="$public $repo"
    done
    QUAY_PUBLIC_REPOS=$public
    if [ -z "${GH_REPO:-}" ] && [ "$DRY_RUN" -eq 0 ]; then
        GH_REPO=$(ask "GitHub repository for the Actions secrets" \
            "$(gh repo view --json nameWithOwner -q .nameWithOwner 2>/dev/null || true)")
    fi

    read -r -s -p "Quay OAuth token (org:admin, repo:admin, repo:create): " QUAY_TOKEN
    say
    QUAY_TOKEN=$(printf '%s' "$QUAY_TOKEN" | tr -d '[:space:]')
    case "$QUAY_TOKEN" in
    "") die "no token was entered" ;;
    *[!A-Za-z0-9_-]*) die "the ${#QUAY_TOKEN} characters entered include some a Quay token never has" ;;
    esac

    load_state
    if [ -z "${QUAY_PULL_ROBOT:-}" ]; then
        say "robots in $QUAY_ORG: ${ROBOTS:-none}"
        QUAY_PULL_ROBOT=$(ask "Existing read robot (short name)")
    fi
    # shellcheck disable=SC2086
    contains "$QUAY_PULL_ROBOT" $ROBOTS || die "no robot $QUAY_ORG+$QUAY_PULL_ROBOT"
    [ "$QUAY_PULL_ROBOT" != "$QUAY_CI_ROBOT" ] || die "the read robot and the CI robot must differ"

    plan_repos
    stage_repos
    stage_read_team
    stage_ci_robot
    if [ "$DRY_RUN" -eq 1 ]; then
        say
        say "dry run: nothing was changed; stages 4 and 5 were not shown"
        return
    fi
    stage_actions_secrets

    PULL_TOKEN=$(robot_token "$QUAY_PULL_ROBOT")
    [ -n "$PULL_TOKEN" ] || die "robot $QUAY_PULL_ROBOT has no token to read"
    stage_vault
    stage_clusters
}

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
    main "$@"
fi
