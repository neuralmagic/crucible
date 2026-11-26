//! Conditional reads against a live GitHub run. Run with `--ignored` and
//! `FORGE_GITHUB_TOKEN`, `FORGE_GITHUB_REPO` (`owner/name`) and `FORGE_GITHUB_RUN` (a run id).

use forge::github::{Client, Repo, Since, Token};

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"))
}

#[tokio::test]
#[ignore = "reaches api.github.com"]
async fn an_unchanged_run_and_its_jobs_answer_not_modified() {
    let client = Client::new().expect("client");
    let token = Token::new(env("FORGE_GITHUB_TOKEN"));
    let repo = Repo::parse(&env("FORGE_GITHUB_REPO")).expect("repo");
    let run_id: u64 = env("FORGE_GITHUB_RUN").parse().expect("run id");

    let Since::Changed { value: run, etag } = client
        .run_since(&token, &repo, run_id, None)
        .await
        .expect("reading the run")
    else {
        panic!("a read without an ETag is never unchanged");
    };
    assert_eq!(run.id, run_id);
    let etag = etag.expect("GitHub sends an ETag");
    assert!(matches!(
        client
            .run_since(&token, &repo, run_id, Some(&etag))
            .await
            .expect("reading again"),
        Since::Unchanged
    ));

    let Since::Changed { value: jobs, etag } = client
        .jobs_since(&token, &repo, run_id, None)
        .await
        .expect("listing jobs")
    else {
        panic!("a read without an ETag is never unchanged");
    };
    assert!(!jobs.is_empty());
    let etag = etag.expect("GitHub sends an ETag");
    assert!(matches!(
        client
            .jobs_since(&token, &repo, run_id, Some(&etag))
            .await
            .expect("listing again"),
        Since::Unchanged
    ));
    assert!(matches!(
        client
            .run_since(&token, &repo, run_id, Some("\"stale\""))
            .await
            .expect("a stale ETag reads the run"),
        Since::Changed { .. }
    ));
}
