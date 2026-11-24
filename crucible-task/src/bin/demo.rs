//! A task written with the SDK, run by the crate's integration test.

use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
struct Params {
    profile: String,
}

#[derive(Deserialize)]
struct Stage {
    members: Vec<String>,
}

#[derive(Serialize)]
struct Out {
    task: String,
    tickets: usize,
}

fn main() -> std::process::ExitCode {
    crucible_task::run(|task| {
        let params: Params = task.params()?;
        let stage: Stage = task.input("stage")?;
        let manifest = std::fs::read_to_string(task.input_file("stage", "manifest.txt")?)
            .map_err(|error| error.to_string())?;
        println!("working on {}", params.profile);
        task.write_file(
            "REPORT.md",
            format!(
                "{}: {} ({})\n",
                params.profile,
                stage.members.join(", "),
                manifest.trim()
            ),
        )?;
        Ok::<_, Failure>(Out {
            task: task.name().to_owned(),
            tickets: stage.members.len(),
        })
    })
}

struct Failure(String);

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<crucible_task::Error> for Failure {
    fn from(error: crucible_task::Error) -> Self {
        Failure(error.to_string())
    }
}

impl From<String> for Failure {
    fn from(error: String) -> Self {
        Failure(error)
    }
}
