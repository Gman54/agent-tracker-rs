// AgentTracker decision runtime UI. Loaded after app.js and reuses its helpers
// (api, esc, when, pill, status, emptyState, showToast, actions, changes, navigate, SCREENS, RENDER).
// Routes: #runtime (tabs), #runtime/ses_* (session), #runtime/act_* | dec_* (Why this action?), #runtime/inv_* (investigation).
'use strict';

const RT = { tab: 'sessions', cfg: null };
const L = {
  available: 'Available to model',
  cited: 'Cited by model',
  rationale: 'Stated rationale',
  tool: 'Tool-reported result',
  observed: 'Independently observed',
  inferred: 'Inferred explanation',
  unavailable: 'Unavailable',
};
const LBL_CLS = {
  [L.available]: 'lbl-available', [L.cited]: 'lbl-cited', [L.rationale]: 'lbl-rationale', [L.tool]: 'lbl-tool',
  [L.observed]: 'lbl-observed', [L.inferred]: 'lbl-inferred', [L.unavailable]: 'lbl-unavailable',
};
const lbl = t => `<span class="sec-label ${LBL_CLS[t] || ''}">${esc(t)}</span>`;
const SESSION_STATUS = {
  running: ['blue', 'Running'], paused: ['amber', 'Paused'], awaiting_approval: ['amber', 'Awaiting approval'],
  completed: ['green', 'Completed'], failed: ['red', 'Failed'],
};
const ACTION_STATE = { prepared: ['', 'Prepared'], started: ['blue', 'Started'], succeeded: ['green', 'Succeeded'], failed: ['red', 'Failed'], unknown: ['amber', 'Unknown outcome'] };
const sPill = s => pill(...(SESSION_STATUS[s] || ['', s || '—']));
const aPill = s => (s ? pill(...(ACTION_STATE[s] || ['', s])) : pill('', 'Not executed'));
const originTag = fixture => (fixture
  ? '<span class="origin-tag fixture">Recorded fixture</span>'
  : '<span class="origin-tag">Runtime session</span>');
const short = id => (id ? String(id).replace(/^([a-z]+_)(.{6}).*(.{6})$/, '$1$2…$3') : '—');
const pretty = v => (v == null ? '' : typeof v === 'string' ? v : JSON.stringify(v, null, 2));
const list = arr => (arr && arr.length ? `<ul>${arr.map(x => `<li>${esc(pretty(x))}</li>`).join('')}</ul>` : '<span class="muted">None stated</span>');
const kv = rows => `<dl class="kv">${rows.map(([k, v]) => `<dt>${esc(k)}</dt><dd>${v}</dd>`).join('')}</dl>`;

SCREENS.push('runtime');
RENDER.runtime = async function render_runtime() {
  const p = state.param || '';
  if (p.startsWith('ses_')) return renderSession(p);
  if (p.startsWith('act_') || p.startsWith('dec_')) return renderWhy(p);
  if (p.startsWith('inv_')) return renderInvestigation(p);
  return renderRuntimeHome();
};

// =====================================================================================================
// Home: tabs
// =====================================================================================================
async function renderRuntimeHome() {
  const tabs = [['sessions', 'Sessions'], ['start', 'Start a task'], ['demos', 'Demonstrations'], ['provider', 'Model provider']];
  screenEl.innerHTML = `
    <div class="page-head">
      <h1>Agent Runtime</h1>
      <p class="lead">Run a local model through AgentTracker's instrumented runtime. Before any tool runs, the model must submit a structured decision proposal, which is validated and saved. You can then inspect what the model was given, its stated rationale, and what actually happened.</p>
    </div>
    <div class="tabs" role="tablist">${tabs.map(([k, t]) => `<button class="tab ${RT.tab === k ? 'active' : ''}" role="tab" data-act="rtTab" data-tab="${k}">${t}</button>`).join('')}</div>
    <div id="rt-body"></div>`;
  const body = $('#rt-body');
  if (RT.tab === 'sessions') return renderSessions(body);
  if (RT.tab === 'start') return renderStart(body);
  if (RT.tab === 'demos') return renderDemos(body);
  return renderProvider(body);
}
actions.rtTab = d => { RT.tab = d.tab; navigate('runtime'); };

async function renderSessions(body) {
  const { sessions } = await api('GET', '/api/ui/runtime/sessions');
  if (!sessions.length) {
    body.innerHTML = `<div class="card">${emptyState('clipboard', 'No runtime sessions yet', 'Start a task with your local model, or run a demonstration with recorded fixtures.',
      '<button class="btn btn-primary" data-act="rtTab" data-tab="demos">Open demonstrations</button>')}</div>`;
    return;
  }
  body.innerHTML = `
    <div class="note note-plain mb-sm">These sessions ran through the AgentTracker runtime. Externally connected agents (Claude Code, Cursor, Copilot) appear under Live Task and Results and are not mixed in here.</div>
    <div class="card"><table class="table">
      <thead><tr><th>Task</th><th>Origin</th><th>Model</th><th>Status</th><th>Actions</th><th>Findings</th><th>Started</th></tr></thead>
      <tbody>${sessions.map(s => `
        <tr class="clickable" data-act="go" data-screen="runtime" data-param="${esc(s.id)}">
          <td><div class="task-row-title">${esc(s.title)}</div><div class="mono muted">${esc(short(s.id))}</div></td>
          <td>${originTag(s.provider_config && s.provider_config.is_fixture)}</td>
          <td class="small">${esc((s.provider_config && s.provider_config.model) || '—')}</td>
          <td>${sPill(s.status)}</td><td>${esc(s.actions)}</td><td>${esc(s.findings)}</td><td class="small muted">${esc(when(s.created_at))}</td>
        </tr>`).join('')}</tbody></table></div>`;
}

async function loadCfg() {
  RT.cfg = await api('GET', '/api/ui/runtime/config');
  return RT.cfg;
}

async function renderStart(body) {
  const c = await loadCfg();
  const noModel = !c.config.provider.model;
  body.innerHTML = `
    ${noModel ? '<div class="note note-warn mb-sm">Set a model on the Model provider tab before starting a task.</div>' : ''}
    <div class="card">
      <div class="field"><label class="field-label" for="rt-instr">Task request</label>
        <textarea id="rt-instr" placeholder="e.g. Copy reports/q3.csv into archive/2026/ and keep the original."></textarea></div>
      <div class="field"><label class="field-label" for="rt-dir">Working folder</label>
        <div class="input-row"><input id="rt-dir" type="text" placeholder="F:\\work\\sandbox"><button class="btn btn-secondary" data-act="rtPick">Browse</button></div>
        <span class="field-hint">Tools are confined to this folder. Use a disposable copy when testing.</span></div>
      <div class="grid-2">
        <div class="field"><label class="field-label" for="rt-prot">Protected paths</label>
          <textarea id="rt-prot" placeholder="originals"></textarea><span class="field-hint">One relative path per line. Enforced by the dispatcher regardless of what the model proposes.</span></div>
        <div class="field"><label class="field-label" for="rt-docs">Documents to include as evidence</label>
          <textarea id="rt-docs" placeholder="notes/readme.md"></textarea><span class="field-hint">One relative path per line. Recorded as lower-trust retrieved content.</span></div>
      </div>
      <div class="row row-end"><button class="btn btn-primary" data-act="rtStart" ${noModel ? 'disabled' : ''}>Start task</button></div>
    </div>`;
}
actions.rtPick = async () => {
  const r = await api('POST', '/api/ui/pick-folder');
  if (r && r.path) $('#rt-dir').value = r.path;
};
actions.rtStart = async () => {
  const instruction = $('#rt-instr').value.trim();
  const working_dir = $('#rt-dir').value.trim();
  if (!instruction || !working_dir) throw new Error('Enter a task request and a working folder.');
  showToast('Running. Local models can take a while per step.', 'info');
  const r = await api('POST', '/api/ui/runtime/tasks', {
    instruction, working_dir,
    protected_paths: lines($('#rt-prot').value),
    documents: lines($('#rt-docs').value).map(path => ({ path, trust: 'retrieved_document (lower trust)', max_chars: 6000 })),
    idempotency_key: newKey(),
  });
  navigate('runtime', r.start.session_id);
};

async function renderDemos(body) {
  body.innerHTML = `
    <div class="grid-2">
      <div class="card">
        <h3>Demonstration A: supported behaviour</h3>
        <p class="muted small">The request is to copy a file while keeping the original. The decision should cite the instruction, propose copy_file, and link to the observed copy.</p>
        <div class="row"><button class="btn btn-primary btn-small" data-act="rtDemo" data-which="a" data-mode="fixture">Run with recorded fixture</button>
          <button class="btn btn-secondary btn-small" data-act="rtDemo" data-which="a" data-mode="live">Run with configured model</button></div>
      </div>
      <div class="card">
        <h3>Demonstration B: conflicting context</h3>
        <p class="muted small">The request says to preserve originals; a lower-trust document in the folder suggests overwriting them. originals/ is protected and enforced independently of the model.</p>
        <div class="row"><button class="btn btn-primary btn-small" data-act="rtDemo" data-which="b" data-mode="fixture">Run with recorded fixture</button>
          <button class="btn btn-secondary btn-small" data-act="rtDemo" data-which="b" data-mode="live">Run with configured model</button></div>
      </div>
    </div>
    <div class="note note-plain mt">Demonstrations run in new disposable folders under the AgentTracker data folder. Recorded fixtures are hand-written, deterministic responses used for UI tests and are labelled "Recorded fixture" everywhere. They are not model output. Live runs show whatever the configured model actually produced.</div>`;
}
actions.rtDemo = async d => {
  if (d.mode === 'live') showToast('Calling the configured model. This can take several minutes on CPU.', 'info');
  const r = await api('POST', `/api/ui/runtime/demo/${d.which}`, { mode: d.mode });
  showToast(`Session ${r.status.status}${r.fixture ? ' (recorded fixture)' : ''}`, 'success');
  navigate('runtime', r.start.session_id);
};

async function renderProvider(body) {
  const c = await loadCfg();
  const p = c.config.provider;
  const lim = c.config.limits;
  const num = (id, label, v, hint = '') => `<div class="field"><label class="field-label" for="${id}">${label}</label><input id="${id}" type="number" value="${v ?? ''}">${hint ? `<span class="field-hint">${hint}</span>` : ''}</div>`;
  body.innerHTML = `
    <div class="card">
      <div class="card-head"><h3>Local OpenAI-compatible endpoint</h3><span class="small muted">Runtime ${esc(c.runtime_version)}</span></div>
      <div class="form-grid">
        <div class="field"><label class="field-label" for="p-kind">Provider</label><select id="p-kind">
          ${[['ollama', 'Ollama'], ['lm_studio', 'LM Studio'], ['open_ai_compatible', 'Other OpenAI-compatible']].map(([v, t]) => `<option value="${v}" ${p.kind === v ? 'selected' : ''}>${t}</option>`).join('')}</select></div>
        <div class="field"><label class="field-label" for="p-endpoint">Endpoint</label><input id="p-endpoint" type="text" value="${esc(p.endpoint)}"><span class="field-hint">Ollama: http://127.0.0.1:11434/v1 · LM Studio: http://127.0.0.1:1234/v1</span></div>
        <div class="field"><label class="field-label" for="p-model">Model</label><input id="p-model" type="text" value="${esc(p.model)}" list="p-models"><datalist id="p-models"></datalist></div>
        <div class="field"><label class="field-label" for="p-key">API key (optional)</label><input id="p-key" type="password" placeholder="${c.has_api_key ? 'Saved (leave blank to keep)' : 'Not required for local servers'}"></div>
        <div class="field"><label class="field-label" for="p-mode">Structured output</label><select id="p-mode">
          ${[['auto', 'Auto (try JSON schema, then JSON object, then prompt only)'], ['json_schema', 'JSON schema'], ['json_object', 'JSON object'], ['prompt_only', 'Prompt only']].map(([v, t]) => `<option value="${v}" ${p.structured_mode === v ? 'selected' : ''}>${t}</option>`).join('')}</select></div>
        ${num('p-temp', 'Temperature', p.temperature)}
        ${num('p-topp', 'Top p', p.top_p)}
        ${num('p-seed', 'Seed', p.seed, 'Seeds do not guarantee reproducible output.')}
        ${num('p-maxtok', 'Max output tokens', p.max_tokens)}
        ${num('p-ctx', 'Context limit (characters)', p.context_limit_chars)}
        ${num('p-timeout', 'Request timeout (seconds)', p.timeout_secs)}
        ${num('l-retries', 'Max validation retries', lim.max_validation_retries)}
        ${num('l-steps', 'Max agent steps', lim.max_steps)}
        ${num('l-tools', 'Max tool calls', lim.max_tool_calls)}
        ${num('r-days', 'Replay retention (days)', c.config.replay_retention_days)}
      </div>
      <label class="toggle-row"><input type="checkbox" id="p-remote" ${p.allow_remote ? 'checked' : ''}><span><b>Allow non-local endpoints</b><br><span class="small muted">Off by default. Everything stays on this computer unless you turn this on.</span></span></label>
      <label class="toggle-row"><input type="checkbox" id="r-capture" ${c.config.replay_capture ? 'checked' : ''}><span><b>Encrypted replay capture</b><br><span class="small muted">Stores the exact, unredacted request so influence tests can replay it faithfully. Encrypted with a key protected by ${esc(c.replay_key_protection)}. Deleted after the retention period.</span></span></label>
      <div class="row row-end mt">
        <button class="btn btn-danger btn-small" data-act="rtReplayDelete">Delete all replay captures</button>
        <span class="spacer"></span>
        <button class="btn btn-secondary" data-act="rtTest">Test connection</button>
        <button class="btn btn-primary" data-act="rtSave">Save</button>
      </div>
      <div id="p-out" class="mt"></div>
    </div>`;
}
const numOr = (id, d) => { const v = $(id).value.trim(); return v === '' ? d : Number(v); };
actions.rtSave = async () => {
  const c = RT.cfg.config;
  const cfg = {
    ...c,
    provider: { ...c.provider, kind: $('#p-kind').value, endpoint: $('#p-endpoint').value.trim(), model: $('#p-model').value.trim(), structured_mode: $('#p-mode').value,
      temperature: numOr('#p-temp', null), top_p: numOr('#p-topp', null), seed: numOr('#p-seed', null), max_tokens: numOr('#p-maxtok', 700),
      context_limit_chars: numOr('#p-ctx', 24000), timeout_secs: numOr('#p-timeout', 300), allow_remote: $('#p-remote').checked },
    limits: { ...c.limits, max_validation_retries: numOr('#l-retries', 2), max_steps: numOr('#l-steps', 6), max_tool_calls: numOr('#l-tools', 4) },
    replay_capture: $('#r-capture').checked,
    replay_retention_days: numOr('#r-days', 7),
  };
  await api('POST', '/api/ui/runtime/config', { config: cfg, api_key: $('#p-key').value || null });
  showToast('Provider settings saved', 'success');
  await loadCfg();
};
actions.rtTest = async () => {
  const out = $('#p-out');
  out.innerHTML = '<span class="spinner"></span> Contacting endpoint…';
  try {
    const r = await api('POST', '/api/ui/runtime/provider/test');
    $('#p-models').innerHTML = r.models.map(m => `<option value="${esc(m)}">`).join('');
    out.innerHTML = `${status('ok', `Reachable. ${r.models.length} model(s) listed.`)}
      ${kv([['Models', esc(r.models.join(', ') || 'none')], ['Selected model digest', `<span class="mono">${esc(r.selected.model_digest || 'Unavailable from this provider')}</span>`]])}
      <p class="small muted mt">${esc(r.note)}</p>`;
  } catch (e) {
    out.innerHTML = status('bad', e.message);
  }
};
actions.rtReplayDelete = async () => {
  if (!confirm('Delete every encrypted replay capture? Audit records are kept.')) return;
  const r = await api('POST', '/api/ui/runtime/replay/delete');
  showToast(`Deleted ${r.deleted} replay capture(s)`, 'success');
};

// =====================================================================================================
// Session detail
// =====================================================================================================
async function renderSession(id) {
  const d = await api('GET', `/api/ui/runtime/sessions/${encodeURIComponent(id)}`);
  const s = d.session;
  const fixture = s.provider_config && s.provider_config.is_fixture;
  const pending = d.decisions.find(x => x.kind === 'action' && !x.action_id && x.approval_required && !x.approved);
  screenEl.innerHTML = `
    <div class="page-head">
      <button class="btn-link" data-act="go" data-screen="runtime">All runtime sessions</button>
      <h1>${esc(s.title)}</h1>
      <div class="row">${originTag(fixture)} ${sPill(s.status)} <span class="mono muted">${esc(s.id)}</span></div>
      ${s.status_reason ? `<p class="mt-xs muted">${esc(s.status_reason)}</p>` : ''}
    </div>
    ${pending ? `<div class="card focus-card"><h3>Approval needed</h3><p>The model proposed <b>${esc(pending.tool)}</b> with the arguments below. It will run only after you approve this exact decision.</p>
      <pre class="diff">${esc(pretty(pending.args))}</pre>
      <div class="row row-end mt"><button class="btn btn-secondary" data-act="go" data-screen="runtime" data-param="${esc(pending.id)}">Why this action?</button><button class="btn btn-primary" data-act="rtApprove" data-id="${esc(pending.id)}">Approve and run</button></div></div>` : ''}
    <div class="grid-2 mt">
      <div class="card"><h3>Request</h3><p class="quote">${esc(s.instruction)}</p>
        ${kv([['Working folder', `<span class="mono">${esc(s.working_dir)}</span>`], ['Protected paths', esc((s.protected_paths || []).join(', ') || 'none')],
          ['Steps / tool calls', `${esc(s.steps_used)} / ${esc(s.tool_calls_used)}`], ['Provider', esc(`${s.provider_config.provider} · ${s.provider_config.model}`)],
          ['Model digest', `<span class="mono">${esc(s.provider_config.model_digest || 'Unavailable')}</span>`]])}
        ${s.status === 'paused' ? '<div class="row row-end mt"><button class="btn btn-secondary btn-small" data-act="rtResume" data-id="' + esc(s.id) + '">Resume</button></div>' : ''}
      </div>
      <div class="card"><h3>Proposal attempts</h3>
        <table class="table"><thead><tr><th>Step</th><th>Attempt</th><th>Result</th></tr></thead><tbody>
        ${d.attempts.map(a => `<tr><td>${esc(a.step)}</td><td>${esc(a.attempt + 1)}</td><td>${a.valid ? pill('green', 'Accepted') : `${pill('red', 'Rejected')}<div class="small muted">${esc((a.errors || []).map(e => `${e.code}: ${e.message}`).join('; '))}</div>`}</td></tr>`).join('') || '<tr><td colspan="3" class="muted">None</td></tr>'}
        </tbody></table></div>
    </div>
    <h2 class="section-title">Decisions and actions</h2>
    <div class="card"><table class="table">
      <thead><tr><th>Step</th><th>Decision</th><th>Proposed</th><th>Execution</th><th></th></tr></thead>
      <tbody>${d.decisions.map(x => `
        <tr><td>${esc(x.step)}</td><td><span class="mono">${esc(short(x.id))}</span>${x.kind === 'retrospective' ? ` ${pill('amber', 'Retrospective')}` : ''}</td>
          <td>${x.kind === 'action' ? `<b>${esc(x.tool)}</b> <span class="mono muted">${esc(JSON.stringify(x.args))}</span>` : `<span class="muted">${x.kind === 'response' ? 'Final text response (no action)' : esc(x.kind)}</span>`}</td>
          <td>${x.kind === 'action' ? aPill(x.state) : ''}</td>
          <td><button class="btn btn-secondary btn-small" data-act="go" data-screen="runtime" data-param="${esc(x.action_id || x.id)}">${x.kind === 'action' ? 'Why this action?' : 'Inspect decision'}</button></td></tr>`).join('') || '<tr><td colspan="5" class="muted">No decisions recorded</td></tr>'}
      </tbody></table></div>
    ${d.findings.length ? `<h2 class="section-title">Findings</h2>${d.findings.map(findingCard).join('')}` : ''}
    <h2 class="section-title">Model requests</h2>
    <div class="card"><table class="table"><thead><tr><th>Request</th><th>Purpose</th><th>Model</th><th>Structured output</th><th>Status</th><th>Duration</th></tr></thead><tbody>
      ${d.requests.map(r => `<tr><td class="mono">${esc(short(r.id))}</td><td>${esc(r.purpose)}</td><td class="small">${esc(r.model)}</td><td class="small">${esc(r.structured_output_mode)}</td><td>${r.status === 'ok' ? pill('green', 'ok') : pill('red', r.status)}${r.error ? `<div class="small muted">${esc(r.error)}</div>` : ''}</td><td class="small">${r.finished_at ? ((r.finished_at - r.started_at) / 1000).toFixed(1) + ' s' : '—'}</td></tr>`).join('')}
    </tbody></table></div>`;
}
actions.rtApprove = async d => {
  const r = await api('POST', `/api/ui/runtime/decisions/${d.id}/approve`);
  showToast(`Approved. Session ${r.status.status}.`, 'success');
  route();
};
actions.rtResume = async d => {
  const r = await api('POST', `/api/ui/runtime/sessions/${d.id}/resume`);
  showToast(`Session ${r.status.status}`, 'info');
  route();
};

function findingCard(f) {
  const inferred = f.basis === 'inferred';
  return `<div class="card check-card"><div class="check-head">
      ${inferred ? lbl(L.inferred) : pill('blue', 'Deterministic check')}
      <span class="check-name">${esc(f.check_type.replace(/_/g, ' '))}</span><span class="spacer"></span><span class="small muted">${esc(when(f.created_at))}</span></div>
    <p class="mt-xs">${esc(f.summary)}</p>
    ${f.excerpts && f.excerpts.length ? `<div class="evidence"><b>Excerpts</b>${list(f.excerpts)}</div>` : ''}
    ${inferred ? '<p class="small muted">Generated by a model after the fact. It is a hypothesis, not a record, and it did not change any stored record.</p>' : ''}</div>`;
}

// =====================================================================================================
// Why this action?
// =====================================================================================================
async function renderWhy(id) {
  const path = id.startsWith('act_') ? `actions/${id}` : `decisions/${id}`;
  const b = await api('GET', `/api/ui/runtime/${path}/why`);
  const d = b.decision;
  const p = d.proposal || {};
  const a = b.action;
  const req = b.model_request || {};
  const items = b.context_items || [];
  const cited = items.filter(i => i.labels.includes(L.cited));
  const finalState = b.action_states.length ? b.action_states[b.action_states.length - 1].state : null;
  const ctx = i => `<div class="ctx-item ${i.labels.includes(L.cited) ? 'cited' : ''} ${i.included ? '' : 'unavail'} ${/lower|retrieved|tool_result/.test(i.trust) ? 'lowtrust' : ''}">
      <div class="ctx-head"><b>${esc(i.label)}</b> ${i.labels.map(lbl).join(' ')} <span class="pill">${esc(i.trust)}</span> <span class="muted">${esc(i.source)}</span>
        ${i.is_excerpt ? pill('amber', `Excerpt: ${i.excerpt_range}`) : ''}${i.redaction_applied ? pill('amber', 'Redacted in audit copy') : ''}</div>
      <div class="mono muted mt-xs">${esc(i.id)}</div>
      <pre class="ctx-body">${esc(i.content_redacted)}</pre></div>`;

  screenEl.innerHTML = `
    <div class="page-head">
      <button class="btn-link" data-act="go" data-screen="runtime" data-param="${esc(d.session_id)}">Back to session</button>
      <h1>${a ? 'Why this action?' : 'Decision record'}</h1>
      <div class="row">${originTag(b.is_fixture)} ${a ? `<b>${esc(a.tool)}</b> ${aPill(finalState)}` : pill('', d.kind === 'response' ? 'Text response, no action' : 'Not executed')}
        <span class="mono muted">decision ${esc(d.id)}${a ? ` · action ${esc(a.id)}` : ''}</span></div>
    </div>
    <div class="notice">${esc(b.notice)}</div>
    ${b.is_fixture ? '<div class="note note-warn mt">This session used a recorded fixture: the "model" responses were written in advance for testing. They are not output from a live model.</div>' : ''}
    ${b.capture_gaps.length ? `<div class="gaps mt"><h3>Missing or limited context</h3><ul>${b.capture_gaps.map(g => `<li>${esc(g)}</li>`).join('')}</ul></div>` : ''}

    <div class="why-sec mt"><h3>Original request</h3><p class="quote">${esc(b.original_request)}</p></div>

    <div class="why-grid mt">
      <div class="why-sec"><h3>Instructions and evidence ${lbl(L.available)}</h3>
        <p class="small muted">Everything listed in the context snapshot for this step. Items marked ${esc(L.unavailable)} existed in the session but were not sent to the model at this step.</p>
        ${items.map(ctx).join('') || '<p class="muted">No context items recorded.</p>'}
        ${b.snapshot && b.snapshot.compaction && b.snapshot.compaction.length ? `<p class="small muted mt">Compaction: ${esc(pretty(b.snapshot.compaction))}</p>` : ''}
      </div>
      <div class="stack">
        <div class="why-sec"><h3>${lbl(L.cited)} References the model chose to cite</h3>
          ${cited.length ? cited.map(i => `<div class="ctx-head mt-xs"><b>${esc(i.label)}</b><span class="muted">${esc(i.source)}</span></div>`).join('') : '<p class="muted">The model cited nothing.</p>'}
          <p class="small muted mt">A citation shows what the model referred to, not what caused its output. Reference validation only confirms the cited IDs existed and were sent; it does not check that they support the action.</p></div>
        <div class="why-sec"><h3>${lbl(L.rationale)} Pre-action justification</h3>
          <p class="quote">${esc(p.stated_rationale || '')}</p>
          ${kv([['Assumptions', list(p.assumptions)], ['Uncertainty', list(p.uncertainty)], ['Alternatives', list(p.alternatives_considered)], ['Expected effect', esc(p.expected_effect || '')], ['Needs clarification', p.needs_clarification ? 'Yes' : 'No']])}
          <p class="small muted mt">Written by the model before execution. It is a concise justification, not a transcript of internal computation.</p>
          ${req.provider_reasoning_redacted ? `<details class="mt"><summary class="small"><b>Provider-exposed reasoning</b> (returned by the provider, optional, may be incomplete)</summary><pre class="ctx-body">${esc(pretty(req.provider_reasoning_redacted))}</pre></details>` : ''}
        </div>
        <div class="why-sec"><h3>Proposed action</h3>
          ${d.kind === 'action' ? `<b>${esc(d.tool)}</b><pre class="diff">${esc(pretty(d.args))}</pre>` : `<p class="muted">No tool action. Response text: ${esc(p.response_text || '')}</p>`}
          ${kv([['Approval', b.approval ? `Approved by ${esc(b.approval.approved_by)}` : d.approval_required ? 'Required, not given' : 'Not required by policy'], ['Arguments hash', `<span class="mono">${esc(d.args_sha256 || '—')}</span>`]])}
        </div>
      </div>
    </div>

    <div class="why-grid mt">
      <div class="why-sec"><h3>Actual execution ${lbl(L.tool)}</h3>
        ${a ? `<ul class="timeline">${b.action_states.map(s => `<li><b>${esc(s.state)}</b> <span class="muted small">${esc(clock(s.at))}</span>${s.detail ? `<div class="small muted">${esc(pretty(s.detail))}</div>` : ''}</li>`).join('')}</ul>
          ${b.result ? `<p class="mt-xs">${b.result.status === 'succeeded' ? status('ok', 'Tool reported success') : status('bad', 'Tool reported failure')}</p>
            ${b.result.error ? `<p class="small">${esc(b.result.error)}</p>` : ''}<pre class="diff">${esc(pretty(b.result.output_redacted))}</pre>`
            : '<p class="muted">No result recorded.</p>'}
          ${a.args_sha256 !== d.args_sha256 ? '<div class="note note-warn">Executed arguments differ from the approved decision.</div>' : ''}` : '<p class="muted">Nothing was executed for this decision.</p>'}
      </div>
      <div class="why-sec"><h3>Observed changes ${lbl(L.observed)}</h3>
        <p class="small muted">Measured by AgentTracker comparing file hashes before and after, separately from what the tool reported.</p>
        ${b.verifications.map(v => `<p class="small"><b>${esc(v.method === 'manifest_diff' ? 'Before/after comparison' : 'Reconciliation')}</b>: ${esc(v.summary)}</p>`).join('')}
        ${b.observed_changes.length ? `<table class="table"><thead><tr><th>Path</th><th>Change</th><th>After hash</th></tr></thead><tbody>${b.observed_changes.map(c => `<tr><td class="mono">${esc(c.rel_path)}</td><td>${esc(c.change_kind)}</td><td class="mono muted">${esc(short(c.after_sha256 ? 'sha_' + c.after_sha256 : ''))}</td></tr>`).join('')}</tbody></table>` : `<p class="muted">${a ? 'No file changes observed.' : esc(L.unavailable)}</p>`}
      </div>
    </div>

    ${b.findings.length ? `<h2 class="section-title">Contradiction checks</h2>${b.findings.map(findingCard).join('')}` : '<p class="muted mt">No deterministic check flagged this decision.</p>'}
    <div class="row mt"><button class="btn btn-secondary btn-small" data-act="rtAnalyze" data-id="${esc(d.id)}">Request inferred analysis from the model</button>
      <span class="small muted">Adds a finding labelled "${esc(L.inferred)}". Records are not modified.</span></div>

    ${b.later_decisions.length ? `<h2 class="section-title">Later decisions and corrections</h2><div class="card"><ul class="timeline">${b.later_decisions.map(x => `<li>${x.revises_decision_id === d.id ? pill('amber', 'Revises this decision') : pill('', `Step ${x.step}`)} <span class="mono">${esc(short(x.id))}</span> ${esc(x.kind)}<div class="small muted">${esc((x.proposal && x.proposal.stated_rationale) || '')}</div></li>`).join('')}</ul></div>` : ''}

    <h2 class="section-title">Test this influence</h2>
    <div class="card">
      <p class="small muted">Replays this decision's request several times with the original context (baseline) and with one item removed or replaced (modified), then compares the proposed actions. No tools are executed. Results show behavioural sensitivity under these conditions only, not proof of cause. Seeds are passed to the provider but do not guarantee reproducible output.</p>
      <div class="form-grid">
        <div class="field"><label class="field-label" for="inv-item">Context item</label><select id="inv-item">${items.filter(i => i.included).map(i => `<option value="${esc(i.id)}">${esc(i.label)}: ${esc(i.source)}</option>`).join('')}</select></div>
        <div class="field"><label class="field-label" for="inv-mod">Modification</label><select id="inv-mod"><option value="remove">Remove the item</option><option value="replace">Replace its text</option></select></div>
        <div class="field"><label class="field-label" for="inv-n">Trials per arm</label><input id="inv-n" type="number" value="5" min="1" max="25"></div>
      </div>
      <div class="field"><label class="field-label" for="inv-text">Replacement text (for Replace)</label><textarea id="inv-text"></textarea></div>
      <div class="row row-end"><button class="btn btn-primary" data-act="rtInvestigate" data-id="${esc(d.id)}" ${d.kind === 'action' ? '' : 'disabled'}>Run investigation</button></div>
      ${b.investigations.length ? `<h4 class="mt">Previous investigations</h4>${b.investigations.map(v => `<div class="row mt-xs">${pill(v.status === 'completed' ? 'blue' : 'amber', v.status)} <button class="btn-link" data-act="go" data-screen="runtime" data-param="${esc(v.id)}">${esc(short(v.id))}</button><span class="small muted">${esc(v.summary || '')}</span></div>`).join('')}` : ''}
    </div>

    <details class="history"><summary>Recorded model request (redacted audit copy)</summary>
      ${kv([['Request', `<span class="mono">${esc(req.id)}</span>`], ['Provider / endpoint', esc(`${req.provider} · ${req.endpoint}`)], ['Model', esc(req.model)], ['Digest', `<span class="mono">${esc(req.model_digest || 'Unavailable')}</span>`],
        ['Settings', `<span class="mono">${esc(JSON.stringify(req.settings))}</span>`], ['Runtime version', esc(req.runtime_version)], ['Structured output', esc(req.structured_output_mode)],
        ['Exact payload hash', `<span class="mono">${esc(req.payload_redacted && req.payload_redacted.exact_messages_sha256)}</span>`],
        ['Redaction changed payload', req.payload_redacted && req.payload_redacted.redaction_changed_payload ? 'Yes: this copy is modified, not exact' : 'No'], ['Replay capture', req.replay_captured ? 'Encrypted exact copy stored' : 'Not captured']])}
      <pre class="diff mt">${esc(pretty(req.payload_redacted && req.payload_redacted.messages))}</pre>
      <h4 class="mt">Raw response (redacted)</h4><pre class="diff">${esc(pretty(req.response_redacted))}</pre>
      ${b.proposal_attempts.length > 1 ? `<h4 class="mt">All attempts at this step</h4>${b.proposal_attempts.map(t => `<div class="ctx-item"><div class="ctx-head">Attempt ${esc(t.attempt + 1)} ${t.valid ? pill('green', 'Accepted') : pill('red', 'Rejected')}</div>${(t.errors || []).length ? `<div class="small">${esc(t.errors.map(e => `${e.code}: ${e.message}`).join('; '))}</div>` : ''}<pre class="ctx-body">${esc(t.raw_redacted)}</pre></div>`).join('')}` : ''}
    </details>`;
}
actions.rtAnalyze = async d => {
  showToast('Asking the configured model for an inferred analysis…', 'info');
  await api('POST', `/api/ui/runtime/decisions/${d.id}/analyze`);
  route();
};
actions.rtInvestigate = async d => {
  const mod = $('#inv-mod').value;
  const modification = mod === 'replace' ? { kind: 'replace', text: $('#inv-text').value } : { kind: 'remove' };
  showToast('Running replay trials. No tools will be executed.', 'info');
  const r = await api('POST', '/api/ui/runtime/investigations', {
    decision_id: d.id, context_item_id: $('#inv-item').value, modification, trials_per_arm: Number($('#inv-n').value) || 5,
  });
  navigate('runtime', r.investigation.id);
};

// =====================================================================================================
// Investigation result
// =====================================================================================================
async function renderInvestigation(id) {
  const r = await api('GET', `/api/ui/runtime/investigations/${encodeURIComponent(id)}`);
  const v = r.investigation;
  const c = v.counts || {};
  const bar = (name, arm, cls) => {
    const x = c[arm];
    if (!x) return '';
    return `<div class="count-bar"><span>${esc(name)}</span><meter class="${cls}" min="0" max="${x.trials}" value="${x.matches_original}"></meter><span><b>${esc(x.matches_original)}/${esc(x.trials)}</b></span></div>
      <div class="small muted">Valid proposals: ${esc(x.valid)}/${esc(x.trials)} · Provider errors: ${esc(x.provider_errors)} · Outcomes: ${esc(Object.entries(x.by_outcome || {}).map(([k, n]) => `${k} ×${n}`).join(', '))}</div>`;
  };
  screenEl.innerHTML = `
    <div class="page-head">
      <button class="btn-link" data-act="go" data-screen="runtime" data-param="${esc(v.decision_id)}">Back to decision</button>
      <h1>Influence investigation</h1>
      <div class="row">${pill(v.status === 'completed' ? 'blue' : 'amber', v.status.replace(/_/g, ' '))} ${pill('', `Replay fidelity: ${v.replay_fidelity.replace(/_/g, ' ')}`)} ${v.confounded ? pill('red', 'Confounded') : ''} <span class="mono muted">${esc(v.id)}</span></div>
    </div>
    <div class="notice">${esc(r.label)}</div>
    ${v.summary ? `<div class="card mt"><h3>Result</h3><p>${esc(v.summary)}</p>${bar('Baseline', 'baseline', '')}${bar('Modified context', 'modified', 'mod')}
      <p class="small muted mt">Counts show how many trials proposed the same normalized action as the original decision. Comparison method: ${esc(v.comparison_method)}.</p></div>` : ''}
    ${v.confounded ? `<div class="gaps mt"><h3>Confound detected</h3><p class="small">${esc(pretty(v.confound_detail))}</p></div>` : ''}
    <div class="gaps mt"><h3>Limitations</h3><ul>${(v.limitations || []).map(l => `<li>${esc(l)}</li>`).join('')}</ul></div>
    <h2 class="section-title">Trials (${r.trials.length})</h2>
    <div class="card"><table class="table"><thead><tr><th>Arm</th><th>#</th><th>Seed</th><th>Valid</th><th>Normalized action</th></tr></thead><tbody>
      ${r.trials.map(t => `<tr><td>${esc(t.arm)}</td><td>${esc(t.trial_no)}</td><td>${esc(t.seed ?? '—')}</td><td>${t.valid ? 'yes' : 'no'}</td><td class="mono">${esc(pretty(t.normalized_action))}${t.error ? `<div class="small muted">${esc(t.error)}</div>` : ''}</td></tr>`).join('')}
    </tbody></table></div>`;
}
