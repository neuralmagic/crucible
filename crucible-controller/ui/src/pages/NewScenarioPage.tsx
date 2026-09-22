import { useState } from 'react';
import { Link, useNavigate } from 'react-router-dom';
import { $api } from '../api/client';
import { formatError } from '../api/errors';
import {
  Breadcrumb,
  Button,
  Empty,
  LoadingBlock,
  PageHeader,
  Section,
  SectionBody,
  SectionHeader,
} from '../ui';
import {
  CheckField,
  FormActions,
  FormError,
  FormGrid,
  Notice,
  RepoRows,
  SelectField,
  TextAreaField,
  TextField,
  useRepoRows,
} from './formControls';
import { scenarioAdoptBody } from './scenarioForm';
import { DispatchTargetField } from './DispatchTargetField';
import { ProviderModelField } from './ProviderModelField';
import { NO_AGENT_PICK, type AgentPick } from './agentPick';

const CRUMBS = [{ label: 'Issues', to: '/issues' }, { label: 'New scenario' }];

/// The scenario adopt form: describe a problem directly, with no upstream GitHub issue behind it.
/// Affected repos are a best guess, not a binding target — the first entry is the repo actually
/// cloned, the rest are just extra context the scoping step may revise. Submits straight to
/// `POST /api/scenarios`; the row lands at `new` and skips the ranker's usual gates entirely (the
/// human adoption is itself the tier/priority authorization).
export function NewScenarioPage() {
  const navigate = useNavigate();
  const whoami = $api.useQuery('get', '/api/whoami');
  const adoptMutation = $api.useMutation('post', '/api/scenarios');
  // Deploy-pinned, so it never changes under an open form. An empty list (or a failed fetch) leaves
  // the select with only the local-measure option, which is the pre-broker behaviour.
  const contracts = $api.useQuery('get', '/api/config/broker-contracts');
  const contractNames = contracts.data?.names ?? [];

  const [title, setTitle] = useState('');
  const [body, setBody] = useState('');
  const { repos: affectedRepos, trimmed: trimmedRepos, onChangeAt, onAdd, onRemove } = useRepoRows();
  const [authoritative, setAuthoritative] = useState(false);
  const [gitRef, setGitRef] = useState('');
  const [codegenContract, setCodegenContract] = useState('');
  const [justification, setJustification] = useState('');
  const [dispatchTarget, setDispatchTarget] = useState('');
  const [dispatchChoiceRequired, setDispatchChoiceRequired] = useState(false);
  const [agent, setAgent] = useState<AgentPick>(NO_AGENT_PICK);
  const [submitError, setSubmitError] = useState<string | null>(null);

  if (whoami.isPending) {
    return <LoadingBlock />;
  }

  const isAdmin = whoami.data?.role === 'admin';

  if (!isAdmin) {
    return (
      <>
        <Breadcrumb items={CRUMBS} />
        <Empty
          title="ADMIN ACCESS REQUIRED"
          description="Adopting a scenario books to your login, so it needs an admin session."
        />
      </>
    );
  }

  const isValid =
    title.trim().length > 0 &&
    body.trim().length > 0 &&
    trimmedRepos.length > 0 &&
    affectedRepos[0].trim().length > 0 &&
    justification.trim().length >= 10 &&
    (!dispatchChoiceRequired || dispatchTarget !== '');

  const handleSubmit = async () => {
    setSubmitError(null);
    try {
      const ack = await adoptMutation.mutateAsync({
        body: scenarioAdoptBody({
          title,
          body,
          affectedRepos,
          authoritative,
          gitRef,
          codegenContract,
          justification,
          dispatchTarget,
          agent,
        }),
      });
      void navigate(`/issues/${encodeURIComponent(ack.key)}`);
    } catch (err: unknown) {
      setSubmitError(formatError(err));
    }
  };

  return (
    <>
      <Breadcrumb items={CRUMBS} />
      <PageHeader
        eyebrow="Queue"
        title="Adopt a scenario"
        description="Describe a problem directly, with no upstream issue behind it. The first affected repo is where work starts."
      />
      <Notice label="Bypass">
        This creates a new work item with no GitHub issue behind it. It skips the usual
        prioritization checks. Starting it here is the approval, recorded in the audit log under
        your account ({whoami.data?.user || 'unknown'}).
      </Notice>

      <Section>
        <SectionHeader title="Scenario" />
        <SectionBody>
          <FormGrid>
            <TextField
              id="scenario-title"
              label="Title"
              required
              value={title}
              onChange={setTitle}
              placeholder="One-line summary of the problem"
              autoFocus
            />

            <TextAreaField
              id="scenario-body"
              label="Body"
              required
              value={body}
              onChange={setBody}
              rows={8}
              placeholder="Describe the problem and what success looks like"
              hint={
                authoritative
                  ? 'This brief reaches the scoping agent verbatim — keep its prescriptions (files, approach, measurement) intact.'
                  : "Describe the problem and what success looks like. Don't prescribe the fix — the scoping agent designs the approach."
              }
            />

            <CheckField
              id="scenario-authoritative"
              label="Authoritative brief (inject verbatim, skip de-prescription)"
              checked={authoritative}
              onChange={setAuthoritative}
              description="For a plan derived from measurement, whose prescriptions have to survive into the run. The frozen gate is what keeps it honest."
            />

            <RepoRows
              idPrefix="scenario"
              repos={affectedRepos}
              onChangeAt={onChangeAt}
              onAdd={onAdd}
              onRemove={onRemove}
            />

            <TextField
              id="scenario-git-ref"
              label="Git ref"
              value={gitRef}
              onChange={setGitRef}
              placeholder="nv_dev"
              mono
              hint="Branch or tag. Blank is the repo's default branch."
            />

            <SelectField
              id="scenario-codegen-contract"
              label="Codegen contract"
              value={codegenContract}
              onChange={setCodegenContract}
              options={[
                { value: '', label: 'none (measure on the loop pod)' },
                ...contractNames.map((name) => ({ value: name, label: name })),
              ]}
              hint="Measure on GPUs through the broker using this named tool contract. Leave unset to let the pack measure locally."
            />

            <DispatchTargetField
              id="scenario-dispatch-target"
              value={dispatchTarget}
              onChange={setDispatchTarget}
              onChoiceRequired={setDispatchChoiceRequired}
            />

            <ProviderModelField
              idPrefix="scenario"
              workloadClass="autoresearch"
              value={agent}
              onChange={setAgent}
            />

            <TextAreaField
              id="scenario-justification"
              label="Justification"
              required
              value={justification}
              onChange={setJustification}
              rows={3}
              placeholder="Why this is worth running now"
              hint="Why this is worth running (recorded in the audit log). At least 10 characters."
            />

            {submitError !== null && <FormError>{submitError}</FormError>}
          </FormGrid>
        </SectionBody>
      </Section>

      <FormActions>
        <Button
          variant="filled"
          onClick={() => void handleSubmit()}
          disabled={!isValid || adoptMutation.isPending}
        >
          {adoptMutation.isPending ? 'ADOPTING…' : 'ADOPT SCENARIO'}
        </Button>
        <Button render={<Link to="/issues" />}>CANCEL</Button>
      </FormActions>
    </>
  );
}
