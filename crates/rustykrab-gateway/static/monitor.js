/* Agent monitor. Only API headers carry authentication; model text is rendered as text. */
'use strict';
(() => {
  const $ = id => document.getElementById(id);
  const closed = new Set(['done', 'failed', 'cancelled', 'expired']);
  const access = window.RustyKrabAccess;
  let snapshot = null, pending = false, paused = false, interval = null, generation = 0;
  let priorityMoving = false, priorityError = '', priorityVersion = 0;
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
  function taskOverview(item) {
    const title = String(item.title || 'Untitled task');
    const goal = String(item.objective || '').trim() || 'No task goal has been recorded yet.';
    const outcome = String(item.done_when || '').trim() || 'No completion criteria have been recorded yet.';
    const recurring = /^Stop recurring ([^ ]+) failure [a-f0-9]{16}$/.exec(title);
    const unknown = /^Classify unknown failure [a-f0-9]{16}$/.test(title);
    const missing = /^Name the missing (tool|credential|consent|compute|knowledge) in failure [a-f0-9]{16}$/.exec(title);
    const failures = {'budget/tokens':'token-budget stops','budget/wall':'execution time limits','budget/iterations':'stalled runs','tool/upstream_error':'service request failures','tool/timeout':'tool timeouts','verification/incomplete':'incomplete results','verification/check_failed':'failed result checks','environment/permission':'file-access failures','environment/dependency':'missing-dependency failures'};
    if (recurring) return {
      title: 'Repair repeated ' + (failures[recurring[1]] || 'task failures'),
      goal: goal.replace(/^The same failure recurred across items \(fingerprint [a-f0-9]{16}, observed_by [^)]*\): /, 'Fix the cause of a failure affecting several tasks, or catch it before work starts. Recorded problem: ').replace(/\. Fix the cause, or add the check that catches it before dispatch, instead of repairing the symptom again\.$/, '.'),
      outcome: outcome.replace(/^The attached evidence no longer reproduces the failure, or a check rejects it before dispatch, and fingerprint [a-f0-9]{16} stops recurring\.$/, 'The recorded failure no longer occurs, or a check prevents it before work starts.')
    };
    if (unknown) return {
      title: 'Identify an unexplained task failure',
      goal: goal.replace(/^Add the probe, log line, check or parser that would have classified this failure\. It was recorded as unknown \(observed_by [^)]*\): /, 'Add the logs or checks needed to explain this recorded failure: '),
      outcome: outcome.replace(/^Replaying the attached evidence through the classifier yields a class other than unknown, and a test pins that classification for fingerprint [a-f0-9]{16}\.$/, 'Replaying the recorded failure produces a known error category, with a test confirming it.')
    };
    if (missing) return {
      title: 'Identify the missing ' + ({consent:'permission',compute:'computing resource',knowledge:'information'}[missing[1]] || missing[1]),
      goal: goal.replace(/^A capability gap was recorded without naming the ([^ ]+) it lacks \(fingerprint [a-f0-9]{16}, observed_by [^)]*\), so no capability item could be filed for it: /, 'Identify the missing requirement so the system can arrange it. Recorded problem: '),
      outcome: outcome.replace(/^Replaying the attached evidence through the classifier names the missing [^,]+, and a test pins that for fingerprint [a-f0-9]{16}\.$/, 'Replaying the recorded failure names the missing requirement, with a test confirming it.')
    };
    return {
      title: title.replace(/^Build tool: (.+)$/, 'Build the $1 tool'),
      goal,
      outcome
    };
  }
  function taskTitle(item) { return taskOverview(item).title; }
  function appendTaskOverview(target, item, compact = false) {
    const overview = taskOverview(item);
    for (const [label, text] of [['Goal',overview.goal],['Completion requires',overview.outcome]]) {
      const p = node('p', undefined, 'task-description' + (compact ? ' compact' : ''));
      p.append(node('strong', label + ': '), node('span', text)); target.append(p);
    }
    if (item.description_truncated) target.append(node('p', 'Open the task for its full description and completion criteria.', 'note'));
  }
  function taskState(item) {
    if (item.wait_reason) return item.wait_reason;
    const waits = {needs_consent:'Needs your permission',needs_decision:'Needs your decision',needs_credential:'Needs a credential',budget_exhausted:'Stopped at its execution budget',needs_tool:'Waiting for a required tool',worker_unavailable:'Waiting for an eligible agent',precondition_failed:'A required check has not passed',upstream_failed:'Waiting for a failed prerequisite to be resolved',upstream_expired:'A prerequisite expired',verification_failed:'The result did not pass verification'};
    if (item.held_by) return 'Waiting for your approval';
    return waits[reason(item)] || ({queued:'Waiting to be scheduled',ready:'Queued for an eligible agent',leased:'Assigned to an agent',running:'Work is running',verifying:'Checking the result',done:'Completed',failed:'Work failed',cancelled:'Cancelled',expired:'The task expired',blocked:'Waiting for recovery'}[status(item)] || 'State unavailable');
  }
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
    access.disconnect(); snapshot = null; lastQuestions = ''; discussionDrafts.clear();
    priorityMoving = false; priorityError = ''; $('priorityRows').replaceChildren(); $('priorityError').hidden = true;
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
      generation++; pending = false; lastQuestions = ''; preferenceSignature = ''; paused = false;
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
      card.append(node('h3', job.task.length > 120 ? job.task.slice(0, 120) + '…' : job.task), badge(job.enabled ? 'scheduled' : 'paused'), node('p', job.schedule + ' · ' + job.timezone + ' · Next ' + at(job.next_run_at)), node('p', 'Execution requirements' + (job.execution?.required_tools?.length ? ' · Tools ' + job.execution.required_tools.join(', ') : '') + (job.execution?.writable_resources?.length ? ' · Resources ' + job.execution.writable_resources.join(', ') : '')));
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
  function renderSubscription(card, kind, runtime) {
    card.append(node('p', 'Subscription: ' + (runtime.subscription || 'unverified') + (runtime.account_fingerprint ? ' · Account ' + runtime.account_fingerprint : '')));
    card.append(node('p', (kind === 'codex' ? 'Codex profile: ' : 'Claude profile: ') + (runtime.codex_home || runtime.config_dir || 'default login')));
    for (const [bucket, limits] of Object.entries(runtime.rate_limits || {})) {
      for (const key of ['primary', 'secondary']) {
        const window = limits[key];
        if (!window || !Number.isFinite(window.used_percent)) continue;
        card.append(node('p', bucket + ' · ' + (window.window_minutes ? num(window.window_minutes) + ' min' : key) + ': ' + num(window.used_percent) + '% used' + (window.resets_at ? ' · Resets ' + new Date(window.resets_at * 1000).toLocaleString() : '')));
      }
    }
    for (const [key, window] of Object.entries(runtime.quota_windows || {})) {
      const measured = Number.isFinite(window.used_percent) ? num(window.used_percent) + '% used' : 'usage unknown';
      const reset = window.resets_at ? ' · Resets ' + new Date(window.resets_at * 1000).toLocaleString() : '';
      card.append(node('p', (window.label || key) + ': ' + measured + (window.exhausted ? ' · Exhausted' : '') + reset));
    }
    if (kind === 'claude_code' && !runtime.quota_checked_at) card.append(node('p', 'Subscription usage has not been observed.', 'note'));
    if (runtime.quota_checked_at) card.append(node('p', 'Quota checked ' + age(runtime.quota_checked_at), 'note'));
    if (runtime.quota_error) card.append(node('p', runtime.quota_error, 'note'));
    if (runtime.rate_limited_until) card.append(node('p', 'Usage limit observed · Next retry ' + new Date(runtime.rate_limited_until).toLocaleString(), 'note'));
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
      stat(0, 'Work needing direction', 'Loading pending decisions')
    );
    $('alertRows').replaceChildren();
    if (!reply.alerts.some(a => a.severity !== 'info')) $('alertRows').append(node('p', 'No warning or critical condition observed.', 'note'));
    for (const a of reply.alerts.filter(a => a.severity !== 'info')) {
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
      card.append(node('h3', project.title), itemButton(row.item.id, taskTitle(row.item)));
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
      if (a.runtime) renderSubscription(card, a.kind, a.runtime);
      for (const row of live) {
        const assignment = node('div', undefined, 'assignment');
        assignment.append(itemButton(row.item.id, taskTitle(row.item)), node('p', human(status(row.item)) + ' · Heartbeat ' + age(row.lease.heartbeat_at)));
        card.append(assignment);
      }
      const records = Object.values(a.routing_record || {});
      const sum = field => records.reduce((n, r) => n + (Number(r[field]) || 0), 0);
      card.append(node('p', 'Routing history: ' + num(sum('verified_done')) + ' verified · ' + num(sum('claimed_not_verified')) + ' unverified claims · ' + num(sum('failed')) + ' failed', 'record'));
      if (w.items_truncated) card.append(node('p', 'Assignments cover displayed items.', 'note'));
      $('agents').append(card);
    }
    renderPriorityList(); renderWork();
    renderPriorityViews(reply);
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
  async function movePriority(index, offset) {
    if (priorityMoving || !snapshot?.priority_list) return;
    const list = snapshot.priority_list, ordered = list.entries.map(e => e.item), to = index + offset;
    if (to < 0 || to >= ordered.length) return;
    [ordered[index], ordered[to]] = [ordered[to], ordered[index]];
    const g = generation;
    priorityMoving = true; priorityVersion++; priorityError = ''; renderPriorityList();
    try {
      const updated = await api('/api/work/priority', {revision:list.revision, ordered});
      if (g !== generation || !snapshot) return;
      priorityVersion++;
      snapshot.priority_list = updated;
      renderWork();
    } catch (e) {
      if (g === generation) priorityError = e.message;
    } finally {
      if (g === generation) { priorityMoving = false; renderPriorityList(); }
    }
  }
  function renderPriorityList() {
    const list = snapshot?.priority_list, rows = $('priorityRows');
    rows.replaceChildren();
    $('priorityError').hidden = !priorityError;
    $('priorityError').textContent = priorityError;
    if (!list) {
      $('priorityCount').textContent = 'Unavailable';
      rows.append(node('li', 'The controller has not supplied its priority list.', 'empty'));
      return;
    }
    $('priorityCount').textContent = num(list.entries.length) + ' pending steps' + (priorityMoving ? ' · Saving…' : '');
    for (const [index, entry] of list.entries.entries()) {
      const row = node('li', undefined, 'priority-item'), position = node('span', num(entry.position), 'priority-position');
      const content = node('div', undefined, 'priority-content');
      content.append(itemButton(entry.item, taskTitle(entry)));
      appendTaskOverview(content, entry, true);
      content.append(node('p', 'Current state: ' + taskState(entry), 'priority-wait'));
      if (entry.goal) content.append(node('p', 'Part of: ' + taskTitle(entry.goal), 'note'));
      if (entry.waiters) content.append(node('p', 'Needed by ' + num(entry.waiters) + ' open items', 'note'));
      if (entry.waiting_on.length) {
        const waiting = node('div', undefined, 'priority-dependencies'); waiting.append(node('span', 'Waiting on: '));
        for (const dependency of entry.waiting_on) waiting.append(itemButton(dependency.item, taskTitle(dependency)));
        content.append(waiting);
      }
      const controls = node('div', undefined, 'priority-controls');
      for (const [label, offset] of [['Move up',-1],['Move down',1]]) {
        const b = node('button', label); b.type = 'button';
        b.setAttribute('aria-label', label + ': ' + taskTitle(entry));
        b.disabled = priorityMoving || index + offset < 0 || index + offset >= list.entries.length;
        b.addEventListener('click', () => movePriority(index, offset)); controls.append(b);
      }
      row.append(position, content, controls); rows.append(row);
    }
    if (!list.entries.length) rows.append(node('li', 'No pending work.', 'empty'));
  }
  function renderWork() {
    if (!snapshot) return;
    const w = snapshot.work, needle = $('search').value.trim().toLowerCase(), filter = $('status').value;
    const selected = w.items.filter(row => {
      const s = status(row.item);
      return progressStates.has(s) && (filter === 'progress' || s === filter) &&
        (!needle || [row.item.id, row.item.title, taskTitle(row.item), row.item.objective, row.item.done_when, row.last_worker].join(' ').toLowerCase().includes(needle));
    });
    const ranks = snapshot.queue_priorities || {};
    const positions = new Map((snapshot.priority_list?.entries || []).map(e => [e.item,e.position]));
    selected.sort((a,b) => {
      return (positions.get(a.item.id) ?? Number.MAX_SAFE_INTEGER) - (positions.get(b.item.id) ?? Number.MAX_SAFE_INTEGER) || Date.parse(a.item.created_at)-Date.parse(b.item.created_at) || a.item.id.localeCompare(b.item.id);
    });
    $('workCount').textContent = num(selected.length) + ' displayed';
    $('workRows').replaceChildren();
    for (const row of selected) {
      const i = row.item, tr = node('tr'), title = node('td'), state = node('td');
      title.append(itemButton(i.id, taskTitle(i)));
      appendTaskOverview(title, i, true);
      const rank = ranks[i.id];
      if (rank) title.append(node('small', rank.waiters ? 'Needed by ' + num(rank.waiters) + ' open tasks' : 'No dependent work'));
      state.append(badge(status(i) === 'ready' ? 'Queued' : status(i), status(i)));
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
    $('workNote').textContent = 'Counts cover all work. ' + (w.items_truncated ? 'Showing ' + num(w.items.length) + ' of ' + num(w.total_live) + ' items. ' : '') + 'Pending steps follow the controller’s priority list. Open an item to discuss direction and inspect progress.';
  }
  const progressStates = new Set(['queued', 'ready', 'leased', 'running', 'verifying']);
  const views = new Set(['priority', 'attention', 'progress', 'results', 'automatic', 'systems']);
  let currentView = views.has(window.location.hash.slice(1)) ? window.location.hash.slice(1) : 'priority';
  let preferences = null, preferenceSignature = '', preferencePending = false;
  const startDrafts = new Map();
  function selectView(view, updateLocation = true) {
    currentView = views.has(view) ? view : 'priority';
    for (const pane of document.querySelectorAll('[data-pane]')) pane.hidden = pane.dataset.pane !== currentView;
    for (const button of document.querySelectorAll('[data-view]')) button.setAttribute('aria-pressed', String(button.dataset.view === currentView));
    if (updateLocation) window.history.replaceState(null, '', window.location.pathname + window.location.search + '#' + currentView);
    if (currentView === 'automatic' && access.connected) void refreshPreferences(generation);
  }
  function renderPriorityViews(reply) {
    const rows = reply.work.items;
    $('proposalRows').replaceChildren(); $('recoveryRows').replaceChildren(); $('resultRows').replaceChildren();
    const proposals = rows.filter(r => r.item.kind === 'proposal' && !closed.has(status(r.item)));
    const recovery = rows.filter(r => r.item.kind !== 'proposal' && (status(r.item) === 'failed' || status(r.item) === 'blocked' && !['needs_decision','needs_consent','needs_credential','needs_tool'].includes(reason(r.item))));
    const results = rows.filter(r => r.item.kind !== 'proposal' && !(r.item.artifact_refs || []).some(a => a.kind === 'dream_review_job') && status(r.item) === 'done').sort((a,b) => Date.parse(b.item.updated_at) - Date.parse(a.item.updated_at)).slice(0,20);
    for (const [target, list, empty] of [['proposalRows', proposals, 'No proposals waiting to start.'], ['recoveryRows', recovery, 'No technical recovery blocks in this observation.'], ['resultRows', results, 'No completed execution results in this observation.']]) {
      if (!list.length) $(target).append(node('p', empty, 'empty'));
      for (const r of list) {
        const card = node('article', undefined, 'work-summary');
        card.append(itemButton(r.item.id, taskTitle(r.item)), badge(reason(r.item) || status(r.item)));
        appendTaskOverview(card, r.item, true);
        card.append(node('p', 'Current state: ' + taskState(r.item), 'note'), node('small', (r.last_worker || 'Unassigned') + ' · ' + age(r.item.updated_at)));
        if (target === 'resultRows') card.append(node('small', 'Open receipt for verification and execution evidence.'));
        $(target).append(card);
      }
    }
    if (reply.work.items_truncated) for (const target of ['proposalRows','recoveryRows','resultRows']) $(target).append(node('p','This is a bounded observation. Counts cover all work; some older items are not displayed.','note'));
  }
  async function refreshPreferences(g) {
    if (preferencePending || !access.connected) return;
    preferencePending = true;
    try {
      const data = await api('/api/work/preferences');
      if (g !== generation) return;
      preferences = data;
      const signature = JSON.stringify(data);
      if (signature === preferenceSignature) return;
      preferenceSignature = signature; $('preferenceRows').replaceChildren();
      if (!data.preferences.length) $('preferenceRows').append(node('p', 'No work preferences recorded yet. Open a work item and choose how to start it.', 'empty'));
      for (const p of [...data.preferences].reverse()) {
        const check = p.checks.find(c => c.check === 'work_template'); if (!check) continue;
        const t = check.template, card = node('article', undefined, 'work-summary');
        card.append(itemButton(t.example_item,taskTitle(t)), badge(p.revoked_at ? 'revoked' : check.mode === 'automatic' ? 'automatic' : 'once', p.revoked_at ? 'cancelled' : 'idle'));
        card.append(node('p', check.mode === 'automatic' ? 'Exact template · Up to ' + check.daily_limit + ' automatic starts per 24 hours' : 'One-time approval example · Grants no future permission'));
        if (p.text) card.append(node('p', p.text));
        const scope = node('details'); scope.append(node('summary', 'Reviewed scope'), node('pre', JSON.stringify(t,null,2))); card.append(scope, node('small', p.granted_by + ' · ' + at(p.granted_at)));
        if (check.mode === 'automatic' && !p.revoked_at) {
          const revoke = node('button', 'Revoke automatic permission');
          revoke.addEventListener('click', async () => {
            revoke.disabled = true;
            try { await api('/api/judgment/' + encodeURIComponent(p.id) + '/revoke', {}); if (g !== generation) return; preferenceSignature = ''; await refreshPreferences(g); }
            catch (e) { if (g === generation) card.append(node('p',e.message,'error')); }
            finally { revoke.disabled = false; }
          }); card.append(revoke);
        }
        $('preferenceRows').append(card);
      }
    } catch (e) { if (g === generation) { preferences = null; preferenceSignature = ''; $('preferenceRows').replaceChildren(node('p', 'Preferences could not be refreshed: ' + e.message, 'error')); } }
    finally { preferencePending = false; }
  }
  function renderStartOptions(d, body, g, request) {
    const item = d.item;
    if (closed.has(status(item)) || !(item.kind === 'proposal' || item.held_by || progressStates.has(status(item)))) return;
    body.dataset.startOptions = 'true';
    const key = item.id, draft = startDrafts.get(key) || {automatic:false,note:'',limit:3};
    const form = node('form',undefined,'start-options'), title = node('h3', item.kind === 'proposal' ? 'Start this proposal' : item.held_by ? 'Approve and start this work' : 'Record how to handle this work');
    const checkbox = node('input'); checkbox.type = 'checkbox'; checkbox.checked = draft.automatic;
    const choice = node('label'); choice.append(checkbox, node('span','Allow this exact work template to start automatically in future'));
    const scope = node('p', 'Same goal, completion criteria, constraints, project references, tools, resources and budget. Changed work needs another decision.', 'note');
    const note = node('textarea'); note.maxLength = 4096; note.value = draft.note; note.placeholder = 'Why this work matters, or how you want it handled'; note.setAttribute('aria-label','Work preference note');
    const limit = node('input'); limit.type = 'number'; limit.min = '1'; limit.max = '50'; limit.value = draft.limit; limit.setAttribute('aria-label','Maximum automatic starts per day');
    const limitLabel = node('label','Maximum automatic starts per 24 hours'); limitLabel.append(limit); limitLabel.hidden = !checkbox.checked;
    const button = node('button', checkbox.checked ? 'Start and allow this template' : 'Start once', 'primary'), error = node('p','','note'); error.setAttribute('role','alert');
    const save = () => {startDrafts.set(key,{automatic:checkbox.checked,note:note.value,limit:Number(limit.value)}); limitLabel.hidden = !checkbox.checked; button.textContent = checkbox.checked ? 'Start and allow this template' : 'Start once';};
    checkbox.addEventListener('change',save); note.addEventListener('input',save); limit.addEventListener('input',save);
    form.append(title,choice,scope,note,limitLabel,button,error);
    if (item.held_by) {
      const reject=node('button','Reject this plan'); reject.type='button';
      reject.addEventListener('click',async()=>{
        reject.disabled=true;
        try {await api('/api/work/'+encodeURIComponent(item.id)+'/reject',{reason:note.value || 'Rejected by the owner'}); if (g!==generation || request!==detailRequest) return; startDrafts.delete(key); await refresh(); await showDetail(item.id);}
        catch(e){if(g===generation && request===detailRequest) error.textContent=e.message;}
        finally {reject.disabled=false;}
      }); form.append(reject);
    }

    form.addEventListener('submit', async event => {
      event.preventDefault(); button.disabled = true;
      try {
        await api('/api/work/' + encodeURIComponent(item.id) + '/start',{mode:checkbox.checked ? 'automatic' : 'once',daily_limit:Number(limit.value),note:note.value});
        if (g !== generation || request !== detailRequest) return;
        startDrafts.delete(key); preferenceSignature = ''; await refresh(); await showDetail(item.id);
      } catch (e) { if (g === generation && request === detailRequest) error.textContent = e.message; }
      finally { button.disabled = false; }
    }); body.append(form);
  }
  const discussionDrafts = new Map();
  function questionsByWork(questions) {
    const groups = new Map();
    for (const q of questions) {
      if (!groups.has(q.item)) groups.set(q.item, []);
      groups.get(q.item).push(q);
    }
    return groups;
  }
  async function refreshQuestions(requestGeneration) {
    try {
      const data = await api('/api/questions?status=open');
      if (requestGeneration !== generation) return;
      const groups = questionsByWork(data.questions);
      const card = $('stats').lastElementChild;
      const proposals = snapshot?.work.items.filter(r => r.item.kind === 'proposal' && !closed.has(status(r.item))).map(r => r.item.id) || [];
      if (card) { card.querySelector('.number').textContent = num(new Set([...groups.keys(),...proposals]).size); card.querySelector('.caption').textContent = num(data.questions.length) + (data.questions.length === 1 ? ' pending decision · ' : ' pending decisions · ') + num(proposals.length) + ' displayed proposals'; }
      const signature = JSON.stringify(data.questions);
      if (signature === lastQuestions) return;
      lastQuestions = signature;
      $('questions').hidden = !groups.size; $('questionRows').replaceChildren();
      for (const [id, questions] of groups) {
        const item = snapshot?.work.items.find(r => r.item.id === id)?.item || snapshot?.priority_list?.entries.find(e => e.item === id);
        const row = node('article', undefined, 'question');
        row.append(itemButton(id, item ? taskTitle(item) : 'Work ' + id.slice(0, 8)));
        if (item) appendTaskOverview(row, item, true);
        row.append(node('p', questions.length + (questions.length === 1 ? ' pending decision' : ' pending decisions') + ' · ' + (item ? taskState(item) : 'Waiting for your direction'), 'note'));
        row.append(node('p', questions[0].text), node('small', 'Questions and replies stay in this work item’s discussion.'));
        $('questionRows').append(row);
      }
    } catch (e) {
      if (requestGeneration === generation && access.connected) {
        $('questions').hidden = false; $('questionRows').replaceChildren(node('p', 'Decisions could not be refreshed: ' + e.message, 'error')); lastQuestions = '';
      }
    }
  }
  async function renderDiscussion(id, body, g, request) {
    const data = await api('/api/work/' + encodeURIComponent(id) + '/discussion');
    if (g !== generation || request !== detailRequest || !$('detail').open) return;
    const section = node('section', undefined, 'discussion');
    section.append(node('h3', 'Discussion and decisions'), node('p', 'Comments reach the next agent run. Answer pending decisions explicitly to unblock work.', 'note'));
    const refreshButton = node('button', 'Refresh discussion');
    refreshButton.addEventListener('click', () => showDetail(id)); section.append(refreshButton);
    const repeated = data.questions.filter(q => q.superseded_by).length;
    if (repeated) section.append(node('p', repeated + ' repeated requests consolidated; their history is retained.', 'note'));
    const entries = data.comments.map(e => ({ at: e.at, author: e.actor, text: e.reason, type: 'comment' }));
    for (const e of data.activity || []) {
      if (e.kind === 'transition' && e.to) entries.push({ at: e.at, author: e.actor, text: 'Work status: ' + human(typeof e.to === 'string' ? e.to : e.to.status), type: 'status' });
    }
    for (const q of data.questions) {
      if (q.superseded_by) continue;
      entries.push({ at: q.created_at, author: q.asked_by, text: q.text, type: 'question', question: q });
      if (q.answer) entries.push({ at: q.answered_at, author: q.answered_by, text: q.answer, type: 'answer' });
    }
    entries.sort((a,b) => new Date(a.at) - new Date(b.at));
    const timeline = node('div', undefined, 'discussion-timeline'); section.append(timeline);
    if (!entries.length) timeline.append(node('p', 'Start the discussion with direction or a comment.', 'note'));
    for (const entry of entries) {
      const row = node('article', undefined, 'discussion-entry');
      row.append(node('small', (entry.author?.startsWith('user:') ? 'You' : entry.author?.startsWith('worker:') ? entry.author.slice(7) : entry.author === 'agent' ? 'Manager' : entry.author) + ' · ' + at(entry.at) + ' · ' + human(entry.type)), node('p', entry.text));
      const q = entry.question;
      if (q?.status === 'open') {
        if (q.kind === 'approval' && body.dataset.startOptions === 'true') row.append(node('p', 'Choose Start once or automatic permission above to approve this work.', 'note'));
        else if (q.kind === 'credential') row.append(node('p', 'Provide credentials through the secure request linked in your notification.', 'note'));
        else {
          if (q.options.length) row.append(node('p', q.options.map((o,i) => (i+1) + '. ' + o).join(' · '), 'note'));
          const form = node('form'), input = node('textarea'), button = node('button', 'Record decision', 'primary'), error = node('p', '', 'note');
          const key = 'answer:' + q.id; input.required = true; input.value = discussionDrafts.get(key) || ''; input.setAttribute('aria-label', 'Decision: ' + q.text);
          input.placeholder = q.kind === 'approval' ? 'approve or reject' : q.kind === 'consent' ? 'yes or no' : 'Your decision'; input.addEventListener('input', () => discussionDrafts.set(key, input.value)); error.setAttribute('role', 'alert');
          form.append(input, button); row.append(form, error);
          form.addEventListener('submit', async event => {
            event.preventDefault(); button.disabled = true;
            try { await api('/api/questions/' + encodeURIComponent(q.id) + '/answer', { answer: input.value }); if (g !== generation || request !== detailRequest) return; discussionDrafts.delete(key); lastQuestions = ''; await refresh(); await showDetail(id); }
            catch (e) { if (g === generation && request === detailRequest) error.textContent = e.message; }
            finally { button.disabled = false; }
          });
        }
      } else if (q) row.append(node('small', human(q.status), 'note'));
      timeline.append(row);
    }
    if (data.total_comments > data.comments.length) {
      const older = node('button', 'Load earlier comments');
      older.addEventListener('click', async () => {
        older.disabled = true;
        try {
          const page = await api('/api/work/' + encodeURIComponent(id) + '/discussion?offset=' + timeline.dataset.offset + '&limit=100');
          if (g !== generation || request !== detailRequest) return;
          for (const e of page.comments) { const row = node('article', undefined, 'discussion-entry'); row.append(node('small', e.actor + ' · ' + at(e.at)), node('p', e.reason)); timeline.prepend(row); }
          timeline.dataset.offset = Number(timeline.dataset.offset) + page.comments.length; older.hidden = Number(timeline.dataset.offset) >= page.total_comments;
        } catch (e) { section.append(node('p', e.message, 'error')); } finally { older.disabled = false; }
      }); timeline.dataset.offset = data.comments.length; section.append(older);
    }
    const form = node('form'), label = node('label', 'Leave direction or a comment'), input = node('textarea'), button = node('button', 'Post comment', 'primary'), error = node('p', '', 'note');
    const key = 'comment:' + id; input.required = true; input.maxLength = 4096; input.value = discussionDrafts.get(key) || ''; input.setAttribute('aria-label', 'Work item comment'); input.addEventListener('input', () => discussionDrafts.set(key, input.value)); error.setAttribute('role', 'alert'); form.append(label, input, button, error);
    form.addEventListener('submit', async event => {
      event.preventDefault(); button.disabled = true;
      try { await api('/api/work/' + encodeURIComponent(id) + '/discussion', { text: input.value }); if (g !== generation || request !== detailRequest) return; discussionDrafts.delete(key); await showDetail(id); }
      catch (e) { if (g === generation && request === detailRequest) error.textContent = e.message; }
      finally { button.disabled = false; }
    }); section.append(form); body.append(section);
    timeline.scrollTop = timeline.scrollHeight;
  }
  async function refresh() {
    if (pending || priorityMoving || !access.connected) return;
    pending = true; const g = generation, observedPriority = priorityVersion; $('refresh').disabled = true;
    try {
      const reply = await api('/api/monitor');
      if (g !== generation) return;
      // An observation started before a move cannot undo its returned order.
      if (observedPriority !== priorityVersion && snapshot) reply.priority_list = snapshot.priority_list;
      snapshot = reply; render(reply);
      await refreshQuestions(g);
      if (currentView === 'automatic') await refreshPreferences(g);
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
      $('detailTitle').textContent = taskTitle(d.item);
      const body = $('detailBody'); delete body.dataset.startOptions; body.replaceChildren(node('p', 'Current state: ' + taskState(d.item), 'task-state'));
      appendTaskOverview(body, d.item);
      const priority = snapshot?.priority_list?.entries.find(entry => entry.item === id);
      if (priority?.waiting_on?.length) {
        const waiting = node('p', 'Waiting on: ', 'task-dependencies');
        for (const dependency of priority.waiting_on) waiting.append(itemButton(dependency.item, taskTitle(dependency)));
        body.append(waiting);
      }
      if (d.item.parent) body.append(itemButton(d.item.parent, 'View parent work'));
      if (d.item.kind === 'proposal' && !closed.has(status(d.item))) {
        const controls = node('div', undefined, 'controls');
        for (const [label, decision] of [['Decline', 'decline']]) {
          const b = node('button', label);
          b.addEventListener('click', async () => { b.disabled = true; try { await api('/api/work/' + encodeURIComponent(id) + '/review', { decision }); await refresh(); await showDetail(id); } catch (e) { b.disabled = false; body.append(node('p', e.message, 'error')); } });
          controls.append(b);
        } body.append(controls);
      }
      const section = (title, value) => { body.append(node('h3', title), node('pre', value)); };
      const scope = node('section'); scope.append(node('h3', 'Requested scope'));
      const resources = d.item.writable_resources.map(resource => resource.replace(/^repo:/, 'Repository: ').replace(/^file:/, 'File: '));
      scope.append(node('p', resources.length ? 'May change: ' + resources.join(' · ') : 'No writable resources requested.', 'task-description'));
      if (d.item.required_tools.length) scope.append(node('p', 'Required tools: ' + d.item.required_tools.join(', '), 'task-description'));
      if (d.item.required_mcp_servers.length) scope.append(node('p', 'Required integrations: ' + d.item.required_mcp_servers.join(', '), 'task-description'));
      body.append(scope);
      renderStartOptions(d, body, g, request);
      if (d.item.constraints.length) {
        const limits = node('section'); limits.append(node('h3', 'Limits on this task'));
        for (const constraint of d.item.constraints) limits.append(node('p', constraint, 'task-description'));
        body.append(limits);
      }
      await renderDiscussion(id, body, g, request);
      if (g !== generation || request !== detailRequest || !$('detail').open) return;
      if (d.item.summary) section('Summary', d.item.summary);
      if (d.lease) section('Current assignment', d.lease.worker + '\nHeartbeat: ' + at(d.lease.heartbeat_at));
      const technical = node('details'), technicalBody = node('div');
      technical.append(node('summary', 'Technical details'), technicalBody);
      const raw = (title,value) => technicalBody.append(node('h3',title),node('pre',value));
      raw('Task ID', id);
      raw('Original title', d.item.title);
      raw('Full execution goal', d.item.objective);
      raw('Full completion criteria', d.item.done_when);
      raw('Resources and tools', [...d.item.writable_resources, ...d.item.required_tools, ...d.item.required_mcp_servers].join('\n') || 'No write resources or required tools declared.');
      if (d.last_error) raw('Last error', JSON.stringify(d.last_error, null, 2));
      if (d.ladder.length) raw('Recovery attempts', JSON.stringify(d.ladder, null, 2));
      body.append(technical);
      const evidenceDetails = node('details'), evidenceBody = node('div');
      evidenceDetails.append(node('summary', 'Execution evidence (' + d.evidence.length + ' records)'), evidenceBody); body.append(evidenceDetails);
      if (!d.evidence.length) evidenceBody.append(node('p', 'No evidence recorded.', 'note'));
      for (const e of d.evidence) {
        evidenceBody.append(node('p', e.kind === 'work_discussion' ? 'Frozen discussion input' : e.verified_by ? 'Verified by ' + e.verified_by : 'Unverified record', 'note'), node('pre', JSON.stringify(e, null, 2)));
      }
      const activityDetails = node('details'); activityDetails.append(node('summary', 'Full activity history'), node('pre', d.events.map(e => at(e.at) + ' · ' + human(e.kind) + ' · ' + e.actor + '\n' + (e.reason || '')).join('\n\n') || 'No activity recorded.')); body.append(activityDetails);
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
  for (const button of document.querySelectorAll('[data-view]')) button.addEventListener('click', () => selectView(button.dataset.view));
  window.addEventListener('hashchange', () => selectView(window.location.hash.slice(1), false));
  $('exportPreferences').addEventListener('click', async () => {
    try {
      await refreshPreferences(generation); if (!preferences || !access.connected) return;
      const url = URL.createObjectURL(new Blob([JSON.stringify(preferences,null,2)],{type:'application/json'}));
      const link = node('a'); link.href = url; link.download = 'rustykrab-work-preferences.json'; link.click(); URL.revokeObjectURL(url);
    } catch (e) { $('preferenceRows').append(node('p',e.message,'error')); }
  });
  selectView(currentView, false);
  $('search').addEventListener('input', renderWork); $('status').addEventListener('change', renderWork);
  $('closeDetail').addEventListener('click', () => $('detail').close());
  document.addEventListener('visibilitychange', () => { if (!document.hidden && !paused) refresh(); });
  connect(true);
})();
