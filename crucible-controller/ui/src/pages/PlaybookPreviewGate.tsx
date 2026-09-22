import { useMemo } from 'react';
import { Empty, Mono, Section, SectionBody, SectionHeader } from '../ui';
import { Note, Notice } from './formControls';
import { WorkflowGraph } from './WorkflowGraph';
import { PlaybookParamFields } from './PlaybookParamFields';
import { PackDispatchNotice } from './PackDispatchNotice';
import { parseParamsSchema, type ParamFieldSpec } from './playbookLaunchForm';
import { declaredLabel, type PackSecretsDto } from './secretsView';
import type { WorkflowGraphDoc } from './workflowGraphLayout';
import type { components } from '../api/schema';

type PackDispatchDto = components['schemas']['PackDispatchDto'];

export interface PlaybookPreviewGateProps {
  /// The engine's params schema; null when the source did not compile.
  schema: unknown;
  /// The compiled plan's graph; null when there is none.
  graph: WorkflowGraphDoc | null;
  /// The engine's diagnostics, rendered verbatim.
  diagnostics: readonly string[];
  /// What this pack's agent needs, against what this deployment can dispatch.
  dispatch: PackDispatchDto;
  /// The credentials the pack's manifest declares, and the deploy-profile names they collide with.
  secrets: PackSecretsDto;
  /// The values the graph was compiled with. Editing one re-compiles: a pack with required params
  /// draws no graph until they are supplied.
  values: Readonly<Record<string, string>>;
  onValueChange: (name: string, value: string) => void;
  /// Fired when an input loses focus, so the caller can re-compile once per edit rather than once
  /// per keystroke.
  onValuesSettled: () => void;
}

const NO_ERRORS: ReadonlyMap<string, string> = new Map();

/// What a pack looks like before it is registered: the launch form its schema renders, the graph
/// its source compiles to, and whatever the engine said. Fetch-free and driven by props, so the
/// import wizard and any other pre-registration flow show the same gate.
export function PlaybookPreviewGate({
  schema,
  graph,
  diagnostics,
  dispatch,
  secrets,
  values,
  onValueChange,
  onValuesSettled,
}: PlaybookPreviewGateProps) {
  const parsed = useMemo(
    () => (schema === null || schema === undefined ? null : parseParamsSchema(schema)),
    [schema]
  );
  const specs: ParamFieldSpec[] = parsed !== null && parsed.kind === 'form' ? parsed.specs : [];

  return (
    <>
      <Section>
        <SectionHeader title="Launch form" />
        {parsed === null ? (
          <SectionBody>
            <Empty
              title="NO FORM"
              description="The engine extracted no schema from this pack; its diagnostics are below."
            />
          </SectionBody>
        ) : parsed.kind === 'unrenderable' ? (
          <SectionBody>
            <Empty title="FORM CANNOT BE RENDERED" description={parsed.reason} />
          </SectionBody>
        ) : specs.length === 0 ? (
          <SectionBody>
            <Empty title="NO PARAMETERS" description="This pack declares none." />
          </SectionBody>
        ) : (
          <>
            <Notice label="Preview only">
              These values compile the graph below. They are not saved: registration stores the pack
              and its form, never a set of values.
            </Notice>
            <SectionBody>
              <PlaybookParamFields
                idPrefix="preview"
                specs={specs}
                values={values}
                errors={NO_ERRORS}
                onChange={onValueChange}
                onBlur={onValuesSettled}
              />
            </SectionBody>
          </>
        )}
      </Section>

      <PackDispatchNotice dispatch={dispatch} />

      <Section>
        <SectionHeader
          title="Secrets"
          actions={
            <Mono size="data" tone="ink-3">
              {secrets.declared.length === 0 ? 'none declared' : `${secrets.declared.length}`}
            </Mono>
          }
        />
        {secrets.warnings.length > 0 && (
          <Notice label="Also in the profile">
            {secrets.warnings.join(' ')}
          </Notice>
        )}
        <SectionBody>
          {secrets.declared.length === 0 ? (
            <Note>
              This pack declares no credentials, so no scope has to bind anything before it
              launches.
            </Note>
          ) : (
            <div className="grid gap-2 font-mono text-data text-ink-2">
              {secrets.declared.map((declared) => (
                <div key={declared.name}>{declaredLabel(declared)}</div>
              ))}
            </div>
          )}
        </SectionBody>
        {secrets.declared.length > 0 && (
          <SectionBody>
            <Note>
              A launch is refused, naming the secret, until the scope it runs under binds every one
              of these. Bind them on the Secrets page.
            </Note>
          </SectionBody>
        )}
      </Section>

      <Section>
        <SectionHeader title="Graph" />
        <SectionBody>
          {graph === null ? (
            <Empty
              title="NO GRAPH"
              description="The pack compiled no plan at these values. The engine's reason is below."
            />
          ) : (
            <WorkflowGraph graph={graph} />
          )}
        </SectionBody>
      </Section>

      <Section>
        <SectionHeader
          title="Diagnostics"
          actions={
            <Mono size="data" tone="ink-3">
              {diagnostics.length === 0 ? 'clean' : `${diagnostics.length}`}
            </Mono>
          }
        />
        <SectionBody>
          {diagnostics.length === 0 ? (
            <Note>The engine compiled this pack without complaint.</Note>
          ) : (
            <div className="grid gap-2">
              {diagnostics.map((diagnostic) => (
                <pre
                  key={diagnostic}
                  className="m-0 overflow-x-auto border border-red bg-surface px-3 py-2 font-mono text-data whitespace-pre-wrap text-red"
                >
                  {diagnostic}
                </pre>
              ))}
            </div>
          )}
        </SectionBody>
      </Section>
    </>
  );
}
