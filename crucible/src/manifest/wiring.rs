//! The `CommandWorld`/`CompositeWorld`/`GitWorld`/`CommandJudge` construction that turns a parsed
//! manifest into the engine's `World`/`Judge` trait objects. Kept apart from the plain-serde config
//! structs in the rest of `manifest/` so a future leaf split of the config types stays cheap.

use crate::command_judge::CommandJudge;
use crate::command_world::{CommandWorld, CompositeWorld, GitWorld};
use crate::crucible::{Judge, World};
use crate::manifest::{CompositeManifest, Manifest};
use crate::task_judge::TaskJudge;
use anyhow::Result;
use std::path::{Path, PathBuf};

impl Manifest {
    /// `GitWorld` when no `[world]` commands are given at all (the 80% case), else
    /// `CommandWorld` (git memory plus any of apply/snapshot/restore). Boxed `+ Send` so a
    /// front-end can move the world onto a worker thread.
    pub fn build_world(&self, workspace: PathBuf) -> Box<dyn World + Send> {
        let carry_forward = self.workspace.carried_paths();
        let excludes: Vec<String> = carry_forward
            .iter()
            .cloned()
            .chain(std::iter::once(format!("{}/", crate::plan::STAGED_INPUTS)))
            .collect();
        crucible_vcs::git_memory::install_harness_excludes(&workspace, &excludes);
        let w = &self.world;
        if w.apply_cmd.is_none() && w.snapshot_cmd.is_none() && w.restore_cmd.is_none() {
            Box::new(GitWorld {
                workspace,
                carry_forward,
            })
        } else {
            Box::new(CommandWorld {
                workspace,
                apply_cmd: w.apply_cmd.clone(),
                snapshot_cmd: w.snapshot_cmd.clone(),
                restore_cmd: w.restore_cmd.clone(),
                carry_forward,
            })
        }
    }

    /// `TaskJudge` when there is no `[judge]` at all (the task lane, mirroring
    /// [`Manifest::build_world`]'s `GitWorld` fallback), else the `CommandJudge`.
    pub fn build_judge(
        &self,
        workspace: PathBuf,
        frozen_injects: Vec<(PathBuf, PathBuf)>,
    ) -> Result<Box<dyn Judge + Send>> {
        let Some(judge) = self.judge.as_ref() else {
            return Ok(Box::new(TaskJudge));
        };
        Ok(Box::new(CommandJudge {
            workspace,
            measure_cmd: judge.measure_cmd.clone(),
            direction: self.direction()?,
            tiebreak_direction: self.tiebreak_direction()?,
            objective: judge.objective.clone(),
            frozen_injects,
        }))
    }
}

impl CompositeManifest {
    /// Build the multi-workspace [`CompositeWorld`]: git memory per component checkout + one combined
    /// external snapshot/restore (the assembled system under test) running in the base workspace.
    pub fn build_world(&self, manifest_dir: &Path) -> Result<Box<dyn World + Send>> {
        let components: Vec<(String, PathBuf)> = self
            .resolve_components(manifest_dir)?
            .into_iter()
            .map(|c| (c.name, c.workspace))
            .collect();
        for (_, ws) in &components {
            crucible_vcs::git_memory::install_harness_excludes(ws, &[]);
        }
        Ok(Box::new(CompositeWorld {
            components,
            base_dir: self.base_dir(manifest_dir),
            apply_cmd: self.world.apply_cmd.clone(),
            snapshot_cmd: self.world.snapshot_cmd.clone(),
            restore_cmd: self.world.restore_cmd.clone(),
            carry_forward: Vec::new(),
        }))
    }

    /// The combined gate. It runs in the base workspace (which holds every component checkout as a
    /// subdir, so the gate can assemble them); no frozen injects at this level (those stay per component).
    pub fn build_judge(&self, manifest_dir: &Path) -> Result<Box<dyn Judge + Send>> {
        Ok(Box::new(CommandJudge {
            workspace: self.base_dir(manifest_dir),
            measure_cmd: self.judge.measure_cmd.clone(),
            direction: self.direction()?,
            tiebreak_direction: self.tiebreak_direction()?,
            objective: self.judge.objective.clone(),
            frozen_injects: vec![],
        }))
    }
}
