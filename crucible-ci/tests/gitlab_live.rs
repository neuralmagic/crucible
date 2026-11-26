//! Read-only checks against a live GitLab project. Ignored by default; run with
//!
//! ```text
//! CRUCIBLE_CI_GITLAB_HOST=https://gitlab.example.com \
//! CRUCIBLE_CI_GITLAB_PROJECT=group/project \
//! CRUCIBLE_CI_GITLAB_TOKEN=$(glab config get token --host gitlab.example.com) \
//!   cargo test -p crucible-ci --test gitlab_live -- --ignored --nocapture
//! ```
//!
//! The test watches the commit of the project's latest pipeline, so it needs no fixed commit.

use crucible_ci::commit::CommitId;
use crucible_ci::gitlab::graph::{Shape, pipeline_graph, pipeline_state};
use crucible_ci::gitlab::{Gitlab, ProjectPath};
use crucible_ci::token::BearerToken;

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"))
}

#[tokio::test]
#[ignore = "reaches a live GitLab host"]
async fn watch_finds_and_follows_the_pipelines_of_a_live_commit() {
    let http = reqwest::Client::builder()
        .build()
        .expect("http client builds");
    let gl = Gitlab::new(http, &env("CRUCIBLE_CI_GITLAB_HOST")).expect("host parses");
    let project = ProjectPath::parse(&env("CRUCIBLE_CI_GITLAB_PROJECT")).expect("project parses");
    let token = BearerToken::new(env("CRUCIBLE_CI_GITLAB_TOKEN"));

    let lint = gl
        .lint(&token, &project, "main")
        .await
        .expect("the configuration validates");
    assert!(lint.valid, "{:?}", lint.errors);
    let shape = Shape::from_lint(&lint);
    println!(
        "stages {:?}, {} jobs with needs",
        shape.stages,
        shape.needs.len()
    );

    let config = gl
        .file(&token, &project, ".gitlab-ci.yml", "main")
        .await
        .expect("reading the configuration")
        .expect("the project has a .gitlab-ci.yml");
    let inputs = crucible_ci::gitlab::inputs::inputs(&config).expect("the header parses");
    println!("{} declared inputs", inputs.len());

    let latest = gl
        .latest_pipeline(&token, &project, "main")
        .await
        .expect("listing pipelines")
        .expect("main has a pipeline");
    let commit = CommitId::parse(&latest.sha).expect("GitLab reports full commits");
    let pipelines = gl
        .pipelines_for(&token, &project, &commit, &[])
        .await
        .expect("finding by commit");
    assert!(pipelines.iter().any(|p| p.id == latest.id));
    assert!(pipelines.iter().all(|p| p.sha == commit.as_str()));
    let pushes = gl
        .pipelines_for(&token, &project, &commit, &["push".to_owned()])
        .await
        .expect("finding pushes");
    assert!(pushes.iter().all(|p| p.source.as_deref() == Some("push")));

    for p in &pipelines {
        let fresh = gl
            .pipeline(&token, &project, p.id)
            .await
            .expect("reading the pipeline")
            .expect("it exists");
        let jobs = gl.jobs(&token, &project, p.id).await.expect("listing jobs");
        let bridges = gl
            .bridges(&token, &project, p.id)
            .await
            .expect("listing trigger jobs");
        let g = pipeline_graph(&jobs, &bridges, &shape);
        println!(
            "  pipeline {} ({:?}): {} nodes, {} edges, {:?}",
            p.id,
            p.source,
            g.graph.nodes.len(),
            g.graph.edges.len(),
            pipeline_state(&fresh, &jobs)
        );
        assert_eq!(g.graph.nodes.len(), jobs.len() + bridges.len());
    }

    let job = gl
        .jobs(&token, &project, latest.id)
        .await
        .expect("listing jobs")
        .into_iter()
        .find(|j| j.status == "success")
        .expect("the latest pipeline has a passed job");
    let log = gl
        .trace(&token, &project, job.id)
        .await
        .expect("reading the log");
    assert!(!log.is_empty());
    println!("job {} log: {} bytes", job.name, log.len());
}
