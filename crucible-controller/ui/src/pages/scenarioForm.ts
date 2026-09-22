import type { components } from '../api/schema.d';
import { agentPickFields, type AgentPick } from './agentPick';

type AdoptScenarioBody = components['schemas']['AdoptScenarioBody'];

/// The raw text the New Scenario form holds, before trimming.
export interface ScenarioFormFields {
  title: string;
  body: string;
  affectedRepos: string[];
  authoritative: boolean;
  gitRef: string;
  /// The selected broker codegen contract NAME, or '' for the local-measure default.
  codegenContract: string;
  justification: string;
  /// The chosen dispatch target name, or '' for the controller's configured default.
  dispatchTarget: string;
  /// The picked inference provider and model, both blank for the configured defaults.
  agent: AgentPick;
}

/// Build the `POST /api/scenarios` body from the form's raw fields. `git_ref` and
/// `codegen_contract` are omitted (not sent as empty strings) when blank: the server rejects a
/// present-but-blank value for either, since blank there means "the caller meant to name one and
/// didn't", not "default branch" / "local measure".
export function scenarioAdoptBody(f: ScenarioFormFields): AdoptScenarioBody {
  const gitRef = f.gitRef.trim();
  const codegenContract = f.codegenContract.trim();
  const dispatchTarget = f.dispatchTarget.trim();
  return {
    title: f.title.trim(),
    body: f.body.trim(),
    affected_repos: f.affectedRepos.map((r) => r.trim()).filter((r) => r.length > 0),
    authoritative: f.authoritative,
    justification: f.justification.trim(),
    ...(gitRef.length > 0 ? { git_ref: gitRef } : {}),
    ...(codegenContract.length > 0 ? { codegen_contract: codegenContract } : {}),
    ...(dispatchTarget.length > 0 ? { dispatch_target: dispatchTarget } : {}),
    ...agentPickFields(f.agent),
  };
}
