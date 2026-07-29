//! `forge-kube`: the engine's generic, domain-agnostic Kubernetes CLI. Domain `[world]` hooks
//! (apply/snapshot/restore scripts) call these subcommands instead of `kubectl`, so nothing in the
//! engine or the loop image shells out to `kubectl`. Each subcommand is a thin wrapper over
//! [`forge::kube`] (typed `kube-rs`); the domain orchestration logic stays in the hook.

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(about = "Typed Kubernetes operations for crucible domain hooks (no kubectl).")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print the image of a container in a deployment (empty if absent).
    CurrentImage {
        #[arg(long, short)]
        namespace: String,
        #[arg(long, short)]
        deployment: String,
        #[arg(long, short)]
        container: String,
    },
    /// Set a deployment container's image.
    SetImage {
        #[arg(long, short)]
        namespace: String,
        #[arg(long, short)]
        deployment: String,
        #[arg(long, short)]
        container: String,
        #[arg(long, short)]
        image: String,
    },
    /// Print a deployment's desired replica count (empty if absent).
    GetReplicas {
        #[arg(long, short)]
        namespace: String,
        #[arg(long, short)]
        deployment: String,
    },
    /// Set a deployment's desired replica count.
    SetReplicas {
        #[arg(long, short)]
        namespace: String,
        #[arg(long, short)]
        deployment: String,
        #[arg(long, short)]
        replicas: i32,
    },
    /// Print a container's literal env vars as `KEY=VALUE` lines (one per line, declaration order).
    GetEnv {
        #[arg(long, short)]
        namespace: String,
        #[arg(long, short)]
        deployment: String,
        #[arg(long, short)]
        container: String,
    },
    /// Replace a container's entire env list from `KEY=VALUE` lines read on stdin (a full replace,
    /// not a merge; see [`forge::kube::set_container_env`]).
    SetEnv {
        #[arg(long, short)]
        namespace: String,
        #[arg(long, short)]
        deployment: String,
        #[arg(long, short)]
        container: String,
    },
    /// Add an imagePullSecrets entry to a deployment's pod template.
    SetPullSecret {
        #[arg(long, short)]
        namespace: String,
        #[arg(long, short)]
        deployment: String,
        #[arg(long, short)]
        secret: String,
    },
    /// Set a deployment's RollingUpdate surge/unavailable (kill-old-first uses 0/1).
    SetRollingUpdate {
        #[arg(long, short)]
        namespace: String,
        #[arg(long, short)]
        deployment: String,
        #[arg(long)]
        max_surge: i32,
        #[arg(long)]
        max_unavailable: i32,
    },
    /// Trigger a rollout restart on one or more deployments (re-pull a mutable tag).
    RolloutRestart {
        #[arg(long, short)]
        namespace: String,
        /// Deployment name(s).
        deployments: Vec<String>,
    },
    /// Wait for one or more deployments to finish rolling out.
    RolloutStatus {
        #[arg(long, short)]
        namespace: String,
        #[arg(long, default_value = "300s")]
        timeout: String,
        deployments: Vec<String>,
    },
    /// Print the name of the first pod matching a label selector (empty if none).
    PodName {
        #[arg(long, short)]
        namespace: String,
        #[arg(long, short)]
        selector: String,
    },
    /// Exec a command in a pod container and print its stdout.
    Exec {
        #[arg(long, short)]
        namespace: String,
        #[arg(long, short)]
        pod: String,
        #[arg(long, short)]
        container: String,
        /// The command argv (after `--`).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Print the logs of a deployment's crashlooping/not-ready replicas (the crash traceback). For an
    /// apply hook to fold into its failure so the agent sees WHY a rollout didn't converge.
    CrashLogs {
        #[arg(long, short)]
        namespace: String,
        #[arg(long, short)]
        deployment: String,
        #[arg(long, short)]
        container: String,
        /// Lines to tail from each unhealthy replica.
        #[arg(long, default_value_t = 60)]
        tail: i64,
    },
    /// Server-side apply a multi-document YAML stream read from stdin.
    Apply,
    /// Print cluster GPU headroom = Σ node allocatable nvidia.com/gpu − Σ requested by running pods.
    FreeGpus,
    /// Print one `data` key of a ConfigMap (empty if absent).
    CmGet {
        #[arg(long, short)]
        namespace: String,
        #[arg(long, short)]
        configmap: String,
        #[arg(long, short)]
        key: String,
    },
    /// Merge-set one `data` key of a ConfigMap; the value is read from stdin (multi-line safe).
    CmSet {
        #[arg(long, short)]
        namespace: String,
        #[arg(long, short)]
        configmap: String,
        #[arg(long, short)]
        key: String,
    },
    /// Delete a namespace, ignoring a not-found (idempotent teardown).
    DeleteNamespace {
        #[arg(long, short)]
        namespace: String,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::CurrentImage {
            namespace,
            deployment,
            container,
        } => {
            if let Some(img) = forge::kube::current_image(&namespace, &deployment, &container)? {
                println!("{img}");
            }
        }
        Cmd::SetImage {
            namespace,
            deployment,
            container,
            image,
        } => forge::kube::set_image(&namespace, &deployment, &container, &image)?,
        Cmd::GetReplicas {
            namespace,
            deployment,
        } => {
            if let Some(n) = forge::kube::get_replicas(&namespace, &deployment)? {
                println!("{n}");
            }
        }
        Cmd::SetReplicas {
            namespace,
            deployment,
            replicas,
        } => forge::kube::set_replicas(&namespace, &deployment, replicas)?,
        Cmd::GetEnv {
            namespace,
            deployment,
            container,
        } => {
            let env = forge::kube::get_container_env(&namespace, &deployment, &container)?;
            print!("{}", forge::kube::format_env_lines(&env));
        }
        Cmd::SetEnv {
            namespace,
            deployment,
            container,
        } => {
            use std::io::Read;
            let mut input = String::new();
            std::io::stdin().read_to_string(&mut input)?;
            let env = forge::kube::parse_env_lines(&input)?;
            forge::kube::set_container_env(&namespace, &deployment, &container, &env)?;
        }
        Cmd::SetPullSecret {
            namespace,
            deployment,
            secret,
        } => forge::kube::set_pull_secret(&namespace, &deployment, &secret)?,
        Cmd::SetRollingUpdate {
            namespace,
            deployment,
            max_surge,
            max_unavailable,
        } => forge::kube::set_rolling_update(&namespace, &deployment, max_surge, max_unavailable)?,
        Cmd::RolloutRestart {
            namespace,
            deployments,
        } => {
            for d in &deployments {
                forge::kube::rollout_restart(&namespace, d)?;
            }
        }
        Cmd::RolloutStatus {
            namespace,
            timeout,
            deployments,
        } => {
            let dur = parse_timeout(&timeout);
            for d in &deployments {
                forge::kube::rollout_status(&namespace, d, dur)?;
            }
        }
        Cmd::PodName {
            namespace,
            selector,
        } => {
            if let Some(name) = forge::kube::pod_name_by_label(&namespace, &selector)? {
                println!("{name}");
            }
        }
        Cmd::Exec {
            namespace,
            pod,
            container,
            command,
        } => {
            let argv: Vec<&str> = command.iter().map(String::as_str).collect();
            print!(
                "{}",
                forge::kube::exec(&namespace, &pod, &container, &argv)?
            );
        }
        Cmd::CrashLogs {
            namespace,
            deployment,
            container,
            tail,
        } => {
            print!(
                "{}",
                forge::kube::crash_logs(&namespace, &deployment, &container, tail)?
            );
        }
        Cmd::Apply => {
            use std::io::Read;
            let mut yaml = String::new();
            std::io::stdin().read_to_string(&mut yaml)?;
            forge::kube::apply_yaml(&yaml)?;
        }
        Cmd::FreeGpus => println!("{}", forge::kube::free_gpus()?),
        Cmd::CmGet {
            namespace,
            configmap,
            key,
        } => {
            if let Some(v) = forge::kube::get_configmap_key(&namespace, &configmap, &key)? {
                print!("{v}");
            }
        }
        Cmd::CmSet {
            namespace,
            configmap,
            key,
        } => {
            use std::io::Read;
            let mut value = String::new();
            std::io::stdin().read_to_string(&mut value)?;
            forge::kube::set_configmap_key(&namespace, &configmap, &key, &value)?;
        }
        Cmd::DeleteNamespace { namespace } => forge::kube::delete_namespace(&namespace)?,
    }
    Ok(())
}

/// Parse a `kubectl`-style timeout (`300s`, `10m`, `1h`) via jiff's friendly format; 300s fallback.
fn parse_timeout(s: &str) -> std::time::Duration {
    s.trim()
        .parse::<jiff::SignedDuration>()
        .ok()
        .and_then(|d| std::time::Duration::try_from(d).ok())
        .unwrap_or(std::time::Duration::from_secs(300))
}
