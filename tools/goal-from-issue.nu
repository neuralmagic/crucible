#!/usr/bin/env nu
# goal-from-issue — turn a forge issue into the per-run goal text (title + body) on stdout.
#
# This is what the production controller does from the webhook payload; locally we fetch
# straight from the GitHub REST API (no gh binary needed). Honors GITHUB_TOKEN / GH_TOKEN for
# private repos and higher rate limits; works unauthenticated for public repos.

def main [
    repo: string      # repository as owner/repo
    number: int       # issue number
] {
    if not ($repo | str contains "/") {
        print -e "repo must be owner/repo"
        exit 1
    }
    let token = ($env.GITHUB_TOKEN? | default ($env.GH_TOKEN? | default ""))
    # GitHub rejects API calls without a User-Agent; the bearer is optional (public repos).
    mut headers = {Accept: "application/vnd.github+json", "User-Agent": "crucible-goal-from-issue"}
    if ($token | is-not-empty) {
        $headers = ($headers | insert Authorization $"Bearer ($token)")
    }

    let issue = (http get --headers $headers $"https://api.github.com/repos/($repo)/issues/($number)")
    let title = ($issue.title? | default "")
    let body = ($issue.body? | default "")
    if (($title | is-empty) and ($body | is-empty)) {
        print -e $"issue ($repo)#($number) has no title or body"
        exit 1
    }
    print $"# ($title)\n\n($body)"
}
