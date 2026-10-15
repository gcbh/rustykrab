// Exercise the shipped dreaming renderer, including older-server observations.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import vm from 'node:vm';
import test from 'node:test';

const source = readFileSync(new URL('../crates/rustykrab-gateway/static/monitor.js', import.meta.url), 'utf8');
new vm.Script(source);
const helpers = source.slice(source.indexOf('  const human ='), source.indexOf('  async function api('));
const renderer = source.slice(source.indexOf('  function renderDreamingAdmission('), source.indexOf('  function renderResources('));
assert(renderer.startsWith('  function renderDreamingAdmission('));

class Element {
  constructor(tag) { this.tag = tag; this.children = []; this.value = ''; this.disabled = false; }
  set textContent(value) { this.value = String(value); this.children = []; }
  get textContent() { return this.value + this.children.map(child => child.textContent).join('\n'); }
  append(...children) { this.children.push(...children); }
  replaceChildren(...children) { this.value = ''; this.children = children; }
  addEventListener() {}
  set innerHTML(_) { throw new Error('Observations must render as literal text'); }
}

function dashboard() {
  const elements = new Map();
  const $ = id => {
    if (!elements.has(id)) elements.set(id, new Element('div'));
    return elements.get(id);
  };
  const context = {
    $, document: { createElement: tag => new Element(tag) },
    Date: class extends Date { static now() { return Date.parse('2026-10-15T03:00:00Z'); } },
    fetch() { throw new Error('Rendering must not request or execute work'); }
  };
  vm.createContext(context);
  vm.runInContext(source.slice(source.indexOf('  const node ='), source.indexOf('  const human =')) + helpers + renderer, context);
  return { render: d => context.renderDreaming(d), text: () => $('dreamMetrics').textContent, $ };
}

function view(admission) {
  return {
    enabled: true, interval_seconds: 3600, last_evaluation: null, last_analysis: null,
    outcome_records: 0, reviews: [], admission,
    metrics: { completed: 0, calibrated_reviews: 0, pending: 0, failed: 0,
      reviewed_projects: 0, eligible_projects: 1, proposal_count: 0,
      pending_decisions: 0, acceptance_rate: null, improvement_rate: null,
      measured_outcomes: 0, tokens: 0, wall_seconds: 0, warnings: [] }
  };
}
const observed = decision => ({ decision, checked_at: '2026-10-15T02:59:55Z',
  observed_at: '2026-10-15T02:59:50Z', active_foreground_runs: 0,
  dispatchable_foreground_leaves: 0, waiting: {} });

test('a blocked backlog exposes the bounded review opportunity and exact known counts', () => {
  const t = dashboard();
  t.render(view({ ...observed('incident_review'), waiting: { 'blocked/capability': 80, dependencies: 19 } }));
  assert.match(t.text(), /waiting-only backlog permits a bounded incident review/);
  assert.match(t.text(), /Active foreground runs: 0 · Runnable foreground tasks: 0/);
  assert.match(t.text(), /blocked \/ capability: 80 · dependencies: 19/);
  assert.match(t.text(), /Decision check: 5s ago · Work-demand observation: 10s ago/);
  assert.equal(t.$('dreamNow').disabled, false);
  assert.doesNotMatch(t.text(), /review is running|healthy/i);
});

test('active and runnable work explain deferral without claiming a new scheduler decision', () => {
  const t = dashboard();
  t.render(view({ ...observed('foreground_active'), active_foreground_runs: 2 }));
  assert.match(t.text(), /Review deferred: foreground work is active/);
  assert.match(t.text(), /Active foreground runs: 2/);
  t.render(view({ ...observed('foreground_runnable'), dispatchable_foreground_leaves: 3 }));
  assert.match(t.text(), /Review deferred: foreground work can be dispatched/);
  assert.match(t.text(), /Runnable foreground tasks: 3/);
  assert.doesNotMatch(t.text(), /foreground work is active/);
  assert.equal(t.$('dreamNow').disabled, false);
});

test('unknown, failed, timed-out and stale demand are visible and never become idle', () => {
  for (const [decision, expected] of [
    ['controller_unavailable', /work manager is stopping, failing, or waiting for exclusive control/],
    ['demand_unknown', /current work demand has not been observed/],
    ['demand_failed', /could not read current work demand/],
    ['demand_timed_out', /exceeded its deadline/],
    ['demand_stale', /stale or ahead of the server clock/]
  ]) {
    const t = dashboard();
    t.render(view({ decision, checked_at: null, observed_at: null,
      active_foreground_runs: null, dispatchable_foreground_leaves: null, waiting: null }));
    assert.match(t.text(), expected);
    assert.match(t.text(), /Active foreground runs: not observed · Runnable foreground tasks: not observed/);
    assert.match(t.text(), /Work-demand observation: not observed/);
    assert.match(t.text(), /Waiting work: not observed/);
    assert.doesNotMatch(t.text(), /may start|Active foreground runs: 0|none in this observation/);
  }
});

test('older and future daemon responses remain unknown rather than implying capacity', () => {
  const t = dashboard();
  t.render(view(undefined));
  assert.match(t.text(), /Review availability has not been reported by this build/);
  assert.doesNotMatch(t.text(), /Active foreground runs: 0|may start/);
  for (const decision of ['future_decision', 'toString', '__proto__']) {
    t.render(view({ decision }));
    assert.match(t.text(), /Review availability has not been reported by this build/);
    assert.doesNotMatch(t.text(), /function|may start/);
  }
});

test('daily limits, due state and in-progress reviews stay distinct from permission to start', () => {
  const t = dashboard();
  for (const [decision, expected] of [
    ['daily_limit', /review-cycle limit for the last 24 hours/],
    ['no_due_project', /No enabled project is due/],
    ['review_in_progress', /review cycle is already in progress/],
    ['idle_review', /An idle project review may start/],
    ['disabled', /no projects are enabled for review/]
  ]) {
    t.render(view(observed(decision)));
    assert.match(t.text(), expected);
    assert.match(t.text(), /Review requests still follow work-capacity and daily review limits/);
  }
  t.render({ ...view(observed('disabled')), enabled: false });
  assert.equal(t.$('dreamNow').disabled, true);
});

test('the server owns current admission wording and the numeric cycle limit', () => {
  const t = dashboard();
  t.render({ ...view(observed('daily_limit')), daily_review_limit: 7,
    admission_explanation: 'Server policy: seven cycles used; <inspect receipt>' });
  assert.match(t.text(), /Last decision: Server policy: seven cycles used; <inspect receipt>/);
  assert.match(t.text(), /Review-cycle limit per 24 hours: 7/);
  assert.doesNotMatch(t.text(), /three-cycle|review-cycle limit for the last 24 hours has been reached/);
  assert.equal(t.$('dreamNow').disabled, false);
  t.render({ ...view({ decision: 'future_policy' }), daily_review_limit: 9,
    admission_explanation: 'A future server supplied this decision' });
  assert.match(t.text(), /Last decision: A future server supplied this decision/);
  assert.match(t.text(), /Review-cycle limit per 24 hours: 9/);
});

test('missing, malformed and orphaned display observations use legacy or unknown values', () => {
  const t = dashboard();
  for (const invalid of [undefined, null, '', '  ', 0, {}]) {
    t.render({ ...view(observed('daily_limit')), admission_explanation: invalid,
      daily_review_limit: invalid });
    assert.match(t.text(), /Last decision: Review deferred: the review-cycle limit/);
    assert.match(t.text(), /Review-cycle limit per 24 hours: not observed/);
  }
  for (const limit of [-1, 0, 1.5, Number.MAX_SAFE_INTEGER + 1, '3']) {
    t.render({ ...view(observed('daily_limit')), daily_review_limit: limit });
    assert.match(t.text(), /Review-cycle limit per 24 hours: not observed/);
  }
  t.render({ ...view(undefined), admission_explanation: 'stale orphaned explanation' });
  assert.doesNotMatch(t.text(), /stale orphaned explanation/);
  assert.match(t.text(), /Review availability has not been reported/);
});

test('latest project-review receipts are separate from general expectation and outcome passes', () => {
  const t = dashboard();
  t.render({ ...view(observed('idle_review')), last_evaluation: '2026-10-15T02:59:00Z',
    last_project_review_completed_at: '2026-10-14T22:00:00Z',
    last_project_review_failed_at: '2026-10-15T01:00:00Z' });
  assert.match(t.text(), /Last completed project review: /);
  assert.match(t.text(), /Last failed project review: /);
  assert(t.text().includes('Last completed project review: ' + new Date('2026-10-14T22:00:00Z').toLocaleString()));
  assert(t.text().includes('Last failed project review: ' + new Date('2026-10-15T01:00:00Z').toLocaleString()));
  t.render(view(observed('idle_review')));
  assert.match(t.text(), /Last completed project review: Not observed · Last failed project review: Not observed/);
  assert.match(t.$('dreamStatus').textContent, /Expectation pass not observed/);
});

test('refresh and disconnect-like missing snapshots remove previous admission evidence', () => {
  const t = dashboard();
  t.render({ ...view({ ...observed('incident_review'), waiting: { marker_from_old_snapshot: 80 } }),
    admission_explanation: 'old server explanation', daily_review_limit: 7 });
  assert.match(t.text(), /marker from old snapshot/);
  assert.match(t.text(), /old server explanation/);
  t.render(view(undefined));
  assert.doesNotMatch(t.text(), /marker from old snapshot|bounded incident review|old server explanation|per 24 hours: 7/);
  assert.match(t.text(), /Review-cycle limit per 24 hours: not observed/);
  t.render(null);
  assert.equal(t.text(), '');
  assert.equal(t.$('dreamNow').disabled, true);
  assert.equal(t.$('dreamStatus').textContent, 'Dreaming status unavailable');
});

test('waiting labels stay literal text and missing or invalid counts do not turn into zero', () => {
  const t = dashboard();
  t.render(view({ ...observed('incident_review'), active_foreground_runs: null,
    dispatchable_foreground_leaves: -1,
    waiting: { '<img src=x onerror=run()>': 2, missing: null, invalid: -1 } }));
  assert.match(t.text(), /<img src=x onerror=run\(\)>: 2/);
  assert.match(t.text(), /missing: not observed · invalid: not observed/);
  assert.doesNotMatch(t.text(), /Active foreground runs: 0|Runnable foreground tasks: 0/);
});
