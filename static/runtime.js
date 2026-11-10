// Houndbit decision runtime UI. Loaded after app.js and reuses its helpers
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
// Model names can be long ("lmstudio-community/Qwen2.5-Coder-7B-Instruct-GGUF/qwen2.5-coder-7b-instruct-q4_k_m.gguf").
// Show a shorter name; the full one is on hover and copied on click.
function shortModel(m) {
  if (!m) return '—';
  let n = String(m).split(/[\\/]/).filter(Boolean).pop() || String(m);
  n = n.replace(/\.gguf$/i, '').replace(/[-_.@](q\d[\w]*|iq\d[\w]*|fp16|bf16|f16|gguf)$/i, '');
  return n.length > 32 ? n.slice(0, 31) + '…' : n;
}
const modelCell = m => (m
  ? `<button type="button" class="model-name" data-act="rtCopy" data-text="${esc(m)}" data-keep-enabled="1" title="${esc(m)} (click to copy)">${esc(shortModel(m))}</button>`
  : '—');
actions.rtCopy = async d => {
  try { await navigator.clipboard.writeText(d.text); showToast(`Copied: ${d.text}`, 'success'); }
  catch (_) { window.prompt('Copy the full model name:', d.text); }
};
const notRecorded = '<span class="muted">Not recorded</span>';

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
  if (p.startsWith('conv_')) return renderConversation(p);
  return renderRuntimeHome();
};

// =====================================================================================================
// Home: tabs
// =====================================================================================================
async function renderRuntimeHome() {
  const tabs = [['sessions', 'Sessions'], ['start', 'Start a task'], ['demos', 'Demonstrations'], ['chats', 'Chat recordings'], ['provider', 'Model provider']];
  screenEl.innerHTML = `
    <div class="page-head">
      <h1>Agent Runtime</h1>
      <p class="lead">Run a local model through Houndbit's instrumented runtime. Before any tool runs, the model must submit a structured decision proposal, which is validated and saved. You can then inspect what the model was given, its stated rationale, and what actually happened.</p>
    </div>
    <div class="tabs" role="tablist">${tabs.map(([k, t]) => `<button class="tab ${RT.tab === k ? 'active' : ''}" role="tab" data-act="rtTab" data-tab="${k}">${t}</button>`).join('')}</div>
    <div id="rt-body"></div>`;
  const body = $('#rt-body');
  if (RT.tab === 'sessions') return renderSessions(body);
  if (RT.tab === 'start') return renderStart(body);
  if (RT.tab === 'demos') return renderDemos(body);
  if (RT.tab === 'chats') return renderChats(body);
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
  const isFixture = s => !!(s.provider_config && s.provider_config.is_fixture);
  // Only show the origin column when the rows differ; otherwise say it once above the table.
  const mixed = sessions.some(isFixture) && sessions.some(x => !isFixture(x));
  const allOrigin = mixed ? '' : (isFixture(sessions[0]) ? 'All sessions below used recorded fixtures (not live model output).' : 'All sessions below are runtime sessions.');
  body.innerHTML = `
    <div class="note note-plain mb-sm">These sessions ran through the Houndbit runtime. Externally connected agents (Claude Code, Cursor, Copilot) appear under Live Task and Results and are not mixed in here.${allOrigin ? ` ${esc(allOrigin)}` : ''}</div>
    <div class="card table-wrap"><table class="table rt-sessions">
      <thead><tr><th>Task</th>${mixed ? '<th>Origin</th>' : ''}<th class="col-model">Model</th><th>Status</th><th>Actions</th><th>Findings</th><th>Started</th></tr></thead>
      <tbody>${sessions.map(s => `
        <tr class="clickable" data-act="go" data-screen="runtime" data-param="${esc(s.id)}">
          <td><div class="task-row-title">${esc(s.title)}</div><div class="mono muted">${esc(short(s.id))}</div></td>
          ${mixed ? `<td>${originTag(isFixture(s))}</td>` : ''}
          <td class="col-model">${modelCell(s.provider_config && s.provider_config.model)}</td>
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
    <div class="note note-plain mt">Demonstrations run in new disposable folders under the Houndbit data folder. Recorded fixtures are hand-written, deterministic responses used for UI tests and are labelled "Recorded fixture" everywhere. They are not model output. Live runs show whatever the configured model actually produced.</div>`;
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
        <div class="field"><label class="field-label" for="p-endpoint">Endpoint</label><input id="p-endpoint" type="text" value="${esc(p.endpoint)}"><span class="field-hint">Ollama: http://127.0.0.1:11434/v1 · LM Studio: http://127.0.0.1:1234/v1 (if you leave off /v1, it's added for you)</span></div>
        <div class="field"><label class="field-label" for="p-model">Model</label><input id="p-model" type="text" value="${esc(p.model)}" list="p-models"><datalist id="p-models"></datalist></div>
        <div class="field"><label class="field-label" for="p-key">API key (optional)</label><input id="p-key" type="password" placeholder="${c.has_api_key ? 'Saved (leave blank to keep)' : 'Not required for local servers'}"></div>
        <div class="field"><label class="field-label" for="p-mode">Structured output</label><select id="p-mode">
          ${[['auto', 'Auto (try JSON schema, then JSON object, then prompt only)'], ['json_schema', 'JSON schema'], ['json_object', 'JSON object'], ['prompt_only', 'Prompt only']].map(([v, t]) => `<option value="${v}" ${p.structured_mode === v ? 'selected' : ''}>${t}</option>`).join('')}</select></div>
        ${num('p-temp', 'Temperature', p.temperature)}
        ${num('p-topp', 'Top p', p.top_p)}
        ${num('p-seed', 'Seed', p.seed, 'Seeds do not guarantee reproducible output.')}
        ${num('p-maxtok', 'Max output tokens', p.max_tokens)}
        ${num('p-ctx', 'Context limit (characters)', p.context_limit_chars)}
        ${num('p-ctxtok', 'Context length (tokens)', p.model_context_tokens ?? 8192, "Use the same number as Context Length in LM Studio's Load tab.")}
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
      context_limit_chars: numOr('#p-ctx', 24000), model_context_tokens: numOr('#p-ctxtok', 8192), timeout_secs: numOr('#p-timeout', 300), allow_remote: $('#p-remote').checked },
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
          ['Steps / tool calls', `${esc(s.steps_used)} / ${esc(s.tool_calls_used)}`], ['Provider', `${esc(s.provider_config.provider)} · ${modelCell(s.provider_config.model)}`],
          ['Model digest', `<span class="mono">${esc(s.provider_config.model_digest || 'Unavailable')}</span>`]])}
        ${s.status === 'paused' ? '<div class="row row-end mt"><button class="btn btn-secondary btn-small" data-act="rtResume" data-id="' + esc(s.id) + '">Resume</button></div>' : ''}
        ${RETRYABLE.includes(s.status) ? retryBox(s) : ''}
      </div>
      <div class="card"><h3>Proposal attempts</h3>
        <table class="table"><thead><tr><th>Step</th><th>Attempt</th><th>Result</th></tr></thead><tbody>
        ${d.attempts.map(a => `<tr><td>${esc(a.step)}</td><td>${esc(a.attempt + 1)}</td><td>${a.valid ? pill('green', 'Accepted') : `${pill('red', 'Rejected')}<div class="small muted">${esc((a.errors || []).map(e => `${e.code}: ${e.message}`).join('; '))}</div><details class="mt-xs"><summary class="small">What the model sent back</summary>${(a.raw_redacted || '').trim() ? `<pre class="ctx-body">${esc(a.raw_redacted)}</pre>` : '<p class="small muted">Nothing. The reply was empty.</p>'}</details>`}</td></tr>`).join('') || '<tr><td colspan="3" class="muted">None</td></tr>'}
        </tbody></table></div>
    </div>
    <h2 class="section-title">Decisions and actions</h2>
    <div class="card"><table class="table">
      <thead><tr><th>Step</th><th>Decision</th><th>Proposed</th><th>Execution</th><th></th></tr></thead>
      <tbody>${d.decisions.map(x => `
        <tr><td>${esc(x.step)}</td><td><span class="mono">${esc(short(x.id))}</span>${x.kind === 'retrospective' ? ` ${pill('amber', 'Retrospective')}` : ''}</td>
          <td>${x.kind === 'action' ? `<b>${esc(x.tool)}</b> <span class="mono muted">${esc(JSON.stringify(x.args))}</span>` : `<span class="muted">${x.kind === 'response' ? 'Final text response (no action)' : esc(x.kind)}</span>`}</td>
          <td>${x.kind === 'action' ? aPill(x.state) : ''}</td>
          <td class="why-cell">${x.kind === 'action'
            ? `<button class="btn btn-primary btn-small" data-act="go" data-screen="runtime" data-param="${esc(x.action_id || x.id)}">Why this action?</button>`
            : `<button class="btn btn-secondary btn-small" data-act="go" data-screen="runtime" data-param="${esc(x.id)}">Inspect decision</button>`}</td></tr>`).join('') || '<tr><td colspan="5" class="muted">No decisions recorded</td></tr>'}
      </tbody></table></div>
    ${d.findings.length ? `<h2 class="section-title">Findings</h2>${d.findings.map(findingCard).join('')}` : ''}
    <h2 class="section-title">Model requests</h2>
    <div class="card"><table class="table"><thead><tr><th>Request</th><th>Purpose</th><th class="col-model">Model</th><th>Structured output</th><th>Prompt size</th><th>Status</th><th>Duration</th></tr></thead><tbody>
      ${d.requests.map(r => `<tr><td class="mono">${esc(short(r.id))}</td><td>${esc(r.purpose)}</td><td class="col-model">${modelCell(r.model)}</td><td class="small">${esc(r.structured_output_mode)}</td><td class="small">${promptSize(r, d.model_context_tokens)}</td><td>${r.status === 'ok' ? pill('green', 'ok') : pill('red', r.status)}${r.error ? `<div class="small muted">${esc(r.error)}</div>` : ''}${replyDetails(r)}</td><td class="small">${r.finished_at ? ((r.finished_at - r.started_at) / 1000).toFixed(1) + ' s' : '—'}</td></tr>`).join('')}
    </tbody></table></div>`;
  if (RETRYABLE.includes(s.status)) fillRetryModels(s.provider_config && !s.provider_config.is_fixture ? s.provider_config.model : '');
}

// ---- Retry with this model ----
const RETRYABLE = ['paused', 'completed', 'failed'];
function retryBox(s) {
  return `<div class="mt" style="border-top:1px solid #eceae4;padding-top:12px">
    <label class="field-label" for="rt-retry-model"><b>Try a different model</b></label>
    <p class="small muted mt-xs">Runs the same request again with a different model. Your old run stays as it is.</p>
    <div class="mt-xs" id="rt-retry-pick"><span class="small muted"><span class="spinner"></span> Getting your models...</span></div>
    <div class="row row-end mt-xs"><button class="btn btn-primary btn-small" data-act="rtRetry" data-id="${esc(s.id)}">Retry with this model</button></div>
  </div>`;
}
async function fillRetryModels(current) {
  if (!$('#rt-retry-pick')) return;
  let models = [];
  try { models = (await api('GET', '/api/ui/runtime/provider/models')).models || []; } catch (_) { models = []; }
  const at = $('#rt-retry-pick');
  if (!at) return; // the user moved on while the list was loading
  if (models.length) {
    // Start on a model other than the one this run used, since that's the point of retrying.
    const pick = models.find(m => m !== current) || models[0];
    at.innerHTML = `<select id="rt-retry-model" style="width:100%">${models.map(m => `<option value="${esc(m)}" ${m === pick ? 'selected' : ''}>${esc(m)}${m === current ? ' (used for this run)' : ''}</option>`).join('')}</select>`;
  } else {
    at.innerHTML = `<input id="rt-retry-model" type="text" style="width:100%" placeholder="Type a model name, e.g. qwen2.5-coder-7b-instruct" value="${esc(current || '')}">
      <div class="small muted mt-xs">Couldn't get the model list from your model app, so type the name instead.</div>`;
  }
}
actions.rtRetry = async d => {
  const el = $('#rt-retry-model');
  const model = el ? el.value.trim() : '';
  if (!model) { showToast('Pick a model to try first.', 'warn'); return; }
  showToast(`Starting a new run with ${model}. This can take a minute.`, 'info');
  const r = await api('POST', `/api/ui/runtime/sessions/${encodeURIComponent(d.id)}/retry`, { model });
  if (r.run_error) showToast(`The new run started but hit a problem: ${r.run_error}`, 'warn');
  navigate('runtime', r.start.session_id);
};
// Rough guide: about 4 characters per token. The context length comes from the provider settings
// (falls back to 8,192, LM Studio's usual default Context Length).
const DEFAULT_CONTEXT_TOKENS = 8192;
// "an 8,192", "an 11,000", "a 32,768": the article follows how the number is said aloud.
const ctxArticle = n => { const lead = n.toLocaleString('en-US').split(',')[0]; return lead.startsWith('8') || lead === '11' || lead === '18' ? 'an' : 'a'; };
function promptSize(r, contextTokens) {
  const ctx = Number(contextTokens) > 0 ? Number(contextTokens) : DEFAULT_CONTEXT_TOKENS;
  const chars = Number(r.prompt_chars);
  if (r.prompt_chars == null || !Number.isFinite(chars)) return '<span class="muted">—</span>';
  const tokens = Math.round(chars / 4 / 100) * 100;
  const shown = tokens > 0 ? tokens : Math.max(1, Math.round(chars / 4));
  const maxTok = Number(r.max_tokens) || 0;
  const tooLong = Math.round(chars / 4) + maxTok > ctx;
  return `about ${shown.toLocaleString('en-US')} tokens (${chars.toLocaleString('en-US')} characters)` +
    (tooLong ? `<div class="small mt-xs" style="color:#92400e;background:#fffbeb;border-radius:6px;padding:4px 8px">May be too long for ${ctxArticle(ctx)} ${ctx.toLocaleString('en-US')}-token context. In LM Studio, raise Context Length, then set the same number on the Model provider tab here.</div>` : '');
}
function replyDetails(r) {
  if (r.status === 'sent') return '';
  const resp = r.response_redacted;
  const content = resp && typeof resp === 'object' ? resp.content : resp;
  const text = typeof content === 'string' ? content : (content == null ? '' : JSON.stringify(content));
  return `<details class="mt-xs"><summary class="small">What came back</summary>${text.trim() ? `<pre class="ctx-body">${esc(text)}</pre>` : '<p class="small muted">Nothing came back.</p>'}</details>`;
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

const FINDING_TITLES = {
  expected_success_but_failed: 'Expected it to work, but it failed',
  claimed_success_but_failed: 'Said it worked, but it failed',
};
const findingTitle = t => FINDING_TITLES[t] || t.replace(/_/g, ' ');

function findingCard(f) {
  const inferred = f.basis === 'inferred';
  return `<div class="card check-card"><div class="check-head">
      ${inferred ? lbl(L.inferred) : pill('blue', 'Deterministic check')}
      <span class="check-name">${esc(findingTitle(f.check_type))}</span><span class="spacer"></span><span class="small muted">${esc(when(f.created_at))}</span></div>
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

    <div class="why-summary card mt">
      <h2>The short version</h2>
      <p class="small muted">What was recorded for this step. Anything missing says “Not recorded”; nothing here is filled in by guesswork.</p>
      <dl class="why-list">
        <dt>Instruction</dt><dd>${(b.original_request || '').trim() ? `<span class="quote-inline">${esc(b.original_request)}</span>` : notRecorded}</dd>
        <dt>Cited evidence</dt><dd>${cited.length ? cited.map(i => `<span class="pill">${esc(i.label)}</span>`).join(' ') : notRecorded}</dd>
        <dt>Stated rationale</dt><dd>${(p.stated_rationale || '').trim() ? esc(p.stated_rationale) : notRecorded}</dd>
        <dt>Resulting change</dt><dd>${b.observed_changes.length
          ? b.observed_changes.slice(0, 6).map(c => `<code>${esc(c.rel_path)}</code> ${esc(c.change_kind)}`).join('<br>') + (b.observed_changes.length > 6 ? `<br><span class="small muted">and ${b.observed_changes.length - 6} more</span>` : '')
          : a && b.verifications.length ? '<span class="muted">No file changes were observed.</span>' : notRecorded}</dd>
      </dl>
    </div>

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
        <p class="small muted">Measured by Houndbit comparing file hashes before and after, separately from what the tool reported.</p>
        ${b.verifications.map(v => `<p class="small"><b>${esc(v.method === 'manifest_diff' ? 'Before/after comparison' : 'Reconciliation')}</b>: ${esc(v.summary)}</p>`).join('')}
        ${b.observed_changes.length ? `<table class="table"><thead><tr><th>Path</th><th>Change</th><th>After hash</th></tr></thead><tbody>${b.observed_changes.map(c => `<tr><td class="mono">${esc(c.rel_path)}</td><td>${esc(c.change_kind)}</td><td class="mono muted">${esc(short(c.after_sha256 ? 'sha_' + c.after_sha256 : ''))}</td></tr>`).join('')}</tbody></table>` : `<p class="muted">${a ? 'No file changes observed.' : esc(L.unavailable)}</p>`}
      </div>
    </div>

    ${b.findings.length ? `<h2 class="section-title">Contradiction checks</h2>${b.findings.map(findingCard).join('')}` : '<p class="muted mt">No deterministic check flagged this decision.</p>'}
    <div class="row mt"><button class="btn btn-secondary btn-small" data-act="rtAnalyze" data-id="${esc(d.id)}">Request inferred analysis from the model</button>
      <span class="small muted">Adds a finding labelled "${esc(L.inferred)}". Records are not modified.</span></div>

    ${b.later_decisions.length ? `<h2 class="section-title">Later decisions and corrections</h2><div class="card"><ul class="timeline">${b.later_decisions.map(x => `<li>${x.revises_decision_id === d.id ? pill('amber', 'Revises this decision') : pill('', `Step ${x.step}`)} <span class="mono">${esc(short(x.id))}</span> ${esc(x.kind)}<div class="small muted">${esc((x.proposal && x.proposal.stated_rationale) || '')}</div></li>`).join('')}</ul></div>` : ''}

    <h2 class="section-title">Would it change its mind?</h2>
    <div class="card">
      <p class="small">Ask the model the same question again, a few times, and see whether it picks the same action. You can ask with nothing changed (does the answer hold?), or with one item taken out or rewritten (did that item matter?). Nothing is run on your files.</p>
      <p class="small muted">This shows how this model behaves under these settings. It is good evidence, not proof of why it chose what it did. Seeds are passed to the model server but do not guarantee the same output.</p>
      <div class="form-grid">
        <div class="field"><label class="field-label" for="inv-item">Item (for take out / rewrite)</label><select id="inv-item">${items.filter(i => i.included).map(i => `<option value="${esc(i.id)}">${esc(i.label)}: ${esc(i.source)}</option>`).join('')}</select></div>
        <div class="field"><label class="field-label" for="inv-mod">What to try</label><select id="inv-mod"><option value="keep">Ask again, nothing changed</option><option value="remove">Take this item out</option><option value="replace">Rewrite this item's text</option></select></div>
        <div class="field"><label class="field-label" for="inv-n">How many times to ask</label><input id="inv-n" type="number" value="5" min="1" max="25"></div>
      </div>
      <div class="field"><label class="field-label" for="inv-text">New text (for rewrite)</label><textarea id="inv-text"></textarea></div>
      <div class="row row-end"><button class="btn btn-primary" data-act="rtInvestigate" data-id="${esc(d.id)}" ${d.kind === 'action' ? '' : 'disabled'}>Ask again</button></div>
      ${d.kind === 'action' ? '' : '<p class="small muted">Only steps that proposed an action can be checked this way.</p>'}
      ${b.investigations.length ? `<h4 class="mt">Earlier checks</h4>${b.investigations.map(v => `<div class="row mt-xs">${pill(v.status === 'completed' ? 'blue' : 'amber', v.status)} <button class="btn-link" data-act="go" data-screen="runtime" data-param="${esc(v.id)}">${esc(short(v.id))}</button><span class="small muted">${esc(v.summary || '')}</span></div>`).join('')}` : ''}
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
  const modification = mod === 'replace' ? { kind: 'replace', text: $('#inv-text').value } : { kind: mod };
  if (mod === 'replace' && !modification.text.trim()) { showToast('Type the new text first.', 'error'); return; }
  showToast('Asking the model again. Nothing will be run on your files. This can take a minute.', 'info');
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
      <h1>${v.modification && v.modification.kind === 'keep' ? 'Does the answer hold?' : 'Would it change its mind?'}</h1>
      <div class="row">${pill(v.status === 'completed' ? 'blue' : 'amber', v.status.replace(/_/g, ' '))} ${pill('', `Replay fidelity: ${v.replay_fidelity.replace(/_/g, ' ')}`)} ${v.confounded ? pill('red', 'Confounded') : ''} <span class="mono muted">${esc(v.id)}</span></div>
    </div>
    <div class="notice">${esc(r.label)}</div>
    ${v.summary ? `<div class="card mt"><h3>Result</h3>${plainVerdict(v)}<p class="small">${esc(v.summary)}</p>${bar(v.modification && v.modification.kind === 'keep' ? 'Same question, nothing changed' : 'As it was', 'baseline', '')}${bar('With the change', 'modified', 'mod')}
      <p class="small muted mt">Counts show how many trials proposed the same normalized action as the original decision. Comparison method: ${esc(v.comparison_method)}.</p></div>` : ''}
    ${v.confounded ? `<div class="gaps mt"><h3>Confound detected</h3><p class="small">${esc(pretty(v.confound_detail))}</p></div>` : ''}
    <div class="gaps mt"><h3>Limitations</h3><ul>${(v.limitations || []).map(l => `<li>${esc(l)}</li>`).join('')}</ul></div>
    <h2 class="section-title">Trials (${r.trials.length})</h2>
    <div class="card"><table class="table"><thead><tr><th>Arm</th><th>#</th><th>Seed</th><th>Valid</th><th>Normalized action</th></tr></thead><tbody>
      ${r.trials.map(t => `<tr><td>${esc(t.arm)}</td><td>${esc(t.trial_no)}</td><td>${esc(t.seed ?? '—')}</td><td>${t.valid ? 'yes' : 'no'}</td><td class="mono">${esc(pretty(t.normalized_action))}${t.error ? `<div class="small muted">${esc(t.error)}</div>` : ''}</td></tr>`).join('')}
    </tbody></table></div>`;
}

// A one-line answer in plain words, worked out from the counts. The detailed summary stays below it.
function plainVerdict(v) {
  const c = v.counts || {};
  const b = c.baseline, m = c.modified;
  if (!b || !b.trials) return '';
  const rate = x => x.matches_original / x.trials;
  let text;
  if (!m) {
    const r = rate(b);
    text = r >= 0.8 ? 'Yes, the answer holds: it picked the same action almost every time.'
      : r >= 0.5 ? 'Mostly: it picked the same action more often than not, but not every time.'
        : 'No, the answer is shaky: it often picked something different when asked again.';
  } else if (rate(b) < 0.5) {
    text = "Can't tell: even with nothing changed it rarely repeated the original action, so the change can't be judged.";
  } else if (rate(b) - rate(m) >= 0.4) {
    text = 'Yes, it changed its mind: without that item it usually picked something else, so the item looks like it mattered.';
  } else if (rate(b) - rate(m) >= 0.2) {
    text = 'A little: it picked the original action less often with the change. Try more asks to be surer.';
  } else {
    text = 'No, it made the same choice with or without the change, so that item does not look like it mattered.';
  }
  return `<p><b>${esc(text)}</b>${v.confounded ? ' <span class="small muted">(Some of the same information was still present elsewhere, so this is not a clean test.)</span>' : ''}</p>`;
}

// =====================================================================================================
// Chat recorder (relay): Connections card + "Chat recordings" tab + one conversation
// =====================================================================================================
const RELAY_REASONING = {
  reasoning_content: 'sent by the model server as reasoning_content',
  reasoning: 'sent by the model server as reasoning',
  thinking: 'sent by Ollama as thinking',
  think_tags: 'written by the model inside <think> tags',
  reasoning_items: 'sent by the model server as reasoning items',
};

async function renderRelayCard(el) {
  if (!el) return;
  let r;
  try { r = await api('GET', '/api/ui/runtime/relay'); } catch (e) { el.innerHTML = `<div class="card">${status('bad', e.message)}</div>`; return; }
  const c = r.config, on = r.status.running;
  el.innerHTML = `<div class="card conn-card" id="conn-relay">
    <div class="conn-head">
      <div class="conn-logo logo-manual" aria-hidden="true">Ch</div>
      <div class="spacer">
        <h3>Chat app (through LM Studio or Ollama)</h3>
        <div class="row row-tight">${on ? status('ok', 'Recording') : status('muted', 'Off')} ${pill('amber', 'Experimental')}</div>
        <div class="small muted mt-xs">${esc(r.conversations)} conversation(s), ${esc(r.calls)} message(s) recorded</div>
      </div>
    </div>
    <p class="small">Houndbit sits between your chat app and your model server, records each message and reply (and the model's thinking, when it sends any), and passes everything through unchanged.</p>
    <ol class="small relay-steps">
      <li>Make sure LM Studio's server (Developer tab, Start Server) or Ollama is running.</li>
      <li>Turn the recorder on below.</li>
      <li>In your chat app, change the server address to this one:
        <div class="row row-tight mt-xs"><code class="relay-addr">${esc(r.address)}</code><button class="btn btn-secondary btn-small" data-act="rtCopy" data-text="${esc(r.address)}" data-keep-enabled="1">Copy</button></div>
        <span class="muted">For apps that expect an Ollama address, use <code>${esc(r.ollama_address)}</code> instead.</span></li>
      <li>Chat as usual. Your chats show up under Agent Runtime, Chat recordings.</li>
    </ol>
    ${r.status.error ? `<div class="note note-warn">${esc(r.status.error)}</div>` : ''}
    <div class="row">
      <button class="btn ${on ? 'btn-secondary' : 'btn-primary'}" data-act="relayToggle" data-on="${on ? '0' : '1'}">${on ? 'Turn off' : 'Turn on'}</button>
      <button class="btn btn-secondary" data-act="relayTest" ${on ? '' : 'disabled'}>Check it works</button>
      <button class="btn-link" data-act="relayOpenChats">See recorded chats</button>
    </div>
    <div class="conn-out" id="relay-out"></div>
    <details class="conn-details">
      <summary>Settings and limits</summary>
      <div class="form-grid mt-xs">
        <div class="field"><label class="field-label" for="relay-up">Your model server</label><input id="relay-up" type="text" value="${esc(c.upstream)}"><span class="field-hint">LM Studio: http://127.0.0.1:1234 · Ollama: http://127.0.0.1:11434</span></div>
        <div class="field"><label class="field-label" for="relay-port">Recorder port</label><input id="relay-port" type="number" value="${esc(c.port)}" min="1024" max="65535"><span class="field-hint">1235 unless something else uses it.</span></div>
      </div>
      <div class="row"><button class="btn btn-secondary btn-small" data-act="relaySave">Save settings</button></div>
      <ul class="small muted conn-notes">
        <li>Works with chat apps that let you set the server address (many local chat apps do). Cloud apps such as Claude desktop, ChatGPT or Grok can't use it: their model runs on someone else's servers.</li>
        <li>Thinking text is only recorded when the model actually writes it (for example Qwen3 or DeepSeek-R1). Models like qwen2.5-coder reply without it.</li>
        <li>Secrets that look like keys or passwords are blanked before saving. Recordings stay on this computer, and you can delete them at any time.</li>
        <li>The recorder only listens on this computer (127.0.0.1) and only forwards to a model server on this computer.</li>
        <li>Not yet tested with a live chat app on Windows.</li>
      </ul>
    </details>
  </div>`;
}
window.renderRelayCard = renderRelayCard;

async function relaySaveWith(enabled) {
  const r0 = await api('GET', '/api/ui/runtime/relay');
  const up = $('#relay-up'), port = $('#relay-port');
  const cfg = { enabled, upstream: up ? up.value.trim() : r0.config.upstream, port: port ? Number(port.value) || 1235 : r0.config.port };
  try {
    await api('POST', '/api/ui/runtime/relay', cfg);
  } catch (e) {
    showToast(e.message, 'error');
  }
  await renderRelayCard($('#relay-card'));
}
actions.relayToggle = async d => {
  await relaySaveWith(d.on === '1');
  showToast(d.on === '1' ? 'Chat recorder is on. Paste the address into your chat app.' : 'Chat recorder is off.', 'success');
};
actions.relaySave = async () => {
  const r = await api('GET', '/api/ui/runtime/relay');
  await relaySaveWith(r.config.enabled);
  showToast('Saved.', 'success');
};
actions.relayTest = async () => {
  const out = $('#relay-out');
  out.innerHTML = '<p class="small muted"><span class="spinner"></span> Asking your model server for its model list, through the recorder…</p>';
  try {
    const r = await api('POST', '/api/ui/runtime/relay/test');
    out.innerHTML = `<div class="card card-soft mt">${r.ok ? status('ok', 'The recorder can reach your model server') : status('bad', 'The recorder could not reach your model server')}
      <p class="small mt-xs">${esc(r.detail)}</p>${r.models.length ? `<p class="small muted">Models: ${esc(r.models.join(', '))}</p>` : ''}
      <p class="small muted">This checks the recorder and your model server. It doesn't check your chat app: send one message there and it should appear under Chat recordings.</p></div>`;
  } catch (e) {
    out.innerHTML = `<div class="mt">${status('bad', e.message)}</div>`;
  }
};
actions.relayOpenChats = () => { RT.tab = 'chats'; navigate('runtime'); };

// Small note on a list row when the model wrote tool requests as plain text.
function relayTypedBadge(n) {
  if (!n) return '';
  return `<div class="small mt-xs"><span title="The model wrote a tool request as plain text, so your chat app didn't run it.">${pill('amber', `Tool request typed as text (${n})`)}</span></div>`;
}

async function renderChats(body) {
  const [{ conversations }, rs] = await Promise.all([api('GET', '/api/ui/runtime/relay/conversations'), api('GET', '/api/ui/runtime/relay')]);
  const head = `<div class="note note-plain mb-sm">Chats your chat app sent through the Houndbit chat recorder. ${rs.status.running ? `Recording now at <code>${esc(rs.address)}</code>.` : 'The recorder is off.'} <a href="#connections/relay">Recorder settings</a></div>`;
  if (!conversations.length) {
    body.innerHTML = head + `<div class="card">${emptyState('clipboard', 'No chats recorded yet', 'Turn on the chat recorder in Connections, paste its address into your chat app, and send a message.',
      '<button class="btn btn-primary" data-act="go" data-screen="connections" data-param="relay">Set up the chat recorder</button>')}</div>`;
    return;
  }
  body.innerHTML = head + `
    <div class="card table-wrap"><table class="table">
      <thead><tr><th>Conversation</th><th class="col-model">Model</th><th>Messages</th><th>Thinking</th><th>Tool calls</th><th>Last message</th></tr></thead>
      <tbody>${conversations.map(c => `
        <tr class="clickable" data-act="go" data-screen="runtime" data-param="${esc(c.conversation_id)}">
          <td><div class="task-row-title">${esc(c.title || '(untitled)')}</div>${c.errors ? `<div class="small">${pill('red', `${c.errors} problem(s)`)}</div>` : ''}${relayTypedBadge(c.typed_tool_calls)}</td>
          <td class="col-model">${modelCell(c.model)}</td><td>${esc(c.calls)}</td>
          <td>${c.with_thinking ? pill('blue', `${c.with_thinking}`) : '<span class="muted">none</span>'}</td>
          <td>${c.with_tools ? esc(c.with_tools) : '<span class="muted">none</span>'}</td>
          <td class="small muted">${esc(when(c.last_at))}</td>
        </tr>`).join('')}</tbody></table></div>
    <div class="row row-end mt"><button class="btn btn-danger btn-small" data-act="relayDelete">Delete all chat recordings</button></div>`;
}

const RELAY_ROLES = { system: 'Instructions for the model', developer: 'Instructions for the model', user: 'You', assistant: 'Model', tool: 'Tool result' };
const relayRole = r => RELAY_ROLES[r] || r || 'Message';

// Chat apps send the whole chat again with every message. Keep those repeats out of the way.
function relayEarlier(c) {
  const msgs = c.earlier_messages || [];
  if (!msgs.length) return '';
  return `<details class="mt-xs"><summary class="small muted">Earlier messages in this chat (${msgs.length}), sent again by your chat app</summary>
    ${msgs.map(m => `<div class="mt-xs"><div class="small"><b>${esc(relayRole(m.role))}</b></div><pre class="ctx-body">${esc(m.text || '(empty)')}</pre></div>`).join('')}
  </details>`;
}

function relayTypedWarning(c) {
  if (!c.typed_tool_call) return '';
  const where = [];
  if (c.typed_tool_call_in_reply) where.push('in this reply');
  if (c.typed_tool_calls_in_history) where.push(c.typed_tool_calls_in_history === 1 ? 'in 1 earlier reply' : `in ${c.typed_tool_calls_in_history} earlier replies`);
  return `<div class="note note-warn mt-xs">The model wrote a tool request as plain text (${esc(where.join(' and '))}), so your chat app didn't run it. This model or its LM Studio settings may not support tool calls.</div>`;
}

async function renderConversation(id) {
  const r = await api('GET', `/api/ui/runtime/relay/conversations/${encodeURIComponent(id)}`);
  const calls = r.calls;
  const first = calls[0];
  const turn = (c, i) => `<div class="card mt">
      <div class="row row-tight"><b>Message ${i + 1}</b> ${modelCell(c.model)} <span class="small muted">${esc(when(c.started_at))}${c.finished_at ? ` · ${esc(((c.finished_at - c.started_at) / 1000).toFixed(1))} s` : ''}${c.streamed ? ' · streamed' : ''}</span>
        ${c.error ? pill('red', 'Problem') : ''}</div>
      ${relayEarlier(c)}
      ${(c.latest_user || c.last_user_redacted) ? `<h4 class="mt-xs">You asked</h4><pre class="ctx-body">${esc(c.latest_user || c.last_user_redacted)}</pre>` : ''}
      ${(c.after_user_messages || []).map(m => `<h4 class="mt-xs">${esc(relayRole(m.role))}</h4><pre class="ctx-body">${esc(m.text || '(empty)')}</pre>`).join('')}
      ${c.reasoning_redacted ? `<details class="mt-xs" open><summary class="small"><b>Model's thinking</b> <span class="muted">(${esc(RELAY_REASONING[c.reasoning_source] || 'sent by the model server')}; may be incomplete)</span></summary><pre class="ctx-body">${esc(c.reasoning_redacted)}</pre></details>` : '<p class="small muted mt-xs">No thinking text was sent for this reply.</p>'}
      <h4 class="mt-xs">Reply</h4>${c.reply_redacted ? `<pre class="ctx-body">${esc(c.reply_redacted)}</pre>` : '<p class="small muted">No reply text.</p>'}
      ${c.tool_calls ? `<h4 class="mt-xs">Tool calls the model asked for</h4>${c.tool_calls.map(t => `<div class="small"><b>${esc(t.name || '?')}</b><pre class="diff">${esc(pretty(t.arguments))}</pre></div>`).join('')}<p class="small muted">The recorder only sees the request. Whether your chat app ran it is not known here.</p>` : ''}
      ${relayTypedWarning(c)}
      ${c.error ? `<div class="note note-warn mt-xs">${esc(c.error)}</div>` : ''}
      <details class="history"><summary>Details</summary>
        ${kv([['Sent by', esc(c.client || 'Unknown app')], ['Kind of call', esc(c.api)], ['Messages in the request', esc(c.message_count ?? '—')], ['Finished because', esc(c.finish_reason || '—')],
          ['Tokens', esc(c.usage ? `${c.usage.prompt_tokens ?? c.usage.input_tokens ?? '?'} in, ${c.usage.completion_tokens ?? c.usage.output_tokens ?? '?'} out` : 'Not reported')], ['Server status', esc(c.http_status ?? 'No answer')],
          ['Request hash', `<span class="mono">${esc(short('sha_' + c.request_sha256))}</span>`]])}
        <h4 class="mt-xs">Full request (redacted)</h4><pre class="diff">${esc(pretty(c.request_redacted))}</pre>
      </details>
    </div>`;
  screenEl.innerHTML = `
    <div class="page-head">
      <button class="btn-link" data-act="relayOpenChats">All chat recordings</button>
      <h1>${esc(first.title || 'Chat')}</h1>
      <p class="small muted">Recorded by the chat recorder. Messages are grouped by their first message, so two chats that start the same way may appear together.</p>
    </div>
    ${r.typed_tool_call_count ? `<div class="note note-warn">In ${esc(r.typed_tool_call_count)} of these messages the model wrote a tool request as plain text instead of making a real tool call, so your chat app didn't run those tools. This model or its LM Studio settings may not support tool calls. Look for the notes below.</div>` : ''}
    ${calls.map(turn).join('')}
    <div class="row row-end mt"><button class="btn btn-danger btn-small" data-act="relayDelete" data-conv="${esc(id)}">Delete this conversation</button></div>`;
}
actions.relayDelete = async d => {
  if (!confirm(d.conv ? 'Delete this recorded conversation?' : 'Delete every recorded chat?')) return;
  const r = await api('POST', '/api/ui/runtime/relay/delete', d.conv ? { conversation_id: d.conv } : {});
  showToast(`Deleted ${r.deleted} message(s).`, 'success');
  RT.tab = 'chats';
  navigate('runtime');
};
