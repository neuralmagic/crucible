import { describe, expect, it } from 'vitest';
import type { components } from '../api/schema';
import { detailPath, dispatchTone, graphRunId, originLabel, originLinks, relaunchPath } from './launchView';

type LaunchDetail = components['schemas']['PlaybookLaunchDetailDto'];

function detail(over: Partial<LaunchDetail> = {}, launch: Partial<LaunchDetail['launch']> = {}): LaunchDetail {
  return {
    launch: {
      key: 'playbook:survey:0199',
      playbook: 'survey',
      description: null,
      params: {},
      schema_digest: 'sha256:2222',
      current_schema_digest: 'sha256:2222',
      schema_drifted: false,
      max_cost: 12,
      max_time: '2h',
      advance_dedupe: false,
      origin: 'manual',
      draft_version: null,
      schedule: null,
      status: 'done',
      parked_reason: null,
      cost_usd: 8.4,
      runs: 1,
      transport_losses: 0,
      created_by: 'wren',
      created_at: '2026-08-21T09:00:00Z',
      ...launch,
    },
    source_exists: true,
    dispatch: { state: 'dispatched', failure: null, failed_at: null, failures: 0 },
    runs: [],
    ...over,
  };
}

describe('originLinks', () => {
  it('points a registered launch at the pack it pinned', () => {
    expect(originLinks(detail())).toEqual([
      { label: 'pack', value: 'survey', to: '/playbooks/survey/launch' },
    ]);
  });

  it('points a draft launch at the studio, naming the version it froze', () => {
    const links = originLinks(detail({}, { origin: 'draft', playbook: 'fences', draft_version: 4 }));
    expect(links).toEqual([{ label: 'draft', value: 'fences v4', to: '/playbooks/drafts/fences' }]);
  });

  it('drops the link when the source is gone, keeping what it named', () => {
    const links = originLinks(detail({ source_exists: false }));
    expect(links).toEqual([{ label: 'pack', value: 'survey', to: null }]);
  });

  it('names the schedule a firing came from alongside its pack', () => {
    const links = originLinks(detail({}, { origin: 'schedule', schedule: 'nightly' }));
    expect(links.map((l) => l.label)).toEqual(['pack', 'schedule']);
    expect(links[1]).toEqual({ label: 'schedule', value: 'nightly', to: null });
  });
});

describe('originLabel', () => {
  it('spells each origin for a reader and passes an unknown one through', () => {
    expect(originLabel('deferred')).toBe('one-shot');
    expect(originLabel('draft')).toBe('draft studio');
    expect(originLabel('whatever')).toBe('whatever');
  });
});

describe('dispatchTone', () => {
  it('colours a failed dispatch as the failure it is', () => {
    expect(dispatchTone('failed')).toBe('red');
    expect(dispatchTone('pending')).toBe('grey');
    expect(dispatchTone('dispatched')).toBe('blue');
  });
});

describe('graphRunId', () => {
  it('draws the newest run, and nothing when none started', () => {
    expect(graphRunId(detail())).toBeNull();
    const runs = [
      { run_id: 'playbook_survey_0199-2', status: 'running', dispatch: 'local', cluster: 'hub', pod: null, cost_usd: null },
      { run_id: 'playbook_survey_0199-1', status: 'failed', dispatch: 'local', cluster: 'hub', pod: null, cost_usd: null },
    ];
    expect(graphRunId(detail({ runs }))).toBe('playbook_survey_0199-2');
  });
});

describe('detailPath', () => {
  it('sends a launch key to its own page and everything else to the issue', () => {
    expect(detailPath('playbook:fences:01a02ffb')).toBe('/playbook-runs/playbook%3Afences%3A01a02ffb');
    expect(detailPath('owner/repo#7')).toBe('/issues/owner%2Frepo%237');
  });
});

describe('relaunchPath', () => {
  it('opens the launch form with the run key in the relaunch query', () => {
    expect(relaunchPath('fences', 'playbook:fences:01a02ffb')).toBe(
      '/playbooks/fences/launch?relaunch=playbook%3Afences%3A01a02ffb',
    );
    expect(relaunchPath('a/b', 'k&v')).toBe('/playbooks/a%2Fb/launch?relaunch=k%26v');
  });
});
