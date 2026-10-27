import { describe, expect, it } from 'vitest';
import type { components } from '../api/schema';
import {
  detailPath,
  dispatchTone,
  graphRunId,
  launchLabel,
  launchWallSecs,
  launchRunPath,
  launchVersion,
  originLabel,
  originLinks,
  relaunchPath,
  rerunLaunchBody,
  rerunRows,
  withTask,
} from './launchView';

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

describe('launchLabel', () => {
  it('names a launch by its pack and the head of its id', () => {
    expect(launchLabel('playbook:cve-triage:01a17b0f-9665-74df-9539-96935bb86827')).toBe('cve-triage 01a17b0f');
    expect(launchLabel('playbook:survey:0199')).toBe('survey 0199');
    expect(launchLabel('INFERENG-77')).toBe('INFERENG-77');
  });

  it('links a launch run under its launch', () => {
    expect(launchRunPath('playbook:survey:0199', 'RUN 1')).toBe('/playbook-runs/playbook%3Asurvey%3A0199/runs/RUN%201');
  });
});

describe('launchWallSecs', () => {
  const ev = (from: string, to: string, ts: string) => ({ from, to, ts });

  it('measures from the move into running to the move out of it', () => {
    expect(
      launchWallSecs([
        ev('new', 'new', '2026-10-27T01:53:01Z'),
        ev('new', 'running', '2026-10-27T01:53:04Z'),
        ev('running', 'running', '2026-10-27T01:53:04Z'),
        ev('running', 'done', '2026-10-27T02:03:19Z'),
      ]),
    ).toBe(615);
  });

  it('reads the trail in time order and measures a failure the same way', () => {
    expect(
      launchWallSecs([
        ev('running', 'failed', '2026-10-27T02:00:30Z'),
        ev('new', 'running', '2026-10-27T02:00:00Z'),
      ]),
    ).toBe(30);
  });

  it('is null while the launch runs, before it starts, or on a bad stamp', () => {
    expect(launchWallSecs([ev('new', 'running', '2026-10-27T02:00:00Z')])).toBeNull();
    expect(launchWallSecs([ev('new', 'new', '2026-10-27T02:00:00Z')])).toBeNull();
    expect(launchWallSecs([])).toBeNull();
    expect(launchWallSecs([ev('new', 'running', 'soon'), ev('running', 'done', 'later')])).toBeNull();
  });
});

describe('launchVersion', () => {
  it('names a draft launch by its save and a registered one by its schema digest', () => {
    expect(launchVersion({ draft_version: 126, schema_digest: 'sha256:8484c6b8284cf62cf6' })).toBe('draft v126');
    expect(launchVersion({ draft_version: null, schema_digest: 'sha256:8484c6b8284cf62cf6' })).toBe('8484c6b8284c');
    expect(launchVersion({ schema_digest: 'deadbeef' })).toBe('deadbeef');
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

describe('rerun', () => {
  it('names the run a rerun launch reruns', () => {
    const links = originLinks(detail({ rerun: { run_id: 'RUN-0412', from: ['rank'] } }));
    expect(links.at(-1)).toEqual({ label: 'rerun of', value: 'RUN-0412', to: '/runs/RUN-0412' });
  });

  it('lists what is reused, then what runs again, then what changed', () => {
    const rows = rerunRows({
      run_id: 'RUN-0412',
      playbook: 'survey',
      draft_version: 3,
      reused: [
        { task: 'scan', cost_usd: 0.25 },
        { task: 'fix', cost_usd: 1 },
      ],
      rerun: ['check'],
      reused_usd: 1.25,
      changed: ['lint'],
    });
    expect(rows).toEqual([
      { task: 'scan', state: 'reused', costUsd: 0.25 },
      { task: 'fix', state: 'reused', costUsd: 1 },
      { task: 'check', state: 'reruns', costUsd: null },
      { task: 'lint', state: 'changed', costUsd: null },
    ]);
  });

  it('keeps a selection sorted and free of repeats', () => {
    expect(withTask(['fix'], 'check')).toEqual(['check', 'fix']);
    expect(withTask(['check', 'fix'], 'fix')).toEqual(['check', 'fix']);
  });

  it('launches with the parent launch\'s own values and ceilings', () => {
    const { launch } = detail({}, { max_cost: 3, max_time: '45m' });
    expect(rerunLaunchBody(launch, 'RUN-0412', ['fix'])).toEqual({
      params: {},
      max_cost: 3,
      max_time: '45m',
      rerun: { of: 'RUN-0412', from: ['fix'] },
    });
  });
});
