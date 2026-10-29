#![allow(clippy::disallowed_macros)]

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

use crate::pod::BuilderPod;
use crate::{BuildArgs, Mode, Schedule};
use crate::{auth, backend, context, job, oci, pod, schedule};

// quay.io/foo:tag -> quay.io/foo@sha256:...
pub(crate) fn pinned_ref(image: &str, digest: &str) -> String {
    format!("{}@{digest}", oci::repo_of(image))
}

fn check(args: &BuildArgs) -> Result<()> {
    if args.no_push && args.mode == Mode::Job {
        bail!("--no-push needs --mode pod: a job build pushes its context and its image");
    }
    Ok(())
}

pub async fn run(client: kube::Client, args: &BuildArgs) -> Result<()> {
    check(args)?;
    let image = args.image()?;
    let authfile = auth::registry_of(image).and_then(|r| auth::minimal_authfile(&r));
    let authfile = match (authfile, args.no_push) {
        (Ok(authfile), _) => authfile,
        (Err(e), true) => {
            tracing::info!("--no-push: shipping no registry credentials ({e:#})");
            br#"{"auths":{}}"#.to_vec()
        }
        (Err(e), false) => return Err(e),
    };

    tracing::info!(
        "context: {}  dockerfile: {}  ->  {}  (ns={}, backend={:?}, mode={:?})",
        args.context.display(),
        args.dockerfile,
        image,
        args.namespace,
        args.backend,
        args.mode
    );

    let idle_nodes = match (args.node.as_deref(), args.schedule) {
        // explicit pin makes the scout pointless
        (Some(_), _) | (None, Schedule::Any) => Vec::new(),
        (None, Schedule::Idle) => {
            let nodes = schedule::idle_nodes(client.clone()).await;
            tracing::info!(
                "{} idle node(s) without GPU workloads, preferring them (best effort)",
                nodes.len()
            );
            nodes
        }
    };

    if let Some(backend::CacheVolume::Pvc(name)) = args.cache() {
        pod::ensure_pvc(client.clone(), &args.namespace, name, &args.cache_size).await?;
    }

    if args.mode == Mode::Job {
        return detach(client, args, &authfile, &idle_nodes).await;
    }

    let tarball = context::tarball(&args.context)?;
    tracing::info!("context tarball: {} KiB", tarball.len() / 1024);

    let resources = args.resources();
    let opts = backend::PodOpts {
        idle_nodes: &idle_nodes,
        resources: &resources,
        cache: args.cache(),
        node: args.node.as_deref(),
    };
    let pod = BuilderPod::create(client, &args.namespace, args.backend, &opts).await?;
    tracing::info!("builder pod {} created, waiting for ready", pod.name);

    let outcome = tokio::select! {
        result = drive(&pod, args, &tarball, &authfile) => result,
        _ = tokio::signal::ctrl_c() => Err(anyhow!("interrupted, cleaning up")),
    };

    if let Err(err) = pod.delete().await {
        tracing::warn!("failed to delete pod {}: {err:#}", pod.name);
        tracing::warn!("run `buildit clean -n {}` to remove it", args.namespace);
    } else {
        tracing::info!("builder pod {} deleted", pod.name);
    }

    match outcome? {
        Some(digest) => {
            tracing::info!("pushed {}", image);
            println!("{}", pinned_ref(image, &digest));
        }
        None => tracing::info!("built {} without pushing it", image),
    }
    Ok(())
}

// push context to the registry, hand the build to a Job, print the handle
async fn detach(
    client: kube::Client,
    args: &BuildArgs,
    authfile: &[u8],
    idle_nodes: &[String],
) -> Result<()> {
    let image = args.image()?;
    let tar = context::tar_bytes(&args.context)?;
    tracing::info!("context tar: {} KiB", tar.len() / 1024);
    let ctx_ref = oci::context_reference(image, &tar);
    let registry = auth::registry_of(image)?;
    let (user, pass) = auth::basic_credentials(&registry)?;
    let reg_auth = oci_client::secrets::RegistryAuth::Basic(user, pass);
    let ctx_labels = if args.context_labels.is_empty() {
        let defaults = oci::default_context_labels(&registry);
        for (k, v) in &defaults {
            tracing::info!("context label default for {registry}: {k}={v}");
        }
        defaults
    } else {
        args.context_labels.clone()
    };
    oci::push_context(&ctx_ref, tar, &reg_auth, &ctx_labels).await?;

    let job = job::create(
        client,
        &args.namespace,
        args.backend,
        &job::DetachArgs {
            image,
            dockerfile: &args.dockerfile,
            target: args.target.as_deref(),
            build_args: &args.build_args,
            ctx_ref: &ctx_ref,
            authfile,
            idle_nodes,
            resources: &args.resources(),
            labels: &args.labels,
            cache: args.cache(),
            node: args.node.as_deref(),
        },
    )
    .await?;
    tracing::info!(
        "job {job} created; follow with `buildit wait {job} -n {}`",
        args.namespace
    );
    println!("{job}");
    Ok(())
}

// --output render: print manifests as YAML, touch nothing
pub fn render(args: &BuildArgs) -> Result<()> {
    check(args)?;
    let image = args.image()?;
    let name = crate::pod::unique_name();
    let resources = args.resources();
    match args.mode {
        Mode::Pod => {
            let opts = backend::PodOpts {
                idle_nodes: &[],
                resources: &resources,
                cache: args.cache(),
                node: args.node.as_deref(),
            };
            let pod = args.backend.pod_spec(&name, &args.namespace, &opts)?;
            print!("{}", serde_norway::to_string(&pod)?);
        }
        Mode::Job => {
            let tar = context::tar_bytes(&args.context)?;
            let ctx_ref = oci::context_reference(image, &tar);
            let spec = args.backend.job_spec(
                &name,
                &args.namespace,
                &job::DetachArgs {
                    image,
                    dockerfile: &args.dockerfile,
                    target: args.target.as_deref(),
                    build_args: &args.build_args,
                    ctx_ref: &ctx_ref,
                    authfile: b"",
                    idle_nodes: &[],
                    resources: &resources,
                    labels: &args.labels,
                    cache: args.cache(),
                    node: args.node.as_deref(),
                },
            )?;
            tracing::info!("secret data redacted; a real run ships your registry token");
            tracing::info!("a real run pushes the context to {ctx_ref} first");
            let secret = job::secret_manifest(&name, &args.namespace, b"<redacted>", None);
            print!(
                "{}---\n{}",
                serde_norway::to_string(&spec)?,
                serde_norway::to_string(&secret)?
            );
        }
    }
    Ok(())
}

async fn drive(
    pod: &BuilderPod,
    args: &BuildArgs,
    tarball: &[u8],
    authfile: &[u8],
) -> Result<Option<String>> {
    let image = args.image()?;
    let backend = args.backend;
    pod.wait_ready(Duration::from_secs(180)).await?;

    tracing::info!("staging source + auth");
    pod.exec_stream(&backend.setup_command()).await?;
    pod.exec_with_stdin(&backend.untar_command(), tarball)
        .await?;
    pod.exec_with_stdin(&backend.auth_upload_command(), authfile)
        .await?;

    let push = !args.no_push;
    tracing::info!(
        "building{} (this can take several minutes)",
        if push { " + pushing" } else { "" }
    );
    for step in backend.build_steps(
        image,
        &args.dockerfile,
        args.target.as_deref(),
        push,
        &args.build_args,
        &args.labels,
    ) {
        pod.exec_stream(&step).await?;
    }
    if !push {
        return Ok(None);
    }

    let raw = pod
        .exec_capture(&backend.digest_command())
        .await
        .context("reading image digest")?;
    backend.digest_from(&raw).map(Some)
}

#[cfg(test)]
mod tests {
    use crate::build::{pinned_ref, render, run};
    use crate::{BuildArgs, Cli, Cmd};

    fn build_args(argv: &[&str]) -> BuildArgs {
        let cli =
            <Cli as clap::Parser>::try_parse_from(["buildit", "build"].iter().chain(argv)).unwrap();
        match cli.cmd {
            Cmd::Build(args) => *args,
            _ => panic!("not a build"),
        }
    }

    #[test]
    fn target_and_no_push_parse_and_no_push_refuses_job_mode() {
        let args = build_args(&["quay.io/acme/foo:tag", "--target", "base", "--no-push"]);
        assert_eq!(args.target.as_deref(), Some("base"));
        assert!(args.no_push);
        let args = build_args(&["quay.io/acme/foo:tag"]);
        assert_eq!(args.target, None);
        assert!(!args.no_push);

        let job = build_args(&[
            "quay.io/acme/foo:tag",
            "--no-push",
            "--mode",
            "job",
            "--output",
            "render",
        ]);
        let e = render(&job).unwrap_err();
        assert!(
            format!("{e:#}").contains("--no-push needs --mode pod"),
            "{e:#}"
        );
        let pod = build_args(&["quay.io/acme/foo:tag", "--no-push", "--output", "render"]);
        render(&pod).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a cluster: BUILDIT_E2E_KUBECONTEXT=kind-x cargo test -- --ignored"]
    async fn e2e_cli_builds_a_target_stage_without_pushing() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let context = std::env::var("BUILDIT_E2E_KUBECONTEXT").unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Dockerfile"),
            "FROM docker.io/library/busybox:latest AS base\n\
             RUN echo stage-base\n\
             FROM base AS broken\n\
             RUN exit 1\n",
        )
        .unwrap();
        let ctx = dir.path().to_str().unwrap();
        let client = crate::client_for(Some(&context)).await.unwrap();
        for backend in ["buildah", "buildkit", "kaniko"] {
            let args = |target: &[&str]| {
                let mut argv = vec![
                    "localhost/buildit-e2e:dev",
                    "--context",
                    ctx,
                    "--backend",
                    backend,
                    "--schedule",
                    "any",
                    "--namespace",
                    "default",
                    "--no-push",
                ];
                argv.extend(target);
                build_args(&argv)
            };
            run(client.clone(), &args(&["--target", "base"]))
                .await
                .unwrap_or_else(|e| panic!("{backend}: {e:#}"));
            assert!(
                run(client.clone(), &args(&[])).await.is_err(),
                "{backend}: the broken final stage built"
            );
        }
    }

    #[test]
    fn pin_swaps_tag_for_digest() {
        assert_eq!(
            pinned_ref("quay.io/acme/foo:tag", "sha256:abc"),
            "quay.io/acme/foo@sha256:abc"
        );
        assert_eq!(
            pinned_ref("localhost:5000/foo", "sha256:abc"),
            "localhost:5000/foo@sha256:abc"
        );
        assert_eq!(
            pinned_ref("quay.io/acme/foo", "sha256:abc"),
            "quay.io/acme/foo@sha256:abc"
        );
    }
}
