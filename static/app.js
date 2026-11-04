// AgentTracker UI — plain JS, no build step. All calls go to the loopback server that served this page.
'use strict';

const $ = (sel, root = document) => root.querySelector(sel);
const $$ = (sel, root = document) => Array.from(root.querySelectorAll(sel));
const screenEl = $('#screen');
let pollTimer = null;
let selectedTaskId = null;
let checkTypes = [];

// ---- helpers -------------------------------------------------------------------------------------
function esc(v) {
  return String(v ?? '').replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
}

async function api(method, path, body) {
  const opts = { method, headers: { 'x-requested-with': 'agenttracker' }, credentials: 'same-origin' };
  if (body !== undefined) {
    opts.headers['content-type'] = 'application/json';
    opts.body = JSON.stringify(body);
  }
  const res = await fetch(path, opts);
  let data = null;
  try { data = await res.json(); } catch (_) { /* empty body */ }
  if (!res.ok) throw new Error((data && data.error) || `Request failed (${res.status})`);
  return data;
}

function toast(msg, isError = false) {
  const t = $('#toast');
  t.textContent = msg;
  t.className = 'toast' + (isError ? ' error' : '');
  clearTimeout(toast._t);
  toast._t = setTimeout(() => t.classList.add('hidden'), isError ? 6000 : 3000);
}

function when(ms) {
  if (!ms) return '—';
  const d = new Date(ms);
  const diff = (Date.now() - ms) / 1000;
  if (diff < 60) return 'just now';
  if (diff < 3600) return `${Math.floor(diff / 60)} min ago`;
  if (diff < 86400) return `${Math.floor(diff / 3600)} h ago`;
  return d.toLocaleString();
}

const LABELS = {
  draft: 'Draft', baselining: 'Taking snapshot', ready: 'Ready', running: 'Agent working', verifying: 'Checking',
  completed: 'Completed', cancelled: 'Cancelled', error: 'Error',
  passed: 'Passed', failed: 'Failed', unable_to_verify: 'Unable to verify',
  implemented: 'Implemented', experimental: 'Experimental', file_only: 'File checks only',
  working: 'Working folder', protected: 'Protected (must not change)', allowed_write: 'Allowed to write',
};
const label = k => LABELS[k] || k;
const pill = (cls, text) => `<span class="badge ${esc(cls)}">${esc(text)}</span>`;
const badge = k => pill(k, label(k));

const CHECK_HELP = {
  originals_unchanged: { name: 'Originals unchanged', desc: 'Protected files must be byte-for-byte identical to the snapshot.', params: { paths: [], allow_additions: true } },
  required_outputs_exist: { name: 'Required outputs exist', desc: 'These files must exist after the task (paths relative to the working folder).', params: { paths: ['output/result.txt'], min_size_bytes: 1 } },
  inputs_preserved_in_output: { name: 'Every input made it to the output', desc: 'Each input file content must appear in the output folder (duplicates counted).', params: { input: 'input', output: 'output' } },
  folder_structure_matches: { name: 'Folder looks right', desc: 'Files in a folder must match the given patterns.', params: { root: 'output', patterns: ['*.txt'], required_patterns: [], allow_other: false } },
  changes_within_scope: { name: 'Changes stayed in bounds', desc: 'Every change must be inside an "Allowed to write" folder.', params: { allowed: [] } },
};
const checkName = t => (CHECK_HELP[t] && CHECK_HELP[t].name) || t;
const VERDICT = {
  passed: ['🎉', 'Everything checks out', 'All required checks passed.'],
  failed: ['🧐', 'Something is off', 'At least one required check failed — see the details below.'],
  unable_to_verify: ['🤔', "We couldn't be sure", 'Some evidence was missing or unreadable, so AgentTracker will not call this a pass.'],
};

function emptyState(icon, text, action = '') {
  return `<div class="empty"><div class="big">${icon}</div><p>${text}</p>${action}</div>`;
}

// ---- router ----------------------------------------------------------------------------------------
const screens = { home, connections, create, live, results, saved, settings };

async function route() {
  clearInterval(pollTimer);
  pollTimer = null;
  const name = (location.hash || '#home').slice(1).split('/')[0];
  const fn = screens[name] || home;
  $$('#nav a').forEach(a => a.classList.toggle('active', a.dataset.screen === (screens[name] ? name : 'home')));
  try {
    await fn();
  } catch (e) {
    screenEl.innerHTML = `<div class="card">${emptyState('😕', esc(e.message))}</div>`;
  }
}
window.addEventListener('hashchange', route);

function go(screen, taskId) {
  if (taskId) selectedTaskId = taskId;
  if (location.hash === '#' + screen) route(); else location.hash = screen;
}

// Delegated click handling: <button data-act="name" data-id="...">.
const actions = {};
screenEl.addEventListener('click', async ev => {
  const el = ev.target.closest('[data-act]');
  if (!el || el.disabled) return;
  const fn = actions[el.dataset.act];
  if (!fn) return;
  el.disabled = true;
  try { await fn(el.dataset, el); } catch (e) { toast(e.message, true); } finally { el.disabled = false; }
});

async function refreshPill() {
  try {
    const s = await api('GET', '/api/ui/state');
    const pill = $('#collect-pill');
    pill.classList.toggle('paused', s.paused);
    $('#collect-text').textContent = s.paused ? 'Collection paused' : 'Collecting';
    return s;
  } catch (e) {
    $('#collect-text').textContent = 'Not connected';
    throw e;
  }
}

// ---- Home ----------------------------------------------------------------------------------------
async function home() {
  const [state, tasks] = await Promise.all([refreshPill(), api('GET', '/api/ui/tasks')]);
  const recent = tasks.slice(0, 6);
  screenEl.innerHTML = `
    <h1>Hi there 👋</h1>
    <p class="lead">AgentTracker watches what your AI agent says it did, then checks the files to see if it really did.</p>
    <div class="card hero">
      <div><h2>New here? Try the demo task</h2><p>It creates three harmless text files and a ready-made task so you can see a full check in two minutes.</p></div>
      <div class="row"><button class="light" data-act="demo">Try the demo</button><button class="light" data-act="goto" data-id="create">Create a task</button></div>
    </div>
    <div class="grid">
      <div class="card stat"><div class="muted small">Tasks</div><div class="num">${tasks.length}</div></div>
      <div class="card stat"><div class="muted small">Agent events recorded</div><div class="num">${state.event_count}</div></div>
      <div class="card stat"><div class="muted small">Waiting to be processed</div><div class="num">${state.queue_pending}</div></div>
      <div class="card stat"><div class="muted small">Collection</div><div class="num">${state.paused ? '⏸️' : '🟢'}</div><div class="small muted">${state.paused ? 'Paused' : 'On'}</div></div>
    </div>
    <div class="card">
      <div class="row between"><h2>Recent tasks</h2><button class="small" data-act="goto" data-id="results">See all results</button></div>
      ${recent.length ? `<ul class="list">${recent.map(t => `
        <li class="clickable row between" data-act="open-task" data-id="${esc(t.id)}">
          <div><b>${esc(t.title)}</b><div class="small muted">${esc(t.connector)} · updated ${when(t.updated_at)}</div></div>
          ${badge(t.lifecycle)}
        </li>`).join('')}</ul>` : emptyState('🌱', 'No tasks yet. Start with the demo or create your own.')}
    </div>
    ${state.hook_binary_exists ? '' : `<div class="card small muted">ℹ️ The hook program (agenttracker-hook) was not found next to the app, so agent connectors can't be installed from this build. File checks still work.</div>`}`;
}

actions.demo = async () => {
  const r = await api('POST', '/api/ui/demo');
  toast('Demo task created — sample files are in ' + r.folder);
  go('live', r.task.id);
};
actions.goto = d => go(d.id);
actions['open-task'] = d => {
  selectedTaskId = d.id;
  go('live', d.id);
};

// ---- Connections ---------------------------------------------------------------------------------
async function connections() {
  const list = await api('GET', '/api/ui/connectors');
  screenEl.innerHTML = `
    <h1>Connections</h1>
    <p class="lead">Connect AgentTracker to the agents you use. We only add our own hook entries and keep a backup of your settings — uninstalling puts everything back.</p>
    ${list.map(c => connectorCard(c)).join('')}
    <div id="conn-output"></div>`;
}

function connectorCard(c) {
  const i = c.info, d = c.detect || {};
  const isManual = i.name === 'manual';
  const caps = (c.capabilities || []).map(x => `<li>${x.available ? '✅' : '➖'} ${esc(x.event_type.replace(/_/g, ' '))} <span class="muted">— ${esc(x.detail)}</span></li>`).join('');
  return `<div class="card">
    <div class="row between">
      <div class="row"><h2>${esc(i.display_name)}</h2>${badge(i.maturity)}${d.hooks_installed ? pill('ok', 'Connected') : ''}</div>
      <div class="row">
        ${isManual ? '' : `<button class="primary small" data-act="conn" data-id="${i.name}/install">${d.hooks_installed ? 'Reinstall' : 'Connect'}</button>
        <button class="small" data-act="conn" data-id="${i.name}/test">Test</button>
        <button class="small danger" data-act="conn" data-id="${i.name}/uninstall" ${d.hooks_installed ? '' : 'disabled'}>Disconnect</button>`}
      </div>
    </div>
    <p class="small muted">${esc(i.status_note)}</p>
    ${isManual ? '' : `<div class="small">
      <div>Agent found: <b>${d.agent_found ? 'yes' + (d.agent_version ? ' (' + esc(d.agent_version) + ')' : '') : 'not detected'}</b></div>
      <div>Settings file: <span class="mono">${esc(d.config_path)}</span> ${d.config_exists ? '' : '<span class="muted">(will be created)</span>'}</div>
      ${(d.notes || []).map(n => `<div class="muted">• ${esc(n)}</div>`).join('')}
    </div>`}
    <details class="small"><summary>What it can see</summary><ul class="list">${caps}</ul><div class="muted">Docs checked: ${esc(i.docs_checked)}</div></details>
  </div>`;
}

actions.conn = async d => {
  const [name, action] = d.id.split('/');
  if (action === 'uninstall' && !confirm('Remove AgentTracker hooks from this agent? Your other settings stay as they are.')) return;
  const r = await api('POST', `/api/ui/connectors/${name}/${action}`);
  let msg;
  if (action === 'test') {
    msg = `<h3>${r.ok ? '✅ Looks good' : '⚠️ Not quite there yet'}</h3><ul class="list">${r.steps.map(s => `<li>${s.ok ? '✅' : '❌'} <b>${esc(s.name)}</b> <span class="muted">${esc(s.detail)}</span></li>`).join('')}</ul>`;
  } else if (action === 'install') {
    msg = `<h3>✅ Connected</h3><p class="small">Updated <span class="mono">${esc(r.config_path)}</span>${r.backup_path ? `. Backup saved to <span class="mono">${esc(r.backup_path)}</span>` : ''}. Added ${r.added.length}, refreshed ${r.updated.length} hook entries.</p><p class="small muted">Restart the agent so it picks up the new hooks.</p>`;
  } else {
    msg = `<h3>👋 Disconnected</h3><p class="small">Method: ${esc(r.method.replace(/_/g, ' '))}. Removed ${r.removed.length} AgentTracker entries.</p>`;
  }
  await connections();
  $('#conn-output').innerHTML = `<div class="card">${msg}</div>`;
};

// ---- Create Task ---------------------------------------------------------------------------------
async function create() {
  const [types, templates] = await Promise.all([api('GET', '/api/ui/check-types'), api('GET', '/api/ui/templates')]);
  checkTypes = types.map(t => t.check_type);
  screenEl.innerHTML = `
    <h1>Create a task</h1>
    <p class="lead">Describe what the agent should do, which folders matter, and how we'll know it worked.</p>
    <div class="card">
      <h2>1. The job</h2>
      <div class="field"><label for="t-title">Title</label><input type="text" id="t-title" placeholder="e.g. Rename holiday photos"></div>
      <div class="field"><label for="t-instr">What you asked the agent <span class="hint">(for your records)</span></label><textarea id="t-instr"></textarea></div>
      <div class="field"><label for="t-conn">Which agent?</label>
        <select id="t-conn"><option value="claude_code">Claude Code</option><option value="cursor">Cursor (experimental)</option><option value="manual">Manual / other — file checks only</option></select></div>
    </div>
    <div class="card">
      <h2>2. Folders</h2>
      <p class="small muted">Use full paths. "Protected" folders must not change; "Allowed to write" is where output may go.</p>
      <div id="folders"></div>
      <button class="small" data-act="add-folder">+ Add folder</button>
    </div>
    <div class="card">
      <h2>3. Checks</h2>
      <p class="small muted">Required checks decide the verdict. With no required checks the result will be "Unable to verify".</p>
      <div id="checks"></div>
      <div class="row">
        <button class="small" data-act="add-check">+ Add check</button>
        ${templates.length ? `<select id="tpl-pick" class="small"><option value="">Add a saved check…</option>${templates.map(t => `<option value="${esc(t.id)}">${esc(t.name)}</option>`).join('')}</select>` : ''}
      </div>
    </div>
    <div class="row"><button class="primary" data-act="create-task">Create task</button><span class="small muted">Next you'll take a snapshot of the folders before the agent starts.</span></div>`;
  addFolderRow('', 'working');
  addFolderRow('', 'protected');
  addCheckRow('originals_unchanged', CHECK_HELP.originals_unchanged.params, true);
  addCheckRow('changes_within_scope', CHECK_HELP.changes_within_scope.params, true);
  const pick = $('#tpl-pick');
  if (pick) pick.addEventListener('change', () => {
    const t = templates.find(x => x.id === pick.value);
    if (t) addCheckRow(t.check_type, t.params, true);
    pick.value = '';
  });
}

function addFolderRow(path, role) {
  const div = document.createElement('div');
  div.className = 'folder-row';
  div.innerHTML = `<input type="text" class="f-path" placeholder="C:\\Users\\me\\Documents\\project" value="${esc(path)}">
    <select class="f-role">${['working', 'protected', 'allowed_write'].map(r => `<option value="${r}" ${r === role ? 'selected' : ''}>${label(r)}</option>`).join('')}</select>
    <button class="small danger" data-act="remove-row">✕</button>`;
  $('#folders').appendChild(div);
}

function addCheckRow(type, params, required) {
  const div = document.createElement('div');
  div.className = 'check-row';
  div.innerHTML = `<div><select class="c-type">${checkTypes.map(t => `<option value="${t}" ${t === type ? 'selected' : ''}>${esc(checkName(t))}</option>`).join('')}</select>
      <div class="small muted c-desc"></div></div>
    <textarea class="code c-params" spellcheck="false"></textarea>
    <label class="check"><input type="checkbox" class="c-req" ${required ? 'checked' : ''}> Required</label>
    <button class="small danger" data-act="remove-row">✕</button>`;
  const sel = $('.c-type', div), ta = $('.c-params', div), desc = $('.c-desc', div);
  const sync = fill => {
    const h = CHECK_HELP[sel.value];
    desc.textContent = h ? h.desc : '';
    if (fill) ta.value = JSON.stringify((h && h.params) || {}, null, 1);
  };
  ta.value = JSON.stringify(params || {}, null, 1);
  sync(false);
  sel.addEventListener('change', () => sync(true));
  $('#checks').appendChild(div);
}

actions['add-folder'] = () => addFolderRow('', 'allowed_write');
actions['add-check'] = () => addCheckRow(checkTypes[0], CHECK_HELP[checkTypes[0]] && CHECK_HELP[checkTypes[0]].params, true);
actions['remove-row'] = (_d, el) => el.closest('.folder-row, .check-row').remove();

actions['create-task'] = async () => {
  const folders = $$('.folder-row').map(r => ({ folder_path: $('.f-path', r).value.trim(), role: $('.f-role', r).value })).filter(f => f.folder_path);
  const checks = [];
  for (const r of $$('.check-row')) {
    let params;
    try { params = JSON.parse($('.c-params', r).value || '{}'); } catch (_) { throw new Error(`The settings for "${checkName($('.c-type', r).value)}" aren't valid JSON.`); }
    checks.push({ check_type: $('.c-type', r).value, params, is_required: $('.c-req', r).checked });
  }
  const body = { title: $('#t-title').value.trim(), instruction: $('#t-instr').value.trim(), connector: $('#t-conn').value, folders, checks };
  if (!body.title) throw new Error('Please give the task a title.');
  if (!folders.some(f => f.role === 'working')) throw new Error('Add at least one working folder.');
  const t = await api('POST', '/api/ui/tasks', body);
  toast('Task created');
  go('live', t.id);
};

// ---- Live Task -----------------------------------------------------------------------------------
async function live() {
  const tasks = await api('GET', '/api/ui/tasks');
  if (!tasks.length) {
    screenEl.innerHTML = `<h1>Live task</h1><div class="card">${emptyState('⏱️', 'No tasks yet.', '<button class="primary" data-act="demo">Try the demo</button>')}</div>`;
    return;
  }
  if (!selectedTaskId || !tasks.some(t => t.id === selectedTaskId)) {
    selectedTaskId = (tasks.find(t => ['running', 'ready', 'draft'].includes(t.lifecycle)) || tasks[0]).id;
  }
  screenEl.innerHTML = `
    <div class="row between"><h1>Live task</h1>
      <select id="task-pick">${tasks.map(t => `<option value="${esc(t.id)}" ${t.id === selectedTaskId ? 'selected' : ''}>${esc(t.title)} — ${esc(label(t.lifecycle))}</option>`).join('')}</select></div>
    <div id="live-body"></div>`;
  $('#task-pick').addEventListener('change', e => { selectedTaskId = e.target.value; renderLive(); });
  await renderLive();
  pollTimer = setInterval(() => renderLive(true), 2500);
}

const FLOW = [['draft', 'Plan', 'Folders and checks set'], ['ready', 'Snapshot', 'Before picture taken'], ['running', 'Agent works', 'Events are recorded'], ['completed', 'Checked', 'Files verified']];

async function renderLive(quiet) {
  const body = $('#live-body');
  if (!body) return;
  let d;
  try { d = await api('GET', `/api/ui/tasks/${encodeURIComponent(selectedTaskId)}`); } catch (e) { if (!quiet) toast(e.message, true); return; }
  const t = d.task, lc = t.lifecycle;
  const opt = $(`#task-pick option[value="${CSS.escape(t.id)}"]`);
  if (opt) opt.textContent = `${t.title} — ${label(lc)}`;
  const order = { draft: 0, baselining: 0, ready: 1, running: 2, verifying: 2, completed: 3, cancelled: -1, error: -1 };
  const pos = order[lc];
  const steps = FLOW.map(([_, title, sub], i) => `<div class="step ${pos > i || lc === 'completed' ? 'done' : pos === i ? 'now' : ''}"><b>${i + 1}. ${title}</b>${sub}</div>`).join('');
  let next = '';
  if (lc === 'draft') next = `<button class="primary" data-act="task" data-id="baseline">📸 Take the "before" snapshot</button><span class="small muted">Do this before the agent touches anything.</span>`;
  else if (lc === 'ready') next = `<button class="primary" data-act="task" data-id="start">▶️ Start — I'm handing it to the agent</button>`;
  else if (lc === 'running') next = `<button class="primary" data-act="task" data-id="verify">🔍 The agent is done — check it</button>`;
  else if (lc === 'completed') next = `<button class="primary" data-act="goto-results">See the results</button>`;
  const cancellable = ['draft', 'ready', 'running'].includes(lc);
  const evs = (d.events || []).slice().reverse();
  body.innerHTML = `
    <div class="card">
      <div class="row between"><h2>${esc(t.title)}</h2>${badge(lc)}</div>
      ${t.instruction ? `<p class="small muted">${esc(t.instruction)}</p>` : ''}
      <div class="steps">${steps}</div>
      <div class="row">${next}${cancellable ? '<button class="small danger" data-act="task" data-id="cancel">Cancel task</button>' : ''}</div>
    </div>
    <div class="grid-2">
      <div class="card"><h2>Folders</h2><ul class="list">${t.folders.map(f => `<li><div class="mono">${esc(f.folder_path)}</div>${pill(f.role === 'protected' ? 'warn' : 'ready', label(f.role))}</li>`).join('')}</ul>
        ${d.baselines.length ? `<p class="small muted">Snapshot: ${d.baselines.map(b => `${b.files} files${b.unreadable ? `, ${b.unreadable} unreadable` : ''}${b.unstable ? `, ${b.unstable} changing` : ''}`).join(' · ')}</p>` : ''}
        ${lc === 'running' ? `<p class="small muted">File changes noticed so far: ${d.live_file_events}</p>` : ''}</div>
      <div class="card"><h2>What the agent reported</h2>
        ${evs.length ? `<ul class="timeline">${evs.map(e => `<li class="ev-${esc(e.event_type)}"><b>${esc(e.event_type.replace(/_/g, ' '))}</b> <span class="small muted">${esc(e.connector_name)} · ${when(e.source_ts || e.collector_ts)}</span>${evSummary(e)}</li>`).join('')}</ul>`
          : emptyState('👂', t.connector === 'manual' ? 'Manual task — no agent events. The file checks still work.' : 'Waiting for the agent… events appear here as it works.')}</div>
    </div>`;
}

function evSummary(e) {
  const p = e.payload_redacted || {};
  const input = p.input || p.tool_input || {};
  const bits = [p.tool || p.tool_name, input.file_path || input.path || input.command, p.prompt].filter(Boolean).map(x => String(x).slice(0, 140));
  return bits.length ? `<div class="mono muted">${esc(bits.join(' · '))}</div>` : '';
}

actions.task = async d => {
  if (d.id === 'cancel' && !confirm('Cancel this task? Its events stay in the history.')) return;
  await api('POST', `/api/ui/tasks/${encodeURIComponent(selectedTaskId)}/${d.id}`);
  if (d.id === 'verify') { toast('Checks finished'); go('results'); return; }
  toast({ baseline: 'Snapshot saved', start: 'Task started — go ahead and give it to your agent', cancel: 'Task cancelled' }[d.id] || 'Done');
  await renderLive();
};
actions['goto-results'] = () => go('results');

// ---- Results -------------------------------------------------------------------------------------
async function results() {
  const tasks = await api('GET', '/api/ui/tasks');
  if (!selectedTaskId || !tasks.some(t => t.id === selectedTaskId)) selectedTaskId = (tasks.find(t => t.lifecycle === 'completed') || tasks[0] || {}).id;
  screenEl.innerHTML = `
    <h1>Results</h1>
    <p class="lead">Each check looks at the real files — not just what the agent said.</p>
    ${tasks.length ? `<div class="grid-2">
      <div class="card"><h2>Tasks</h2><ul class="list">${tasks.map(t => `<li class="clickable row between ${t.id === selectedTaskId ? 'selected' : ''}" data-act="pick-result" data-id="${esc(t.id)}"><span>${esc(t.title)}</span>${badge(t.lifecycle)}</li>`).join('')}</ul></div>
      <div id="result-body"></div></div>` : `<div class="card">${emptyState('📋', 'Nothing checked yet.')}</div>`}`;
  if (selectedTaskId) await renderResult();
}

async function renderResult() {
  const d = await api('GET', `/api/ui/tasks/${encodeURIComponent(selectedTaskId)}`);
  const run = (d.runs || [])[0];
  const box = $('#result-body');
  if (!run) {
    box.innerHTML = `<div class="card">${emptyState('⏳', 'This task has not been checked yet.', '<button data-act="open-task" data-id="' + esc(d.task.id) + '">Open in Live Task</button>')}</div>`;
    return;
  }
  const v = run.overall_verdict || 'unable_to_verify';
  const [emo, head, sub] = VERDICT[v];
  const required = Object.fromEntries(d.task.checks.map(c => [c.id, c.is_required]));
  box.innerHTML = `
    <div class="verdict ${v}"><div class="emoji">${emo}</div><div><h2>${head}</h2><div class="small">${sub}</div><div class="small muted">Checked ${when(run.finished_at)} · task version ${run.definition_version}</div></div></div>
    ${run.results.map(r => `<div class="result">
      <div class="row between"><b>${esc(checkName(r.check_type))}</b><div class="row">${required[r.check_id] === false ? '<span class="badge">optional</span>' : ''}${badge(r.status)}</div></div>
      <div class="small">${esc(r.summary)}</div>
      ${r.evidence_refs.length ? `<ul>${r.evidence_refs.slice(0, 12).map(x => `<li class="mono">${esc(x)}</li>`).join('')}${r.evidence_refs.length > 12 ? `<li>…and ${r.evidence_refs.length - 12} more</li>` : ''}</ul>` : ''}
      ${r.limitations.length ? `<ul>${r.limitations.map(x => `<li>⚠️ ${esc(x)}</li>`).join('')}</ul>` : ''}
    </div>`).join('')}
    ${d.runs.length > 1 ? `<p class="small muted">${d.runs.length - 1} earlier check run(s) are kept in the history.</p>` : ''}
    <p class="small muted">AgentTracker observes and reports — it doesn't undo changes. The snapshot is a list of fingerprints, not a backup.</p>`;
}

actions['pick-result'] = async d => {
  selectedTaskId = d.id;
  $$('.list li').forEach(li => li.classList.toggle('selected', li.dataset.id === d.id));
  await renderResult();
};

// ---- Saved checks --------------------------------------------------------------------------------
async function saved() {
  const [templates, types] = await Promise.all([api('GET', '/api/ui/templates'), api('GET', '/api/ui/check-types')]);
  checkTypes = types.map(t => t.check_type);
  screenEl.innerHTML = `
    <h1>Saved checks</h1>
    <p class="lead">Keep the checks you use often and add them to new tasks in one click.</p>
    <div class="card">
      ${templates.length ? `<ul class="list">${templates.map(t => `<li class="row between"><div><b>${esc(t.name)}</b> <span class="small muted">${esc(checkName(t.check_type))} · ${esc(t.source)}</span><div class="mono muted">${esc(JSON.stringify(t.params))}</div></div>
        <button class="small danger" data-act="del-tpl" data-id="${esc(t.id)}">Delete</button></li>`).join('')}</ul>` : emptyState('📌', 'No saved checks yet.')}
    </div>
    <div class="card">
      <h2>Save a new check</h2>
      <div class="field"><label for="s-name">Name</label><input type="text" id="s-name" placeholder="e.g. Photos folder untouched"></div>
      <div class="field"><label for="s-type">Type</label><select id="s-type">${checkTypes.map(t => `<option value="${t}">${esc(checkName(t))}</option>`).join('')}</select><div class="small muted" id="s-desc"></div></div>
      <div class="field"><label for="s-params">Settings <span class="hint">(JSON)</span></label><textarea class="code" id="s-params" spellcheck="false"></textarea></div>
      <button class="primary" data-act="save-tpl">Save check</button>
    </div>`;
  const sel = $('#s-type');
  const sync = () => { const h = CHECK_HELP[sel.value]; $('#s-desc').textContent = h ? h.desc : ''; $('#s-params').value = JSON.stringify((h && h.params) || {}, null, 1); };
  sel.addEventListener('change', sync);
  sync();
}

actions['save-tpl'] = async () => {
  let params;
  try { params = JSON.parse($('#s-params').value || '{}'); } catch (_) { throw new Error("The settings aren't valid JSON."); }
  await api('POST', '/api/ui/templates', { name: $('#s-name').value.trim(), check_type: $('#s-type').value, params });
  toast('Saved');
  await saved();
};
actions['del-tpl'] = async d => {
  if (!confirm('Delete this saved check? Tasks that already use it are not affected.')) return;
  await api('DELETE', `/api/ui/templates/${encodeURIComponent(d.id)}`);
  await saved();
};

// ---- Settings ------------------------------------------------------------------------------------
async function settings() {
  const s = await api('GET', '/api/ui/settings');
  const lic = s.license;
  screenEl.innerHTML = `
    <h1>Settings</h1>
    <p class="lead">Everything stays on this computer.</p>
    <div class="card">
      <div class="row between"><div><h2>Collection</h2><p class="small muted">When paused, agent events are not recorded. The tray icon shows the same switch.</p></div>
        <button class="${s.paused ? 'primary' : ''}" data-act="pause" data-id="${s.paused ? '0' : '1'}">${s.paused ? '▶️ Resume collection' : '⏸️ Pause collection'}</button></div>
    </div>
    <div class="card">
      <h2>License</h2>
      <p class="small">${pill(lic.state === 'format_valid' ? 'ok' : lic.state === 'invalid' ? 'bad' : 'warn', lic.state.replace(/_/g, ' '))} ${esc(lic.message)}</p>
      <div class="row"><input type="text" id="lic-key" placeholder="AT-XXXXX-XXXXX-XXXXX-XXXXX" class="mono"><button data-act="license">Save key</button></div>
      <p class="small muted">Only the key format is checked for now — online activation isn't wired up yet.</p>
    </div>
    <div class="card">
      <h2>Event history integrity</h2>
      <p class="small muted">Each stored event is linked to the one before it with a fingerprint chain. This catches accidental edits — it can't stop a program on this computer from rewriting the whole database.</p>
      <button data-act="chain">Check the chain</button><div id="chain-out" class="small"></div>
    </div>
    <div class="card small">
      <h2>About</h2>
      <div>Version: <b>${esc(s.version)}</b></div>
      <div>Data folder: <span class="mono">${esc(s.data_dir)}</span></div>
      <div>Connector state: <span class="mono">${esc(s.agenttracker_home)}</span></div>
      <div>Hook program: <span class="mono">${esc(s.hook_binary || 'not found')}</span></div>
      <div>Machine id: <span class="mono">${esc(s.machine_id_prefix)}…</span></div>
    </div>`;
}

actions.pause = async d => {
  await api('POST', '/api/ui/pause', { paused: d.id === '1' });
  await refreshPill();
  await settings();
};
actions.license = async () => {
  const r = await api('POST', '/api/ui/license', { key: $('#lic-key').value });
  toast(r.message, r.state !== 'format_valid');
  await settings();
};
actions.chain = async () => {
  const r = await api('POST', '/api/ui/chain/verify');
  $('#chain-out').innerHTML = `<p>${r.ok ? '✅' : '❌'} ${esc(r.detail)} <span class="muted">(${r.checked} entries checked)</span></p>`;
};

// ---- boot ------------------------------------------------------------------------------------------
route();
setInterval(() => refreshPill().catch(() => {}), 10000);
