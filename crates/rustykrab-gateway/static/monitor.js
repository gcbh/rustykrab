/* Agent monitor. Only API headers carry authentication; model text is rendered as text. */
'use strict';
(() => {
  const $ = id => document.getElementById(id);
  const closed = new Set(['done', 'failed', 'cancelled', 'expired']);
  const access = window.RustyKrabAccess;
  let snapshot = null, pending = false, paused = false, interval = null, generation = 0;
  let lastQuestions = '', detailRequest = 0;
  const linkedWork = new URLSearchParams(window.location.search).get('work');
  const node = (tag, text, cls) => {
    const e = document.createElement(tag);
    if (text !== undefined) e.textContent = text;
    if (cls) e.className = cls;
    return e;
  };
  const human = s => String(s || 'unknown').replaceAll('_', ' ');
  const num = n => Number(n || 0).toLocaleString();
  const status = i => typeof i.status === 'string' ? i.status : (i.status?.status || 'unknown');
  const reason = i => typeof i.status === 'object' ? i.status?.reason : null;
  const at = t => t ? new Date(t).toLocaleString() : 'Not observed';
  const age = t => {
    if (!t) return 'not observed';
    const seconds = Math.max(0, Math.floor((Date.now() - Date.parse(t)) / 1000));
    if (!Number.isFinite(seconds)) return 'not observed';
    if (seconds < 60) return seconds + 's ago';
    if (seconds < 3600) return Math.floor(seconds / 60) + 'm ago';
    if (seconds < 86400) return Math.floor(seconds / 3600) + 'h ago';
    return Math.floor(seconds / 86400) + 'd ago';
  };
  const badge = (text, kind) => node('span', human(text), 'badge ' + (kind || text));
  const itemButton = (id, text) => {
    const b = node('button', text);
    b.type = 'button';
    b.addEventListener('click', () => showDetail(id));
    return b;
  };
  async function api(path, body) {
    const response = await fetch(path, {
      method: body === undefined ? 'GET' : 'POST',
      headers: { ...access.headers(), ...(body === undefined ? {} : { 'Content-Type': 'application/json' }) },
      body: body === undefined ? undefined : JSON.stringify(body),
      cache: 'no-store',
      signal: AbortSignal.timeout(12000)
    });
    if (response.status === 401) {
      disconnect('Access has changed. Check Tailscale and reconnect.');
      throw new Error('Authentication required.');
    }
    let data;
    try { data = JSON.parse(await response.text()); } catch (_) {
      if (!response.ok) throw new Error('Access rejected by the gateway (' + response.status + '). Open the monitor at your daemon address.');
      throw new Error('The daemon returned an unreadable response.');
    }
    if (!response.ok) throw new Error(data.message || 'The daemon could not complete this request (' + response.status + ').');
    return data;
  }
  function disconnect(message = '') {
    generation++; detailRequest++; pending = false;
    clearInterval(interval); interval = null;
    access.disconnect(); snapshot = null; lastQuestions = '';
    $('accessStatus').textContent = ''; $('accessMessage').textContent = access.message({disconnected: true});
    $('dashboard').hidden = true; $('logout').hidden = true; $('login').hidden = false;
    $('loginError').textContent = message; $('token').value = '';
    $('pairCode').value = ''; $('pairError').textContent = '';
    if ($('detail').open) $('detail').close();
    $('detailBody').replaceChildren(); $('workRows').replaceChildren(); $('agents').replaceChildren();
    $('serviceRows').replaceChildren(); $('scheduleRows').replaceChildren(); $('briefingRows').replaceChildren();
    $('dreamRows').replaceChildren(); $('dreamMetrics').replaceChildren(); $('projectRows').replaceChildren(); $('events').replaceChildren(); $('quality').replaceChildren(); $('questionRows').replaceChildren();
  }
  async function connect(automatic = false, operation = () => access.connect(automatic)) {
    $('accessConnect').disabled = true; $('loginError').textContent = '';
    try {
      const result = await operation();
      $('accessMessage').textContent = access.message(result);
      if (!result.authenticated) {
        if (!result.tailscale_enabled && !result.disconnected) $('manualAccess').open = true;
        return;
      }
      $('accessStatus').textContent = access.message(result);
      generation++; pending = false; lastQuestions = ''; paused = false;
      $('pause').textContent = 'Pause updates'; $('pause').setAttribute('aria-pressed', 'false');
      clearInterval(interval);
      await refresh();
      if (snapshot) {
        $('token').value = ''; $('pairCode').value = '';
        interval = setInterval(() => { if (!paused && !document.hidden) refresh(); }, 5000);
        if (linkedWork) await showDetail(linkedWork);
      }
    } catch (error) { $('loginError').textContent = error.message; }
    finally { $('accessConnect').disabled = false; }
  }
  function stat(value, title, caption) {
    const card = node('div', undefined, 'card stat');
    card.append(node('div', num(value), 'number'), node('p', title), node('p', caption, 'caption'));
    return card;
  }
  function renderDreaming(d) {
    $('dreamRows').replaceChildren(); $('dreamMetrics').replaceChildren();
    $('dreamNow').disabled = !d?.enabled;
    $('dreamStatus').textContent = d ? (d.enabled ? 'Project reviews enabled · Interval ' + num(d.interval_seconds / 3600) + ' hours' : 'Project reviews are not configured') + ' · Expectation pass ' + age(d.last_evaluation) + ' · Outcome analysis ' + age(d.last_analysis) : 'Dreaming status unavailable';
    if (!d) return;
    const m = d.metrics;
    $('dreamMetrics').append(node('p', m.completed + ' completed reviews (' + m.calibrated_reviews + ' calibrated) · ' + m.pending + ' in progress · ' + m.failed + ' failed · ' + m.reviewed_projects + '/' + m.eligible_projects + ' projects reviewed this interval'));
    $('dreamMetrics').append(node('p', m.proposal_count + ' proposals · ' + m.pending_decisions + ' awaiting review · Acceptance ' + (m.acceptance_rate === null ? 'not yet measured' : Math.round(m.acceptance_rate * 100) + '%') + ' · Improvement ' + (m.improvement_rate === null ? 'not yet measured' : Math.round(m.improvement_rate * 100) + '%') + ' (' + m.measured_outcomes + ' measured outcomes)'));
    $('dreamMetrics').append(node('p', num(m.tokens) + ' recorded tokens · ' + num(m.wall_seconds) + ' run seconds · ' + num(d.outcome_records) + ' captured turn outcomes', 'note'));
    for (const warning of m.warnings) $('dreamMetrics').append(node('p', warning, 'note'));
    for (const r of d.reviews) {
      const card = node('article', undefined, 'agent');
      card.append(badge(r.stage), node('p', 'Project ' + r.project_id + ' · Revision ' + r.revision.slice(0, 12) + ' · ' + at(r.created_at)));
      if (r.generator_item) card.append(itemButton(r.generator_item, 'Generator run'));
      if (r.evaluator_item) card.append(itemButton(r.evaluator_item, 'Meta-evaluator run'));
      if (r.error) card.append(node('p', r.error, 'error'));
      if (r.generated?.abstention) card.append(node('p', 'Abstained: ' + r.generated.abstention));
      for (const [i, idea] of (r.generated?.ideas || []).entries()) {
        card.append(node('h3', idea.title), node('p', idea.change), node('p', 'Experiment: ' + idea.experiment));
        const a = r.meta?.assessments.find(a => a.index === i);
        if (a) card.append(node('p', 'Reviewer judgment /4: evidence ' + a.evidence + ' · usefulness ' + a.usefulness + ' · novelty ' + a.novelty + ' · testability ' + a.testability), node('p', a.reason));
      }
      if (r.meta) {
        card.append(node('p', 'Calibration: ' + r.meta.calibration.length + '/3 negative controls passed. This checks basic grading behavior; real utility remains separate.', 'note'));
        card.append(node('p', 'Coverage: ' + r.meta.coverage));
        for (const text of r.meta.blind_spots) card.append(node('p', 'Blind spot: ' + text));
        for (const text of r.meta.improvements) card.append(node('p', 'Suggested evaluation improvement: ' + text));
      }
      for (const text of r.skipped) card.append(node('p', text, 'note'));
      for (const id of r.filed) card.append(itemButton(id, 'Review proposal'));
      const receipt = node('button', 'Frozen input and full receipt');
      receipt.addEventListener('click', async () => {
        const g = generation, request = ++detailRequest;
        try { const data = await api('/api/dreaming/reviews/' + encodeURIComponent(r.id)); if (g !== generation || request !== detailRequest) return; $('detailTitle').textContent = 'Dreaming receipt'; $('detailBody').replaceChildren(node('pre', JSON.stringify(data, null, 2))); if (!$('detail').open) $('detail').showModal(); }
        catch (e) { if (g === generation && request === detailRequest) { $('error').hidden = false; $('error').textContent = e.message; } }
      }); card.append(receipt); $('dreamRows').append(card);
    }
  }
  function renderResources(reply) {
    $('managerMode').textContent = reply.work_manager ? 'v6 manages conversational assignments and scheduled work through the durable controller.' : 'Work-manager mode is disabled on this instance.';
    $('serviceRows').replaceChildren();
    if (!reply.services?.length) $('serviceRows').append(node('p', 'No constituent services registered.', 'note'));
    for (const s of reply.services || []) {
      const card = node('article', undefined, 'agent');
      card.append(node('h3', s.id), badge(s.healthy === true && s.supervised ? 'healthy' : s.healthy === false ? 'degraded' : 'unknown'), node('p', s.role + ' · ' + (s.version || 'Version unavailable')), node('p', s.detail + ' · Checked ' + age(s.checked_at)), node('p', 'Supervisor ' + s.supervisor + (s.process_id ? ' · PID ' + s.process_id : '') + (s.ensure_running ? ' · Automatic recovery enabled' : ' · Manual recovery')));
      for (const [action, title] of [['ensure_running', 'Ensure running'], ['restart', 'Restart']]) {
        const b = node('button', title); b.type = 'button'; b.disabled = !s.lifecycle_safe;
        b.addEventListener('click', async () => {
          const g = generation; b.disabled = true;
          try { const receipt = await api('/api/resources/' + encodeURIComponent(s.id) + '/actions', {action}); if (g !== generation) return; card.append(node('p', 'Service action queued; follow its work item for verification.'), itemButton(receipt.root, 'Track action')); }
          catch (e) { if (g === generation) { $('error').hidden = false; $('error').textContent = e.message; } }
          finally { if (g === generation) b.disabled = false; }
        }); card.append(b);
      }
      $('serviceRows').append(card);
    }
    $('scheduleRows').replaceChildren();
    if (!reply.schedules?.length) $('scheduleRows').append(node('p', 'No schedules in this manager.', 'note'));
    for (const job of reply.schedules || []) {
      const card = node('article', undefined, 'agent');
      card.append(node('h3', job.task.length > 120 ? job.task.slice(0, 120) + '…' : job.task), badge(job.enabled ? 'scheduled' : 'paused'), node('p', job.schedule + ' · ' + job.timezone + ' · Next ' + at(job.next_run_at)), node('p', 'Execution ' + human(job.execution?.worker_kind || 'any') + (job.execution?.required_tools?.length ? ' · Tools ' + job.execution.required_tools.join(', ') : '') + (job.execution?.writable_resources?.length ? ' · Resources ' + job.execution.writable_resources.join(', ') : '')));
      const delivery = reply.scheduled_deliveries?.find(d => d.job_id === job.id);
      if (delivery) card.append(node('p', 'Last delivery: ' + delivery.status + ' · ' + at(delivery.at), 'note'));
      const history = node('button', 'Run history');
      history.addEventListener('click', async () => {
        const g = generation, request = ++detailRequest;
        try { const d = await api('/api/schedules/' + encodeURIComponent(job.id)); if (g !== generation || request !== detailRequest) return; $('detailTitle').textContent = 'Schedule history'; $('detailBody').replaceChildren(node('pre', JSON.stringify(d, null, 2))); if (d.work_item_id) $('detailBody').prepend(itemButton(d.work_item_id, 'Latest work item')); if (!$('detail').open) $('detail').showModal(); }
        catch (e) { if (g === generation && request === detailRequest) { $('error').hidden = false; $('error').textContent = e.message; } }
      }); card.append(history);
      const toggle = node('button', job.enabled ? 'Pause schedule' : 'Resume schedule');
      toggle.disabled = !reply.work_manager;
      toggle.addEventListener('click', async () => {
        const g = generation; toggle.disabled = true;
        try { await api('/api/schedules/' + encodeURIComponent(job.id) + '/enabled', {enabled: !job.enabled}); if (g === generation) await refresh(); }
        catch (e) { if (g === generation) { $('error').hidden = false; $('error').textContent = e.message; } }
        finally { if (g === generation) toggle.disabled = false; }
      }); card.append(toggle); $('scheduleRows').append(card);
    }
  }
  async function renderBriefings(g) {
    try {
      const d = await api('/api/briefings'); if (g !== generation) return;
      $('briefingRows').replaceChildren();
      if (!d.configured) $('briefingRows').append(node('p', 'Managed vault is not configured.', 'note'));
      else if (!d.notes.length) $('briefingRows').append(node('p', 'No saved briefings yet.', 'note'));
      for (const n of d.notes) {
        const button = node('button', n.date + ' · ' + num(n.bytes) + ' bytes');
        button.addEventListener('click', async () => {
          const request = ++detailRequest;
          try { const note = await api('/api/briefings/' + encodeURIComponent(n.date)); if (g !== generation || request !== detailRequest) return; $('detailTitle').textContent = note.path; $('detailBody').replaceChildren(node('p', 'SHA-256 ' + note.sha256, 'note'), node('pre', note.content)); if (!$('detail').open) $('detail').showModal(); }
          catch (e) { if (g === generation) { $('error').hidden = false; $('error').textContent = e.message; } }
        }); $('briefingRows').append(button);
      }
    } catch (e) { if (g === generation) $('briefingRows').replaceChildren(node('p', 'Briefings unavailable: ' + e.message, 'note')); }
  }
  function render(reply) {
    const w = reply.work, counts = w.counts, c = reply.controller;
    renderDreaming(reply.dreaming);
    renderResources(reply);
    void renderBriefings(generation);
    $('login').hidden = true; $('dashboard').hidden = false; $('logout').hidden = false;
    $('error').hidden = true;
    $('health').textContent = human(reply.health); $('health').className = 'badge ' + reply.health;
    $('updated').textContent = 'Observed ' + at(w.captured_at) + ' · Tick ' + age(c?.last_tick) + (paused ? ' · Updates paused' : ' · Updates every 5 seconds');
    const open = Object.entries(counts).reduce((n, [s, v]) => n + (closed.has(s) ? 0 : v), 0);
    const active = (counts.running || 0) + (counts.leased || 0) + (counts.verifying || 0);
    $('stats').replaceChildren(
      stat(open, 'Open work', num(w.total_archived) + ' archived'),
      stat(active, 'Active items', num(c?.runs_in_flight) + ' runs in this daemon'),
      stat(counts.blocked || 0, 'Blocked items', Object.entries(w.blocked_reasons).map(([k,v]) => num(v) + ' ' + human(k)).join(' · ') || 'No blocked work'),
      stat(w.pending_questions, 'Questions for you', num(w.pending_notices) + ' notices awaiting delivery')
    );
    $('alertRows').replaceChildren();
    if (!reply.alerts.length) $('alertRows').append(node('p', 'No warning or critical condition observed.', 'note'));
    for (const a of reply.alerts) {
      const row = node('div', undefined, 'alert');
      row.append(badge(a.severity, a.severity === 'warning' ? 'degraded' : a.severity), node('p', a.message));
      if (a.item) row.append(itemButton(a.item, 'View'));
      $('alertRows').append(row);
    }
    $('projectRows').replaceChildren();
    const handoffs = w.items.filter(row => row.project_context);
    for (const row of handoffs) {
      const ctx = row.project_context, project = ctx.snapshot.project;
      const card = node('article', undefined, 'agent');
      card.append(node('h3', project.title), itemButton(row.item.id, row.item.title));
      card.append(node('p', 'Agent: ' + (row.last_worker || 'Unassigned') + ' · Context revision ' + ctx.snapshot.revision.id.slice(0, 12)));
      if (row.workspace) card.append(node('p', 'Code base: ' + row.workspace.base + ' · Branch ' + row.workspace.branch));
      const sources = (ctx.work || []).filter(work => (ctx.base_sources || []).includes(work.item));
      card.append(node('p', sources.length ? 'Continues verified work from ' + sources.map(work => work.worker || work.item.slice(0, 8)).join(', ') : 'No earlier verified code inherited.'));
      card.append(node('p', (ctx.work || []).filter(work => status(work) === 'done').length + ' completed items in supplied context · ' + (ctx.work || []).filter(work => !closed.has(status(work))).length + ' unfinished items'));
      const attempts = (ctx.work || []).filter(work => work.unfinished_attempt).length;
      if (attempts) card.append(node('p', attempts + ' unfinished attempt records available for inspection in context evidence.'));
      $('projectRows').append(card);
    }
    if (!handoffs.length) $('projectRows').append(node('p', 'No project handoff recorded for the displayed work.', 'empty'));
    $('agentCount').textContent = num(reply.workers.length) + ' registered';
    $('agents').replaceChildren();
    if (!reply.workers.length) $('agents').append(node('p', 'No registered agents observed.', 'card empty'));
    for (const a of reply.workers) {
      const live = w.items.filter(i => i.lease?.worker === a.name);
      const activeCount = w.active_by_worker[a.name] || 0;
      const card = node('article', undefined, 'card agent');
      const title = node('div', undefined, 'agent-title');
      title.append(node('span', a.name), badge(!a.live || !a.healthy ? 'unavailable' : activeCount ? 'active' : 'idle', !a.live || !a.healthy ? 'degraded' : activeCount ? 'running' : 'idle'));
      card.append(title, node('p', human(a.kind) + ' · ' + a.health), node('p', 'Last check ' + age(a.last_seen)), node('p', num(activeCount) + ' active assignments · Capacity ' + num(a.concurrency)));
      if (a.capabilities?.models?.length) card.append(node('p', 'Runtime: ' + a.capabilities.models.join(', ')));
      if (a.runtime) {
        const runtime = a.runtime;
        card.append(node('p', 'Subscription: ' + (runtime.subscription || 'unverified') + (runtime.account_fingerprint ? ' · Account ' + runtime.account_fingerprint : '')));
        card.append(node('p', (a.kind === 'codex' ? 'Codex profile: ' : 'Claude profile: ') + (runtime.codex_home || runtime.config_dir || 'default login')));
        for (const [bucket, limits] of Object.entries(runtime.rate_limits || {})) {
          for (const key of ['primary', 'secondary']) {
            const window = limits[key];
            if (!window || !Number.isFinite(window.used_percent)) continue;
            card.append(node('p', bucket + ' · ' + (window.window_minutes ? num(window.window_minutes) + ' min' : key) + ': ' + num(window.used_percent) + '% used' + (window.resets_at ? ' · Resets ' + new Date(window.resets_at * 1000).toLocaleString() : '')));
          }
        }
        if (runtime.quota_checked_at) card.append(node('p', 'Quota checked ' + age(runtime.quota_checked_at), 'note'));
        if (runtime.quota_error) card.append(node('p', runtime.quota_error, 'note'));
        if (runtime.rate_limited_until) card.append(node('p', 'Usage limit observed · Next retry ' + new Date(runtime.rate_limited_until).toLocaleString(), 'note'));
      }
      for (const row of live) {
        const assignment = node('div', undefined, 'assignment');
        assignment.append(itemButton(row.item.id, row.item.title), node('p', human(status(row.item)) + ' · Heartbeat ' + age(row.lease.heartbeat_at)));
        card.append(assignment);
      }
      const records = Object.values(a.routing_record || {});
      const sum = field => records.reduce((n, r) => n + (Number(r[field]) || 0), 0);
      card.append(node('p', 'Routing history: ' + num(sum('verified_done')) + ' verified · ' + num(sum('claimed_not_verified')) + ' unverified claims · ' + num(sum('failed')) + ' failed', 'record'));
      if (w.items_truncated) card.append(node('p', 'Assignments cover displayed items.', 'note'));
      $('agents').append(card);
    }
    renderWork();
    $('events').replaceChildren();
    for (const row of [...w.events].reverse()) {
      const e = row.event, li = node('li'), time = node('time', age(e.at));
      time.dateTime = e.at; time.title = at(e.at);
      const detail = node('div');
      detail.append(itemButton(e.item, human(e.kind) + ' · ' + e.item.slice(0, 8)));
      if (e.reason) detail.append(node('p', e.reason));
      detail.append(node('small', e.actor + (e.to ? ' → ' + human(typeof e.to === 'string' ? e.to : e.to.status) : '')));
      li.append(time, detail); $('events').append(li);
    }
    if (!w.events.length) $('events').append(node('li', 'No work events yet.', 'empty'));
    $('quality').replaceChildren();
    for (const m of reply.expectation_metrics) {
      const row = node('div', undefined, 'metric'), info = node('div');
      info.append(node('div', human(m.name)), node('small', num(m.sample) + ' observations · ' + m.window_days + ' days · ' + human(m.signal) + ' · ' + at(m.computed_at)));
      row.append(info, node('strong', m.sample ? Number(m.value).toLocaleString(undefined, { maximumFractionDigits: 3 }) + ' ' + m.unit : 'No observations'));
      $('quality').append(row);
    }
    if (!reply.expectation_metrics.length) $('quality').append(node('p', 'No evaluation recorded yet.', 'empty'));
    $('build').textContent = 'RustyKrab ' + reply.version + (reply.commit ? ' · ' + reply.commit.slice(0, 12) : '') + ' · Usage covers completed run records. Active run totals may still be pending.';
  }
  function renderWork() {
    if (!snapshot) return;
    const w = snapshot.work, needle = $('search').value.trim().toLowerCase(), filter = $('status').value;
    const selected = w.items.filter(row => {
      const s = status(row.item);
      return (filter === 'all' || filter === 'open' && !closed.has(s) || s === filter) &&
        (!needle || [row.item.id, row.item.title, row.last_worker].join(' ').toLowerCase().includes(needle));
    });
    $('workCount').textContent = num(selected.length) + ' displayed';
    $('workRows').replaceChildren();
    for (const row of selected) {
      const i = row.item, tr = node('tr'), title = node('td'), state = node('td');
      title.append(itemButton(i.id, i.title), node('small', i.id.slice(0, 8) + ' · ' + human(i.kind)));
      state.append(badge(status(i)));
      if (reason(i)) state.append(node('small', human(reason(i))));
      const agent = node('td', row.last_worker || 'Unassigned');
      if (row.lease) agent.append(node('small', 'Heartbeat ' + age(row.lease.heartbeat_at)));
      const evidence = node('td', num(row.verified_evidence_count) + ' verified');
      evidence.append(node('small', num(row.evidence_count - row.verified_evidence_count) + ' unverified records'));
      const usage = node('td', row.spend.runs ? num(row.spend.tokens) + ' tokens' : 'Pending');
      usage.append(node('small', row.spend.runs ? num(row.spend.runs) + ' runs · ' + (row.spend.wall_ms / 1000).toFixed(1) + 's' : 'No completed run record'));
      tr.append(title, state, agent, evidence, usage); $('workRows').append(tr);
    }
    if (!selected.length) {
      const tr = node('tr'), td = node('td', 'No work matches this view.', 'empty'); td.colSpan = 5;
      tr.append(td); $('workRows').append(tr);
    }
    $('workNote').textContent = 'Counts cover all work. ' + (w.items_truncated ? 'Showing ' + num(w.items.length) + ' of ' + num(w.total_live) + ' items, with active work first. ' : '') + 'Open an item to inspect its objective, evidence, and activity.';
  }
  async function refreshQuestions(requestGeneration) {
    try {
      const data = await api('/api/questions?status=open');
      if (requestGeneration !== generation) return;
      const signature = JSON.stringify(data.questions);
      if (signature === lastQuestions) return; // Preserve an answer being typed between refreshes.
      lastQuestions = signature;
      $('questions').hidden = !data.questions.length; $('questionRows').replaceChildren();
      for (const q of [...data.questions].reverse()) {
        const row = node('div', undefined, 'question');
        row.append(node('p', q.text), node('small', q.item.slice(0, 8) + ' · ' + human(q.kind)), itemButton(q.item, 'View work'));
        if (q.kind === 'credential') {
          row.append(node('p', 'Use the secure credential request in your notification to provide this credential.', 'note'));
        } else {
          if (q.options.length) row.append(node('p', q.options.map((o,i) => (i + 1) + '. ' + o).join(' · '), 'note'));
          const form = node('form'), input = node('input'), button = node('button', 'Answer', 'primary');
          input.required = true; input.autocomplete = 'off'; input.setAttribute('aria-label', 'Answer: ' + q.text);
          input.placeholder = q.kind === 'approval' ? 'approve or reject' : q.kind === 'consent' ? 'yes or no' : 'Your answer';
          const error = node('p', '', 'note'); error.setAttribute('role', 'alert');
          form.append(input, button); row.append(form, error);
          form.addEventListener('submit', async event => {
            event.preventDefault(); button.disabled = true;
            const g = generation;
            try {
              await api('/api/questions/' + encodeURIComponent(q.id) + '/answer', { answer: input.value });
              if (g !== generation) return;
              error.textContent = 'Answer recorded.'; input.value = ''; lastQuestions = ''; await refresh();
            } catch (e) { if (g === generation) error.textContent = e.message; }
            finally { button.disabled = false; }
          });
        }
        $('questionRows').append(row);
      }
    } catch (e) {
      if (requestGeneration === generation && access.connected) {
        $('questions').hidden = false;
        $('questionRows').replaceChildren(node('p', 'Questions could not be refreshed: ' + e.message, 'error'));
        lastQuestions = '';
      }
    }
  }
  async function refresh() {
    if (pending || !access.connected) return;
    pending = true; const g = generation; $('refresh').disabled = true;
    try {
      const reply = await api('/api/monitor');
      if (g !== generation) return;
      snapshot = reply; render(reply);
      await refreshQuestions(g);
    } catch (e) {
      if (g !== generation) return;
      if (snapshot) {
        $('error').hidden = false; $('error').textContent = 'Connection lost. Last observation: ' + at(snapshot.work.captured_at) + '. ' + e.message;
        $('health').textContent = 'Disconnected'; $('health').className = 'badge critical';
      } else $('loginError').textContent = e.message;
    } finally { if (g === generation) { pending = false; $('refresh').disabled = false; } }
  }
  async function showDetail(id) {
    const g = generation, request = ++detailRequest;
    $('detailTitle').textContent = 'Work item'; $('detailBody').replaceChildren(node('p', 'Loading…'));
    if (!$('detail').open) $('detail').showModal();
    try {
      const d = await api('/api/work/' + encodeURIComponent(id));
      if (g !== generation || request !== detailRequest || !$('detail').open) return;
      $('detailTitle').textContent = d.item.title;
      const body = $('detailBody'); body.replaceChildren(badge(status(d.item)), node('p', id, 'note'));
      if (d.item.kind === 'proposal' && !closed.has(status(d.item))) {
        const controls = node('div', undefined, 'controls');
        for (const [label, decision] of [['Accept for implementation', 'accept'], ['Decline', 'decline']]) {
          const b = node('button', label);
          b.addEventListener('click', async () => { b.disabled = true; try { await api('/api/work/' + encodeURIComponent(id) + '/review', { decision }); await refresh(); await showDetail(id); } catch (e) { b.disabled = false; body.append(node('p', e.message, 'error')); } });
          controls.append(b);
        } body.append(controls);
      }
      const section = (title, value) => { body.append(node('h3', title), node('pre', value)); };
      section('Objective', d.item.objective);
      section('Done when', d.item.done_when);
      if (d.item.constraints.length) section('Constraints', d.item.constraints.join('\n'));
      if (d.item.summary) section('Summary', d.item.summary);
      if (d.lease) section('Current assignment', d.lease.worker + '\nHeartbeat: ' + at(d.lease.heartbeat_at));
      if (d.last_error) section('Last error', JSON.stringify(d.last_error, null, 2));
      if (d.ladder.length) section('Recovery attempts', JSON.stringify(d.ladder, null, 2));
      body.append(node('h3', 'Evidence'));
      if (!d.evidence.length) body.append(node('p', 'No evidence recorded.', 'note'));
      for (const e of d.evidence) {
        body.append(node('p', e.verified_by ? 'Verified by ' + e.verified_by : 'Unverified record', 'note'), node('pre', JSON.stringify(e, null, 2)));
      }
      section('Activity', d.events.map(e => at(e.at) + ' · ' + human(e.kind) + ' · ' + e.actor + '\n' + (e.reason || '')).join('\n\n') || 'No activity recorded.');
    } catch (e) { if (g === generation && request === detailRequest) $('detailBody').replaceChildren(node('p', e.message, 'error')); }
  }
  $('pairForm').addEventListener('submit', async e => {
    e.preventDefault();
    if ($('pairButton').disabled) return;
    $('pairButton').disabled = true; $('pairError').textContent = '';
    await connect(false, () => access.pair($('pairCode').value, $('deviceName').value));
    $('pairButton').disabled = false;
  });
  $('loginForm').addEventListener('submit', e => { e.preventDefault(); connect(false, () => access.connectToken($('token').value)); });
  $('accessConnect').addEventListener('click', () => connect());
  $('logout').addEventListener('click', () => disconnect());
  $('dreamNow').addEventListener('click', async () => {
    const g = generation;
    $('dreamNow').disabled = true;
    try { const d = await api('/api/dreaming', {}); if (g !== generation) return; renderDreaming(d); await refresh(); }
    catch (e) { $('error').hidden = false; $('error').textContent = e.message; }
  });
  $('refresh').addEventListener('click', refresh);
  $('pause').addEventListener('click', () => {
    paused = !paused; $('pause').setAttribute('aria-pressed', String(paused));
    $('pause').textContent = paused ? 'Resume updates' : 'Pause updates';
    if (snapshot) render(snapshot);
    if (!paused) refresh();
  });
  $('search').addEventListener('input', renderWork); $('status').addEventListener('change', renderWork);
  $('closeDetail').addEventListener('click', () => $('detail').close());
  document.addEventListener('visibilitychange', () => { if (!document.hidden && !paused) refresh(); });
  connect(true);
})();
