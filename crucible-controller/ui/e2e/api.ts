import type { Page, Route } from '@playwright/test';

type Json = Record<string, unknown> | unknown[];

const ISSUES = [
  {
    key: 'GH-1489',
    kind: 'github',
    repo: 'vllm-project/vllm',
    cluster: 'hub',
    namespace: 'autoresearch',
    title: 'PreRequest path skips cost accounting, scorer under-reports spend',
    status: 'running',
    tier: 'T0',
    priority: 3,
    author: 'kylesayrs',
    labels: ['bug'],
    stale_closable: false,
    updated_at: '2026-08-20T09:00:00Z',
    upstream_updated_at: '2026-08-20T08:00:00Z',
    pr_url: null,
  },
  {
    key: 'SCN-0042',
    kind: 'scenario',
    repo: 'llm-d/llm-d-inference-scheduler',
    title: 'Converge P/D disaggregation rollout across NIXL generations',
    status: 'awaiting-approval',
    tier: 'T0',
    priority: 2,
    author: 'wren',
    labels: [],
    stale_closable: false,
    updated_at: '2026-08-20T08:30:00Z',
    upstream_updated_at: null,
    pr_url: null,
  },
  {
    key: 'GH-1109',
    kind: 'github',
    repo: 'neuralmagic/crucible',
    title: 'Engine-side candidate builds leak layer cache between digests',
    status: 'pr-open',
    tier: 'T1',
    priority: 1,
    author: 'robertgshaw2',
    labels: [],
    stale_closable: false,
    updated_at: '2026-08-20T06:00:00Z',
    upstream_updated_at: '2026-08-20T05:00:00Z',
    pr_url: 'https://github.com/neuralmagic/crucible/pull/1839',
  },
  {
    key: 'INF-8332',
    kind: 'jira',
    repo: 'neuralmagic/crucible',
    title: 'Team self-serve rig backend so squads can register their own clusters',
    status: 'parked',
    tier: 'T2',
    priority: 0,
    author: 'wren',
    labels: [],
    parked_by: 'wren',
    parked_reason: { text: 'waiting on the core pin to land', truncated: false },
    stale_closable: true,
    updated_at: '2026-08-19T12:00:00Z',
    upstream_updated_at: null,
    pr_url: null,
  },
];

const RUNS = [
  {
    run_id: 'RUN-0412',
    scope: 1,
    issue_key: 'GH-1489',
    repo: 'vllm-project/vllm',
    cluster: 'hub',
    namespace: 'autoresearch',
    status: 'running',
    best_score: 270,
    cost_usd: 84.2,
    score_series: [358, 341, 327, 318, 299, 288, 270],
    identity_digest: 'sha256:aaaa',
    pod: 'work-0412',
  },
  {
    run_id: 'RUN-0411',
    scope: 2,
    cluster: 'hub',
    namespace: 'autoresearch',
    issue_key: 'SCN-0042',
    repo: 'llm-d/llm-d-inference-scheduler',
    status: 'succeeded',
    best_score: 234,
    cost_usd: 122.6,
    score_series: [358, 340, 318, 295, 271, 254, 241, 234],
    identity_digest: 'sha256:bbbb',
    pod: null,
  },
  {
    run_id: 'RUN-0408',
    scope: 3,
    cluster: 'hub',
    namespace: 'autoresearch',
    issue_key: 'GH-0731',
    repo: 'deepseek-ai/DeepGEMM',
    status: 'failed',
    best_score: 398,
    cost_usd: 210.75,
    score_series: [412, 405, 398, 401, 410, 418],
    identity_digest: null,
    pod: null,
  },
];

/** A run's admitted work graph: a fanned-out task whose instances land in the results without ever
 * being declared, one of them failed, plus a task nothing has reported on yet. */
const RUN_GRAPH = {
  plan_version: 3,
  tasks: [
    { name: 'read', kind: 'agent', depends_on: [], session: 'survey', needs: 'any', required: true },
    { name: 'summarize', kind: 'command', depends_on: ['read'], session: '', needs: 'all', required: true },
    { name: 'rank', kind: 'top_k', depends_on: ['summarize'], session: '', needs: 'all', required: true },
    { name: 'file', kind: 'command', depends_on: ['rank'], session: '', needs: 'all', required: true },
  ],
  results: [
    { iter: 0, task: 'read', status: 'fail', note: 'the harness dropped the turn', cost_usd: 0.4, secs: 31 },
    { iter: 1, task: 'read', status: 'pass', note: 'read 14 papers', cost_usd: 1.1, secs: 240 },
    { iter: 1, task: 'summarize[paged-attention]', status: 'pass', note: 'one entry per citation', cost_usd: 0.2, secs: 18 },
    { iter: 1, task: 'summarize[flashinfer]', status: 'fail', note: 'exit 1: no citations parsed\n  at summarize.sh:14', cost_usd: 0.05, secs: 6 },
    { iter: 1, task: 'rank', status: 'pass', note: 'kept 1 of 2', cost_usd: null, secs: 2 },
  ],
};


/** The triage run this machine actually ran, as the endpoints answer for it: the plan it admitted,
 * what each task reported, and the evidence `triage[1027]` left behind — its payload and the
 * TRIAGE.md it captured, verbatim from the run's own state directory. */
export const TRIAGE_RUN = 'playbook_triage-local_01a030fe-2592-7902-a02c-3e3b8d9738d7-1787528357';

const TRIAGE_GRAPH = {
  plan_version: 1,
  tasks: [
    { name: 'scan', kind: 'agent', depends_on: [], session: '', needs: 'any', required: true },
    { name: 'triage', kind: 'agent', depends_on: ['scan'], session: '', needs: 'any', required: false },
    { name: 'roundup', kind: 'command', depends_on: ['scan', 'triage'], session: '', needs: 'any', required: true },
  ],
  results: [
    { iter: 0, task: 'scan', status: 'pass', note: '', cost_usd: 0.2659475, secs: 0 },
    { iter: 0, task: 'triage[1027]', status: 'pass', note: '', cost_usd: 0.2297655, secs: 0 },
    { iter: 0, task: 'triage[952]', status: 'pass', note: '', cost_usd: 0.30397949999999996, secs: 0 },
    { iter: 0, task: 'roundup', status: 'pass', note: '', cost_usd: 0, secs: 0 },
  ],
};

const TRIAGE_MD = [
  '# 1027: [RFC]: Checkpoint and restore RNG state so a resumed run reproduces the uninterrupted run',
  '',
  '## Classification',
  'feature, severity low, confidence high',
  '',
  '## Evidence',
  'The title is tagged `[RFC]` and the body is structured as a proposal for a new capability.',
].join('\n');

const TRIAGE_EVIDENCE = {
  run_id: TRIAGE_RUN,
  task: 'triage[1027]',
  status: 'pass',
  iter: 0,
  note: '',
  attempts: 1,
  cost_usd: 0.2297655,
  secs: 0,
  payload: { classification: 'feature', confidence: 'high', severity: 'low' },
  files: [{ name: 'TRIAGE.md', size_bytes: 1379, content: TRIAGE_MD }],
  running: false,
};

/** A task the run has not reported on yet: evidence at rest, honestly empty. */
const SCAN_EVIDENCE = {
  run_id: TRIAGE_RUN,
  task: 'scan',
  status: null,
  iter: null,
  note: null,
  attempts: null,
  cost_usd: null,
  secs: null,
  payload: null,
  files: [],
  running: true,
};

const TRIAGE_LOG = {
  run_id: TRIAGE_RUN,
  dispatch: 'local',
  text: 'plan run: admitted 3 tasks\nplan run: triage[1027] pass\n',
  truncated: false,
  location: null,
};

const RUN_DETAIL = {
  run: RUNS[0],
  candidates: [],
};

/** The compiled plan the preview surfaces draw: one of every card the graph has to say
 * something about, plus a task named with markup so the injection case has a target. */
/** The pack a draft opens on in the studio. */
const DRAFT_FILES = {
  'crucible.toml': '[repo]\npath = "."\n\n[workflow]\ntype = "playbook"\nfile = "workflow.star"\n',
  'workflow.star':
    'params = {}\nread = agent(name = "read", prompt = "READ THE PAPER")\nworkflow(type = "playbook", tasks = [read], result = read)\n',
  'skills/read/SKILL.md': 'Read the paper and report one entry per citation.\n',
};

const PREVIEW_SCHEMA = {
  type: 'object',
  properties: {
    topic: { type: 'string', pattern: '^[a-z ]+$', description: 'What the survey covers.' },
    depth: { type: 'string', default: 'deep', description: 'How far to chase citations.' },
  },
  required: ['topic'],
  additionalProperties: false,
};

const PREVIEW_GRAPH = {
  workflow_type: 'playbook',
  result: 'file',
  nodes: [
    { name: 'read', kind: 'agent', required: true, needs: 'any', join: 'all', isolation: 'worktree', emits: ['paper'], emits_files: [], fanout: null, session: 'survey', harness: 'claude', model: 'opus', effort: 'high', prompt: 'READ THE PAPER\n\nReport one entry per citation.\n', command: null },
    { name: 'summarize', kind: 'command', required: true, needs: 'any', join: 'all', isolation: null, emits: [], emits_files: [], fanout: { over_task: 'read', over_field: 'paper', max_fanout: 3 }, session: null, harness: null, model: null, effort: null, prompt: null, command: './summarize.sh --one' },
    { name: 'lint', kind: 'command', required: false, needs: 'any', join: 'all', isolation: null, emits: [], emits_files: [], fanout: null, session: null, harness: null, model: null, effort: null, prompt: null, command: './lint.sh' },
    { name: '<img src=x onerror="alert(1)">', kind: 'engine', required: true, needs: 'any', join: 'all', isolation: null, emits: [], emits_files: [], fanout: null, session: null, harness: null, model: null, effort: null, prompt: null, command: null },
    { name: 'file', kind: 'command', required: true, needs: 'all', join: 'passed', isolation: null, emits: [], emits_files: ['spec.md'], fanout: null, session: null, harness: null, model: null, effort: null, prompt: null, command: './file.sh' },
  ],
  edges: [
    { from: 'read', to: 'summarize', join: 'all', required: true },
    { from: 'read', to: 'lint', join: 'all', required: false },
    { from: 'summarize', to: '<img src=x onerror="alert(1)">', join: 'all', required: true },
    { from: '<img src=x onerror="alert(1)">', to: 'file', join: 'passed', required: true },
    { from: 'lint', to: 'file', join: 'passed', required: true },
  ],
};

/** The pending import a proposal lands, and what its own URL serves back. */
export const IMPORT_ID = '0199c0de-7a11-71ec-9a1b-3f0f5b6a1c22';

/// A pack this deployment cannot dispatch: the laptop runs playbooks locally, the pack wants an
/// OpenShell sandbox.
const UNDISPATCHABLE = {
  backend: 'openshell',
  sandbox_image: 'ghcr.io/example/sandbox:latest',
  dispatchable: false,
  refusal:
    'the pack declares [agent] backend "openshell", which needs an OpenShell sandbox on a cluster; this deployment runs playbooks as a local subprocess (CONTROLLER_PLAYBOOK_EXECUTOR=local)',
  local_mode: true,
  harness: null,
  requires: {},
  prefers: {},
  allow_unverified_image: false,
  resources: { gpus: 0, cpu: null, memory: null, node_selector: {} },
  image: {
    reference: 'ghcr.io/example/sandbox:latest',
    digest: null,
    capability_digest: null,
    tags: [],
    checked: true,
    catalogued: false,
    verified: false,
    overridden: false,
    unsatisfied: [],
    refusals: [
      'sandbox image ghcr.io/example/sandbox:latest is not in the image catalog; pick a catalogued image, or set [agent] allow_unverified_image = true to launch it unmatched',
    ],
    warnings: [],
  },
};

const DISPATCHABLE = {
  backend: 'local',
  sandbox_image: null,
  dispatchable: true,
  refusal: null,
  local_mode: true,
  harness: null,
  requires: {},
  prefers: {},
  allow_unverified_image: false,
  resources: { gpus: 0, cpu: null, memory: null, node_selector: {} },
  image: {
    reference: null,
    digest: null,
    capability_digest: null,
    tags: [],
    checked: false,
    catalogued: false,
    verified: false,
    overridden: false,
    unsatisfied: [],
    refusals: [],
    warnings: [],
  },
};

/// The image catalog as the studio's picker sees it: one exact fit on the promoted channel, one
/// image the pack's requirement excludes, one the catalog cannot vouch for.
const GO_CC_IMAGE = {
  name: 'sandbox-go-cc',
  verified: true,
  repository: 'ghcr.io/acme/sandbox-go-cc',
  digest: 'sha256:1111111111111111111111111111111111111111111111111111111111111111',
  tags: ['latest', 'fd2438cd09d2ac0bd3d467e8ad25d511f06f9a4e'],
  arches: ['amd64', 'arm64'],
  created_at: '2026-09-12T06:40:00Z',
  capabilities: {
    features: ['base', 'go', 'claude-code'],
    image: 'sandbox-go-cc',
    predicates: { 'toolchain.go': '1.25.11', 'agent.claude-code': '2.1.270' },
    schema: 'io.crucible.capabilities/v1',
  },
  capability_digest: 'sha256:cap1',
  intro_digest: null,
  first_seen: '2026-09-12T07:00:00Z',
  last_seen: '2026-09-12T07:00:00Z',
};
const RUST_CC_IMAGE = {
  ...GO_CC_IMAGE,
  name: 'sandbox-rust-cc',
  repository: 'ghcr.io/acme/sandbox-rust-cc',
  digest: 'sha256:2222222222222222222222222222222222222222222222222222222222222222',
  capabilities: {
    features: ['base', 'rust', 'claude-code'],
    image: 'sandbox-rust-cc',
    predicates: { 'toolchain.rust': '1.90.0', 'agent.claude-code': '2.1.270' },
    schema: 'io.crucible.capabilities/v1',
  },
};
const CUSTOM_IMAGE = {
  ...GO_CC_IMAGE,
  name: 'custom-sandbox',
  verified: false,
  repository: 'ghcr.io/acme/custom-sandbox',
  digest: 'sha256:3333333333333333333333333333333333333333333333333333333333333333',
  tags: ['latest'],
  capabilities: null,
  capability_digest: null,
};
const RANKED_IMAGES = {
  compatible: [{ image: GO_CC_IMAGE, surplus: 0, preferred: 0, default: true }],
  excluded: [
    {
      image: RUST_CC_IMAGE,
      unsatisfied: [{ predicate: 'toolchain.go', required: '>=1.25', found: null }],
    },
  ],
  unverified: [CUSTOM_IMAGE],
};

export const PACK_IMPORT = {
  id: IMPORT_ID,
  repo: 'neuralmagic/other-packs',
  git_ref: null,
  path: 'packs/survey',
  rev: '4d5e6f708192a3b4c5d6e7f80912a3b4c5d6e7f8',
  tar_digest: 'sha256:5555',
  params_schema: PREVIEW_SCHEMA,
  schema_digest: 'sha256:3333',
  graph: PREVIEW_GRAPH,
  diagnostics: [],
  dispatch: UNDISPATCHABLE,
  secrets: {
    declared: [
      { name: 'pr_token', kind: 'opaque', projection_kind: 'env', projection: 'AUTORESEARCH_PR_TOKEN' },
      { name: 'registry', kind: 'registry_authfile', projection_kind: 'file', projection: '/etc/quay/push.json' },
    ],
    warnings: [],
  },
  exposure: { document: null, digest: 'sha256:expo', approved_digest: null, lines: ['outputs:', '  draft-pr x1 -> owner/repo'] },
  exposure_digest: 'sha256:expo',
  core_rev: '7c2c1a5',
  status: 'pending',
  playbook: null,
  draft_id: null,
  proposed_by: 'authoring-agent',
  created_at: '2026-08-23T09:00:00Z',
  resolved_by: null,
  resolved_at: null,
};

/** One ad-hoc launch, shared by the runs rail's list and the launch view's by-key read. */
export const PLAYBOOK_RUN = {
  key: 'playbook:survey:0199',
  playbook: 'survey',
  description: 'Survey a topic across the tracked repos and file what it finds.',
  params: { topic: 'attention kernels', depth: 'deep' },
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
  secrets_refusal: null,
  cost_usd: 8.4,
  runs: 1,
  created_by: 'wren',
  created_at: '2026-08-21T09:00:00Z',
};

const TEAMS = [
  {
    slug: 'llm-d',
    display_name: 'LLM-D',
    members: [
      { kind: 'user', member: 'wren', role: 'owner', since: '2026-09-13T00:00:00Z', added_by: 'wren' },
      { kind: 'group', member: '/groups/platform', role: 'maintainer', since: '2026-09-13T00:00:00Z', added_by: 'wren' },
      { kind: 'team', member: 'core', role: 'member', since: '2026-09-13T00:00:00Z', added_by: 'wren' },
      { kind: 'rule', member: 'email-domain:example.com', role: 'member', since: '2026-09-13T00:00:00Z', added_by: null },
    ],
    my_role: 'owner',
    reachable: true,
    created_at: '2026-09-13T00:00:00Z',
    created_by: 'wren',
    updated_at: '2026-09-13T00:00:00Z',
  },
  {
    slug: 'platform-administrators',
    display_name: 'Platform administrators',
    members: [{ kind: 'user', member: 'wren', role: 'owner', since: '2026-09-13T00:00:00Z', added_by: null }],
    my_role: 'owner',
    reachable: true,
    created_at: '2026-09-13T00:00:00Z',
    created_by: null,
    updated_at: '2026-09-13T00:00:00Z',
  },
];

export const ROUTES: Record<string, Json> = {
  '/api/issues': ISSUES,
  '/api/issues/facets': {
    total: 4,
    kind: [
      { value: 'github', count: 2 },
      { value: 'jira', count: 1 },
      { value: 'scenario', count: 1 },
    ],
    status: [
      { value: 'awaiting-approval', count: 1 },
      { value: 'parked', count: 1 },
      { value: 'pr-open', count: 1 },
      { value: 'running', count: 1 },
    ],
    tier: [
      { value: 'T0', count: 2 },
      { value: 'T1', count: 1 },
      { value: 'T2', count: 1 },
    ],
    affinity: [
      { value: 'perf', count: 2 },
      { value: 'perf-adjacent', count: 1 },
    ],
    repo: [
      { value: 'llm-d/llm-d-inference-scheduler', count: 4 },
      { value: 'neuralmagic/crucible', count: 13 },
      { value: 'vllm-project/vllm', count: 9 },
      { value: 'deepseek-ai/DeepGEMM', count: 3 },
      { value: 'neuralmagic/crucible', count: 2 },
      { value: 'neuralmagic/epp-mcp', count: 2 },
      { value: 'llm-d/llm-d', count: 1 },
      { value: 'vllm-project/vllm-ascend', count: 1 },
    ],
  },
  // The provider picker reads the registry on every page that can launch; an empty one hides it.
  '/api/config/providers': { providers: [], defaults: [] },
  '/api/runs': RUNS,
  '/api/runs/RUN-0412': RUN_DETAIL,
  '/api/runs/RUN-0412/iterations': [],
  '/api/runs/RUN-0412/graph': RUN_GRAPH,
  '/api/runs/RUN-0412/files': {
    files: [
      { task: 'summarize', instance: 'paged-attention', path: 'SUMMARY.md', size_bytes: 412, key: 'summarize[paged-attention]/SUMMARY.md' },
      { task: 'summarize', instance: 'flashinfer', path: 'SUMMARY.md', size_bytes: 96, key: 'summarize[flashinfer]/SUMMARY.md' },
      { task: 'rank', instance: null, path: 'RANKING.md', size_bytes: 210, key: 'rank/RANKING.md' },
    ],
  },
  '/api/runs/RUN-0412/files/summarize[paged-attention]/SUMMARY.md': '# paged attention\n\nOne entry per citation.',
  '/api/runs/RUN-0412/files/summarize[flashinfer]/SUMMARY.md': '# flashinfer\n\nno citations parsed',
  '/api/runs/RUN-0412/files/rank/RANKING.md': '# ranking\n\nkept 1 of 2',
  '/api/runs/RUN-0412/log': { run_id: 'RUN-0412', dispatch: 'pod', text: null, truncated: false, location: 'pod work-0412 in namespace autoresearch' },
  '/api/runs/RUN-0412/tasks/summarize[flashinfer]/evidence': {
    run_id: 'RUN-0412',
    task: 'summarize[flashinfer]',
    status: 'fail',
    iter: 1,
    note: 'exit 1: no citations parsed\n  at summarize.sh:14',
    attempts: 1,
    cost_usd: 0.05,
    secs: 6,
    payload: null,
    files: [],
    running: false,
  },
  [`/api/runs/${TRIAGE_RUN}`]: { run: { ...RUNS[1], run_id: TRIAGE_RUN, issue_key: 'playbook:triage-local:01a030fe', cost_usd: 1.38 }, candidates: [] },
  [`/api/runs/${TRIAGE_RUN}/iterations`]: [],
  [`/api/runs/${TRIAGE_RUN}/graph`]: TRIAGE_GRAPH,
  [`/api/runs/${TRIAGE_RUN}/log`]: TRIAGE_LOG,
  [`/api/runs/${TRIAGE_RUN}/tasks/triage[1027]/evidence`]: TRIAGE_EVIDENCE,
  [`/api/runs/${TRIAGE_RUN}/tasks/scan/evidence`]: SCAN_EVIDENCE,
  '/api/repos': [
    { repo: 'vllm-project/vllm', paused: false, new: 2, scoped: 1, awaiting_approval: 1, running: 1, pr_open: 0, parked: 0, done: 4 },
    { repo: 'neuralmagic/crucible', paused: false, new: 1, scoped: 0, awaiting_approval: 0, running: 0, pr_open: 1, parked: 1, done: 9 },
  ],
  '/api/funnel': {
    stages: [
      { key: 'new', label: 'New', count: 3, href_hint: '/issues?status=new' },
      { key: 'scoped', label: 'Scoped', count: 2, href_hint: '/issues?status=scoped' },
      { key: 'awaiting_approval', label: 'Awaiting', count: 1, href_hint: '/approvals' },
      { key: 'running', label: 'Running', count: 1, href_hint: '/runs' },
      { key: 'pr_open', label: 'PR open', count: 1, href_hint: '/issues?status=pr-open' },
      { key: 'done', label: 'Done', count: 9, href_hint: '/issues?status=done' },
    ],
  },
  '/api/ledger/by-tag': {
    day: '2026-08-20',
    ceiling: 1500,
    tags: [
      { tag: 'agent.propose', total_usd: 71.4 },
      { tag: 'agent.measure', total_usd: 28.9 },
      { tag: 'build', total_usd: 14.2 },
    ],
  },
  '/api/ledger/summary': {
    days: [
      { day: '2026-08-18', total_usd: 288.1 },
      { day: '2026-08-19', total_usd: 401.5 },
      { day: '2026-08-20', total_usd: 412.8 },
    ],
  },
  '/api/playbooks': [
    {
      id: 'survey',
      description: 'Survey a topic across the tracked repos and file what it finds.',
      source: { kind: 'git', repo: 'neuralmagic/crucible-packs', git_ref: null, path: 'packs/survey' },
      rev: '9f2c1a4c0b3d5e6f7a8b9c0d1e2f3a4b5c6d7e8f',
      tar_digest: 'sha256:1111',
      schema_digest: 'sha256:2222',
      core_rev: '7c2c1a5',
      dispatch: DISPATCHABLE,
      owner: 'team:llm-d',
      actions: ['read', 'create', 'update', 'delete', 'transfer', 'share', 'launch'],
      created_by: 'wren',
      created_at: '2026-08-20T07:00:00Z',
      updated_at: '2026-08-20T07:00:00Z',
    },
    {
      id: 'triage',
      description: 'Triage the inbox and park what cannot move.',
      source: { kind: 'git', repo: 'neuralmagic/crucible-packs', git_ref: null, path: 'packs/triage' },
      rev: '0a1b2c3d4e5f60718293a4b5c6d7e8f901234567',
      tar_digest: 'sha256:3333',
      schema_digest: 'sha256:4444',
      core_rev: '7c2c1a5',
      dispatch: DISPATCHABLE,
      owner: 'user:wren',
      actions: ['read', 'create', 'update', 'delete', 'transfer', 'share', 'launch'],
      created_by: 'wren',
      created_at: '2026-08-21T07:00:00Z',
      updated_at: '2026-08-21T07:00:00Z',
    },
  ],
  '/api/playbooks/survey/schema': {
    type: 'object',
    properties: {
      topic: { type: 'string', pattern: '^[a-z ]+$', description: 'What the survey covers.' },
      depth: { type: 'string', default: 'shallow', description: 'How far to chase citations.' },
    },
    required: ['topic'],
    additionalProperties: false,
  },
  '/api/playbooks/import/candidates': {
    rev: '4d5e6f708192a3b4c5d6e7f80912a3b4c5d6e7f8',
    candidates: [
      { path: 'packs/survey', workflow_file: 'workflow.star' },
      { path: 'packs/audit', workflow_file: 'graph.star' },
    ],
  },
  [`/api/playbooks/imports/${IMPORT_ID}/compile`]: {
    params_schema: PREVIEW_SCHEMA,
    schema_digest: 'sha256:3333',
    graph: PREVIEW_GRAPH,
    diagnostics: [],
    dispatch: UNDISPATCHABLE,
    exposure: { document: null, digest: 'sha256:expo', approved_digest: null, lines: [] },
  },
  '/api/images': {
    images: [GO_CC_IMAGE, RUST_CC_IMAGE, CUSTOM_IMAGE],
    repositories: [
      { repository: 'ghcr.io/acme/sandbox-*', last_polled: '2026-09-12T07:00:00Z', last_ok: '2026-09-12T07:00:00Z', last_error: null },
    ],
  },
  '/api/playbook-drafts': [
    {
      id: 'studio',
      description: 'A pack being authored in the controller.',
      origin: null,
      graduation_repo: null,
      graduation_path: null,
      graduation_pr_url: null,
      published_playbook: null,
      retired_at: null,
      latest_version: 1,
      compiles: true,
      diagnostics: 0,
      owner: 'user:wren',
      actions: ['read', 'create', 'update', 'delete', 'transfer', 'share', 'launch', 'approve'],
      created_by: 'wren',
      created_at: '2026-08-22T09:00:00Z',
      updated_at: '2026-08-22T09:00:00Z',
    },
  ],
  '/api/playbook-drafts/studio': {
    id: 'studio',
    description: 'A pack being authored in the controller.',
    origin: null,
    graduation_repo: null,
    graduation_path: null,
    graduation_pr_url: null,
    published_playbook: null,
    retired_at: null,
    latest_version: 1,
    compiles: true,
    diagnostics: 0,
    actions: ['read', 'create', 'update', 'delete', 'transfer', 'share', 'launch', 'approve'],
    created_by: 'wren',
    created_at: '2026-08-22T09:00:00Z',
    updated_at: '2026-08-22T09:00:00Z',
    versions: [
      {
        version: 1,
        tar_digest: 'sha256:4444',
        schema_digest: 'sha256:3333',
        diagnostics: 0,
        core_rev: '7c2c1a5',
        created_by: 'wren',
        created_at: '2026-08-22T09:00:00Z',
      },
    ],
  },
  '/api/playbook-drafts/studio/files': {
    version: 1,
    saved_by: 'wren',
    saved_at: '2026-08-22T09:00:00Z',
    diagnostics: [],
    files: DRAFT_FILES,
  },
  '/api/playbook-drafts/studio/preview': {
    version: 1,
    saved_by: 'wren',
    saved_at: '2026-08-22T09:00:00Z',
    params_schema: PREVIEW_SCHEMA,
    schema_digest: 'sha256:3333',
    graph: PREVIEW_GRAPH,
    diagnostics: [],
    dispatch: DISPATCHABLE,
  },
  '/api/playbooks/drafts/co-draft': {
    url: 'https://crucible.example.com',
    skill_url: 'https://crucible.example.com/api/playbooks/drafts/skill',
    steps: [
      {
        label: 'Install the skill',
        commands: [
          'mkdir -p ~/.claude/skills/crucible-co-draft',
          'mv ~/Downloads/crucible-co-draft-SKILL.md ~/.claude/skills/crucible-co-draft/SKILL.md',
        ],
      },
      {
        label: 'Mint an API key at https://crucible.example.com/settings and keep it in your shell',
        commands: ['export CONTROLLER_API_TOKEN=crk_...'],
      },
      {
        label: 'Point your agent at this controller',
        commands: [
          'claude mcp add --transport http crucible https://crucible.example.com/mcp \\\n  --header "Authorization: Bearer $CONTROLLER_API_TOKEN"',
        ],
      },
    ],
    example_prompt: 'Use the crucible-co-draft skill: author a playbook pack that sweeps our repos.',
  },
  '/api/config/playbook-caps': { max_cost: 25, max_time: '4h' },
  '/api/schedules': [
    {
      id: 'sch-0001',
      playbook: 'survey',
      params: { repo: 'vllm-project/vllm' },
      schema_digest: 'sha256:1f0c',
      max_cost: 12,
      max_time: '2h',
      advance_dedupe: true,
      cursor: null,
      cursor_value: null,
      cursor_updated_at: null,
      cron_expr: '0 6 * * MON-FRI',
      tz: 'UTC',
      enabled: true,
      next_due_at: '2026-08-25T06:00:00Z',
      last_fired_at: '2026-08-24T06:00:00Z',
      consecutive_failures: 0,
      created_by: 'wren',
      owner_principal: 'wren',
      owner_groups_at: '2026-08-24T06:00:00Z',
      owner_signin_required: false,
      owner_refresh_error: null,
      owner_refresh_at: '2026-08-24T06:00:00Z',
      created_at: '2026-08-01T09:00:00Z',
      updated_at: '2026-08-24T06:00:00Z',
    },
    {
      id: 'sch-0002',
      playbook: 'triage',
      params: {},
      schema_digest: 'sha256:44ab',
      max_cost: 5,
      max_time: '1h',
      advance_dedupe: false,
      cursor: null,
      cursor_value: null,
      cursor_updated_at: null,
      cron_expr: '*/30 * * * *',
      tz: 'America/New_York',
      enabled: true,
      next_due_at: '2026-08-24T12:30:00Z',
      last_fired_at: '2026-08-24T12:00:00Z',
      consecutive_failures: 0,
      created_by: 'wren',
      owner_principal: 'kylesayrs',
      owner_groups_at: '2026-08-24T11:00:00Z',
      owner_signin_required: false,
      owner_refresh_error: 'the identity provider could not be reached',
      owner_refresh_at: '2026-08-24T12:00:00Z',
      created_at: '2026-08-02T09:00:00Z',
      updated_at: '2026-08-24T12:00:00Z',
    },
    {
      id: 'sch-0003',
      playbook: 'nightly-rank',
      params: {},
      schema_digest: 'sha256:90de',
      max_cost: 25,
      max_time: '4h',
      advance_dedupe: true,
      cursor: null,
      cursor_value: null,
      cursor_updated_at: null,
      cron_expr: '0 2 * * *',
      tz: 'UTC',
      enabled: false,
      next_due_at: null,
      last_fired_at: '2026-08-23T02:00:00Z',
      consecutive_failures: 3,
      created_by: 'robertgshaw2',
      owner_principal: 'robertgshaw2',
      owner_groups_at: '2026-08-20T02:00:00Z',
      owner_signin_required: true,
      owner_refresh_error: 'the refresh was refused: invalid_grant',
      owner_refresh_at: '2026-08-24T02:00:00Z',
      created_at: '2026-08-03T09:00:00Z',
      updated_at: '2026-08-24T02:00:00Z',
    },
  ],
  '/api/schedules/preview': {
    cron_expr: '0 6 * * MON-FRI',
    tz: 'UTC',
    firings: ['2026-08-24T06:00:00Z', '2026-08-25T06:00:00Z', '2026-08-26T06:00:00Z'],
  },
  '/api/playbook-runs/playbook:survey:0199': {
    launch: PLAYBOOK_RUN,
    source_exists: true,
    dispatch: {
      state: 'dispatched',
      failure: 'dispatch_run needs a deploy profile: set CONTROLLER_DEPLOY_PROFILE',
      failed_at: '2026-08-21T09:00:10Z',
      failures: 1,
    },
    runs: [
      { run_id: 'RUN-0412', status: 'running', dispatch: 'local', pod: null, cost_usd: 8.4 },
    ],
  },
  '/api/playbook-runs/playbook:survey:0197': {
    launch: {
      ...PLAYBOOK_RUN,
      key: 'playbook:survey:0197',
      status: 'parked',
      cost_usd: null,
      runs: 0,
      parked_reason:
        'secrets: the pack declares secret pr_token, and repo neuralmagic/crucible has no binding for it',
      secrets_refusal:
        'the pack declares secret pr_token, and repo neuralmagic/crucible has no binding for it',
    },
    source_exists: true,
    dispatch: { state: 'pending', failure: null, failed_at: null, failures: 0 },
    runs: [],
  },
  '/api/playbook-runs/playbook:survey:0198': {
    launch: { ...PLAYBOOK_RUN, key: 'playbook:survey:0198', status: 'parked', cost_usd: null, runs: 0 },
    source_exists: true,
    dispatch: {
      state: 'failed',
      failure: 'dispatch_run needs a deploy profile: set CONTROLLER_DEPLOY_PROFILE',
      failed_at: '2026-08-21T09:00:10Z',
      failures: 3,
    },
    runs: [],
  },
  '/api/turns': [],
  '/api/whoami': {
    user: 'wren',
    admin: true,
    role: 'admin',
    groups: ['/groups/platform'],
    mode: 'native',
    downgraded: false,
    proves_groups: true,
    teams: [
      { team: 'llm-d', role: 'maintainer', via: [{ kind: 'group', group: '/groups/platform', role: 'maintainer' }] },
      { team: 'platform-administrators', role: 'owner', via: [{ kind: 'rule', rule: 'configured-admins', role: 'owner' }] },
    ],
  },
  '/api/teams': TEAMS,
  '/api/teams/llm-d': TEAMS[0],
  '/api/teams/platform-administrators': TEAMS[1],
  '/api/playbook-drafts/studio/shares': [
    {
      grantee: 'team:core',
      role: 'launcher',
      not_after: null,
      expired: false,
      created_by: 'user:wren',
      created_at: '2026-09-13T00:00:00Z',
      updated_at: '2026-09-13T00:00:00Z',
    },
    {
      grantee: 'user:kylesayrs',
      role: 'viewer',
      not_after: '2026-01-01T00:00:00Z',
      expired: true,
      created_by: 'user:wren',
      created_at: '2025-12-01T00:00:00Z',
      updated_at: '2025-12-01T00:00:00Z',
    },
  ],
  '/api/secrets': [
    {
      id: '0199-secret-a',
      name: 'pr_token',
      owner: 'user:wren',
      kind: 'opaque',
      visibility: 'broker_only',
      consumer: 'run',
      mode: 'managed',
      vault_path: 'user:wren/pr_token',
      current_version: 2,
      created_by: 'wren',
      created_at: '2026-08-23T09:00:00Z',
      updated_at: '2026-08-24T09:00:00Z',
    },
    {
      id: '0199-secret-b',
      name: 'registry',
      owner: 'group:/groups/platform',
      kind: 'registry_authfile',
      visibility: 'broker_only',
      consumer: 'run',
      mode: 'reference',
      vault_path: 'vault://kv/platform/quay#authfile',
      current_version: null,
      created_by: 'someone-else',
      created_at: '2026-08-22T09:00:00Z',
      updated_at: '2026-08-22T09:00:00Z',
    },
  ],
  '/api/autopilot': { enabled: true, changed_at: '2026-08-20T07:00:00Z', changed_by: 'wren', reason: null },
  '/api/approvals': {
    awaiting_approval: [],
    pending_imports: [
      {
        id: IMPORT_ID,
        repo: 'neuralmagic/other-packs',
        path: 'packs/survey',
        rev: '4d5e6f708192a3b4c5d6e7f80912a3b4c5d6e7f8',
        proposed_by: 'authoring-agent',
        created_at: '2026-08-23T09:00:00Z',
        compiles: true,
        diagnostics: 0,
        draft_id: null,
      },
    ],
    kept_prs: [],
  },
  '/api/overview': {
    statuses: [
      { status: 'new', count: 3 },
      { status: 'running', count: 1 },
      { status: 'pr-open', count: 1 },
    ],
    tiers: [
      { tier: 'T0', count: 2 },
      { tier: 'T1', count: 1 },
      { tier: 'T2', count: 1 },
    ],
    running: { current: 1, cap: 3 },
    scopes_today: { current: 2, cap: 10 },
    cost_today: { current: 412.8, ceiling: 1500 },
  },
};

function fallback(path: string): Json {
  return path.endsWith('s') ? [] : {};
}

/** Serve every API call from fixtures so a render is a function of the code alone. A proposal is
 * the one stateful stub: the row it lands is what its own URL serves back, so a shared link and a
 * refresh read what was proposed. */
export async function stubApi(page: Page): Promise<void> {
  let proposed: Record<string, unknown> = { ...PACK_IMPORT };
  let shares = [...(ROUTES['/api/playbook-drafts/studio/shares'] as Record<string, unknown>[])];
  let team = { ...TEAMS[0] };
  await page.route(
    (url: URL) => url.pathname.startsWith('/api/'),
    (route: Route) => {
      // Decoded: a key like `playbook:survey:0199` reaches the wire percent-encoded, and the
      // fixture map is keyed by the path the endpoint answers on.
      const path = decodeURIComponent(new URL(route.request().url()).pathname);

      if (path === '/api/events') {
        return route.fulfill({ status: 200, contentType: 'text/event-stream', body: '' });
      }
      // Shares on the studio draft and the llm-d member list are the two stateful stubs the
      // editors need: what is granted, changed, or removed is what the next read serves.
      const share = /^\/api\/playbook-drafts\/studio\/shares\/(.+)$/.exec(path);
      if (share !== null && route.request().method() === 'PUT') {
        const sent: unknown = JSON.parse(route.request().postData() ?? '{}');
        const body = typeof sent === 'object' && sent !== null ? (sent as Record<string, unknown>) : {};
        const row = {
          grantee: share[1],
          role: body.role ?? 'viewer',
          not_after: body.not_after ?? null,
          expired: false,
          created_by: 'user:wren',
          created_at: '2026-09-14T00:00:00Z',
          updated_at: '2026-09-14T00:00:00Z',
        };
        const existed = shares.some((s) => s.grantee === row.grantee);
        shares = [...shares.filter((s) => s.grantee !== row.grantee), row];
        return route.fulfill({ status: existed ? 200 : 201, contentType: 'application/json', body: JSON.stringify(row) });
      }
      if (share !== null && route.request().method() === 'DELETE') {
        shares = shares.filter((s) => s.grantee !== share[1]);
        return route.fulfill({ status: 204, body: '' });
      }
      if (path === '/api/playbook-drafts/studio/shares') {
        return route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify(shares) });
      }
      if (path === '/api/teams/llm-d/members' && route.request().method() === 'PUT') {
        const sent: unknown = JSON.parse(route.request().postData() ?? '{}');
        const body = typeof sent === 'object' && sent !== null ? (sent as Record<string, unknown>) : {};
        const members = Array.isArray(body.members) ? (body.members as Record<string, unknown>[]) : [];
        team = {
          ...team,
          members: members.map((m) => ({ ...m, since: '2026-09-14T00:00:00Z', added_by: 'wren' })),
        };
        return route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify(team) });
      }
      if (path === '/api/teams/llm-d') {
        return route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify(team) });
      }
      if (path === '/api/images/rank' && route.request().method() === 'POST') {
        return route.fulfill({
          status: 200,
          contentType: 'application/json',
          body: JSON.stringify(RANKED_IMAGES),
        });
      }
      if (path === '/api/playbooks/imports' && route.request().method() === 'POST') {
        const sent: unknown = JSON.parse(route.request().postData() ?? '{}');
        const body = typeof sent === 'object' && sent !== null ? (sent as Record<string, unknown>) : {};
        proposed = { ...PACK_IMPORT, repo: body.repo ?? PACK_IMPORT.repo, path: body.path ?? '' };
        return route.fulfill({
          status: 201,
          contentType: 'application/json',
          body: JSON.stringify(proposed),
        });
      }
      if (path === `/api/playbooks/imports/${IMPORT_ID}/compile`) {
        // A re-compile of the same frozen bytes: the graph and the diagnostics can move, the form,
        // the substrate and the declared credentials cannot.
        const row = proposed;
        return route.fulfill({
          status: 200,
          contentType: 'application/json',
          body: JSON.stringify({
            params_schema: row.params_schema,
            schema_digest: row.schema_digest,
            graph: row.graph,
            diagnostics: row.diagnostics,
            dispatch: row.dispatch,
            secrets: row.secrets,
            exposure: row.exposure,
          }),
        });
      }
      if (path === `/api/playbooks/imports/${IMPORT_ID}`) {
        return route.fulfill({
          status: 200,
          contentType: 'application/json',
          body: JSON.stringify(proposed),
        });
      }
      const body = ROUTES[path] ?? fallback(path);
      return route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify(body) });
    },
  );
}
