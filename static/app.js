// Houndbit UI — plain JS, no build step, no external assets.
// Every call goes to the loopback server that served this page (session cookie + CSRF header).
'use strict';

const $ = (sel, root = document) => root.querySelector(sel);
const $$ = (sel, root = document) => Array.from(root.querySelectorAll(sel));
const screenEl = $('#screen');

const state = {
  screen: 'home',
  param: null,
  taskId: null,          // task shown on Live / Results
  runId: null,           // run shown on Results
  pollTimer: null,
  statusTimer: null,
  status: null,
  connectors: null,
  templates: [],
  verifyKeys: {},        // taskId -> { key, at } for Verify-now idempotency
  draft: null,           // Create Task stepper state
  wizard: { step: 1, choice: null },
};

// =====================================================================================================
// Small helpers
// =====================================================================================================
function esc(v) {
  return String(v ?? '').replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
}

function when(ms) {
  if (!ms) return '—';
  const diff = (Date.now() - ms) / 1000;
  if (diff < 45) return 'just now';
  if (diff < 3600) return `${Math.max(1, Math.floor(diff / 60))} min ago`;
  if (diff < 86400) return `${Math.floor(diff / 3600)} h ago`;
  if (diff < 86400 * 7) return `${Math.floor(diff / 86400)} day${diff < 172800 ? '' : 's'} ago`;
  return new Date(ms).toLocaleDateString();
}
function changeLabel(k) {
  const s = String(k || '');
  if (/^Create/.test(s)) return 'New file';
  if (/^Remove/.test(s)) return 'Deleted';
  if (/Name|Rename/.test(s)) return 'Renamed or moved';
  if (/^Modify\(Metadata/.test(s)) return 'Details changed';
  if (/^Modify/.test(s)) return 'Edited';
  if (/Close\(Write\)/.test(s)) return 'Finished writing';
  if (/^Access/.test(s)) return 'Opened';
  return s || 'Changed';
}
const clock = ms => (ms ? new Date(ms).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' }) : '');
const lines = txt => String(txt || '').split(/\r?\n/).map(s => s.trim()).filter(Boolean);
const newKey = () => (window.crypto && crypto.randomUUID ? crypto.randomUUID() : `k-${Date.now()}-${Math.random().toString(16).slice(2)}`);

async function api(method, path, body) {
  const opts = { method, headers: {}, credentials: 'same-origin' };
  if (method !== 'GET') opts.headers['x-requested-with'] = 'agenttracker';
  if (body !== undefined) {
    opts.headers['content-type'] = 'application/json';
    opts.body = JSON.stringify(body);
  }
  const res = await fetch(path, opts);
  let data = null;
  try { data = await res.json(); } catch (_) { /* empty body */ }
  if (res.status === 401) throw new Error('This window lost its sign-in. Please reopen Houndbit from the tray.');
  if (!res.ok) throw new Error((data && data.error) || `Something went wrong (error ${res.status}).`);
  return data;
}

// Downloads a POST response as a file (used for report export).
async function downloadPost(path, fallbackName) {
  const res = await fetch(path, { method: 'POST', credentials: 'same-origin', headers: { 'x-requested-with': 'agenttracker' } });
  if (!res.ok) {
    let msg = `Export failed (error ${res.status}).`;
    try { msg = (await res.json()).error || msg; } catch (_) { /* not json */ }
    throw new Error(msg);
  }
  const blob = await res.blob();
  const cd = res.headers.get('content-disposition') || '';
  const m = cd.match(/filename="([^"]+)"/);
  const a = document.createElement('a');
  a.href = URL.createObjectURL(blob);
  a.download = (m && m[1]) || fallbackName;
  document.body.appendChild(a);
  a.click();
  setTimeout(() => { URL.revokeObjectURL(a.href); a.remove(); }, 1000);
  return a.download;
}

function showToast(msg, type = 'info') {
  const stack = $('#toast');
  const t = document.createElement('div');
  t.className = `toast ${type}`;
  t.textContent = msg;
  stack.appendChild(t);
  while (stack.children.length > 4) stack.firstElementChild.remove();
  setTimeout(() => {
    t.classList.add('leaving');
    setTimeout(() => t.remove(), 300);
  }, 3500);
}

// =====================================================================================================
// Icons, illustrations, labels
// =====================================================================================================
const ICON = {
  ok: '<svg viewBox="0 0 24 24" aria-hidden="true"><circle cx="12" cy="12" r="9"/><path d="M8 12.5l2.7 2.7L16 9.5"/></svg>',
  bad: '<svg viewBox="0 0 24 24" aria-hidden="true"><circle cx="12" cy="12" r="9"/><path d="M9 9l6 6M15 9l-6 6"/></svg>',
  warn: '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M12 3.5 21.5 20h-19z"/><path d="M12 10v4.5M12 17.2v.1"/></svg>',
  info: '<svg viewBox="0 0 24 24" aria-hidden="true"><circle cx="12" cy="12" r="9"/><path d="M12 11v5.5M12 7.8v.1"/></svg>',
  muted: '<svg viewBox="0 0 24 24" aria-hidden="true"><circle cx="12" cy="12" r="9"/><path d="M8 12h8"/></svg>',
  unknown: '<svg viewBox="0 0 24 24" aria-hidden="true"><circle cx="12" cy="12" r="9"/><path d="M9.6 9.3a2.5 2.5 0 1 1 3.4 2.4c-.6.3-1 .8-1 1.5v.6M12 16.8v.1"/></svg>',
  eye: '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M2.5 12S6 5.5 12 5.5 21.5 12 21.5 12 18 18.5 12 18.5 2.5 12 2.5 12z"/><circle cx="12" cy="12" r="2.8"/></svg>',
  chat: '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M4 5h16v11H9l-5 4z"/></svg>',
};
const status = (kind, text) => `<span class="status status-${kind}">${ICON[kind] || ''}<span>${esc(text)}</span></span>`;
const pill = (cls, text) => `<span class="pill ${cls ? 'pill-' + cls : ''}">${esc(text)}</span>`;

const ART = {
  box: `<svg class="empty-art" viewBox="0 0 120 96" aria-hidden="true">
      <ellipse class="art-fill-cream" cx="60" cy="84" rx="44" ry="7"/>
      <path class="art-fill-blue" d="M28 38h64v40a4 4 0 0 1-4 4H32a4 4 0 0 1-4-4z"/>
      <path class="art-stroke" d="M28 38h64v40a4 4 0 0 1-4 4H32a4 4 0 0 1-4-4zM28 38l10-14h44l10 14M50 52h20"/>
      <path class="art-stroke-blue" d="M60 8v8M44 12l4 6M76 12l-4 6"/></svg>`,
  clipboard: `<svg class="empty-art" viewBox="0 0 120 96" aria-hidden="true">
      <ellipse class="art-fill-cream" cx="60" cy="86" rx="40" ry="6"/>
      <rect class="art-fill-mint" x="36" y="14" width="48" height="66" rx="8"/>
      <path class="art-stroke" d="M44 14h32a8 8 0 0 1 8 8v50a8 8 0 0 1-8 8H44a8 8 0 0 1-8-8V22a8 8 0 0 1 8-8zM50 10h20v10H50z"/>
      <path class="art-stroke-blue" d="M47 40l4 4 7-8M64 40h10M47 58l4 4 7-8M64 58h10"/></svg>`,
  bookmark: `<svg class="empty-art" viewBox="0 0 120 96" aria-hidden="true">
      <ellipse class="art-fill-cream" cx="60" cy="86" rx="38" ry="6"/>
      <path class="art-fill-blue" d="M42 12h36v68L60 66 42 80z"/>
      <path class="art-stroke" d="M42 12h36v68L60 66 42 80z"/>
      <path class="art-stroke-blue" d="M88 22l6-4M90 34h8M30 22l-6-4M30 34h-8"/></svg>`,
  ear: `<svg class="empty-art" viewBox="0 0 120 96" aria-hidden="true">
      <ellipse class="art-fill-cream" cx="60" cy="86" rx="36" ry="6"/>
      <circle class="art-fill-mint" cx="60" cy="44" r="30"/>
      <path class="art-stroke" d="M48 44a12 12 0 1 1 24 0c0 8-8 10-8 18"/>
      <path class="art-stroke-blue" d="M94 30c4 4 4 24 0 28M100 24c7 7 7 33 0 40"/></svg>`,
  home: `<svg class="hero-art" viewBox="0 0 150 120" aria-hidden="true">
      <ellipse class="art-fill-cream" cx="75" cy="108" rx="62" ry="8"/>
      <path class="art-fill-blue" d="M30 56 75 20l45 36v48H30z"/>
      <path class="art-stroke" d="M22 62 75 18l53 44M30 56v48h90V56"/>
      <rect class="art-fill-mint" x="62" y="70" width="26" height="34" rx="4"/>
      <path class="art-stroke-blue" d="M66 86l6 6 12-13"/>
      <path class="art-stroke" d="M104 30V16h10v22"/></svg>`,
  wave: `<svg class="wizard-art" viewBox="0 0 110 88" aria-hidden="true">
      <ellipse class="art-fill-cream" cx="55" cy="80" rx="40" ry="6"/>
      <circle class="art-fill-blue" cx="55" cy="40" r="30"/>
      <circle class="art-stroke" cx="55" cy="40" r="30"/>
      <path class="art-stroke-blue" d="M44 36v.1M66 36v.1M44 50c6 6 16 6 22 0"/></svg>`,
};
const emptyState = (art, title, text, actions = '') =>
  `<div class="empty">${ART[art] || ''}<h3>${esc(title)}</h3><p>${esc(text)}</p>${actions ? `<div class="row row-center">${actions}</div>` : ''}</div>`;

const LIFECYCLE = {
  draft: ['', 'Draft'],
  baselining: ['blue', 'Taking snapshot'],
  ready: ['blue', 'Ready to start'],
  running: ['green', 'Agent working'],
  verifying: ['blue', 'Checking'],
  completed: ['', 'Finished'],
  cancelled: ['', 'Cancelled'],
  error: ['red', 'Needs attention'],
};
const lifecyclePill = lc => pill(...(LIFECYCLE[lc] || ['', lc]));
const ACTIVE = ['draft', 'baselining', 'ready', 'running', 'verifying'];

const VERDICT = {
  passed: { cls: 'verdict-pass', icon: '✓', text: 'All required checks passed', short: 'Passed', pill: 'green', kind: 'ok' },
  failed: { cls: 'verdict-fail', icon: '✗', text: 'A check found a problem', short: 'Needs attention', pill: 'red', kind: 'bad' },
  unable_to_verify: { cls: 'verdict-unknown', icon: '?', text: 'Unable to verify', short: 'Unable to verify', pill: 'amber', kind: 'warn' },
  not_run: { cls: 'verdict-neutral', icon: '–', text: 'Verification wasn’t run', short: 'Not checked', pill: '', kind: 'muted' },
};
VERDICT.safety_only = { cls: 'verdict-neutral', icon: '–', text: 'Original files kept — the result itself wasn’t checked', short: 'Result not checked', pill: '', kind: 'muted' };
VERDICT.safety_only_other = { ...VERDICT.safety_only, text: 'Safety checks passed — the result itself wasn’t checked' };
VERDICT.partial = { cls: 'verdict-neutral', icon: '–', text: 'Output checks passed, but coverage was incomplete', short: 'Coverage incomplete', pill: '', kind: 'muted' };
// What each check can vouch for (kept in step with check_kind in verifiers/mod.rs).
const CHECK_KIND = { required_outputs_exist: 'exists', inputs_preserved_in_output: 'content', folder_structure_matches: 'content' };
// Kept in step with verification_state in report.rs. A run with zero checks is "not run", never
// "couldn't confirm"; a pass that only protected the originals never shows green.
function runVerdict(run) {
  const rs = (run && run.results) || [];
  if (!rs.length) return VERDICT.not_run;
  if (run.overall_verdict === 'failed') return VERDICT.failed;
  const passed = rs.filter(r => r.status === 'passed');
  const unsure = rs.some(r => r.status === 'unable_to_verify');
  const insufficient = rs.some(r => r.status === 'insufficient_coverage');
  const passing = run.overall_verdict === 'passed' || (!unsure && insufficient && passed.length > 0);
  if (!passing || !passed.length) return VERDICT.unable_to_verify;
  const output = passed.some(r => CHECK_KIND[r.check_type]);
  if (!output) return passed.some(r => ['originals_unchanged', 'no_unexpected_deletions'].includes(r.check_type)) ? VERDICT.safety_only : VERDICT.safety_only_other;
  return insufficient ? VERDICT.partial : VERDICT.passed;
}

// ---- Short task titles (kept in step with short_title in task.rs) ----
const TITLE_FILLER = ['please', 'pls', 'hey', 'hi', 'hello', 'ok', 'okay', 'so', 'now', 'claude', 'can you', 'could you', 'would you', 'will you',
  'i want you to', 'i need you to', "i'd like you to", 'i would like you to', 'i want to', 'i need to', "i'd like to", 'help me', 'go ahead and', "let's", 'lets'];
const TITLE_SMALL = ['the', 'a', 'an', 'and', 'or', 'of', 'to', 'with', 'for', 'in', 'on', 'at', 'that', 'from', 'by', 'into', 'so', 'this', 'these', 'those', 'my', 'your', 'its', 'our', 'each', 'every', 'all', 'some', 'any'];
function shortTitle(text) {
  let s = (String(text || '').split('\n').map(l => l.trim()).find(Boolean) || '').replace(/^["'`]+|["'`]+$/g, '').trim();
  for (;;) {
    const low = s.toLowerCase();
    const f = TITLE_FILLER.find(x => low.startsWith(x) && !/[\p{L}\p{N}]/u.test(low.charAt(x.length)));
    if (!f) break;
    s = s.slice(f.length).replace(/^[\s,!:\-]+/, '');
  }
  const m = s.match(/[?!;]|[.:,](?=\s|$)/);
  const cutShort = !!m;
  const first = m ? s.slice(0, m.index) : s;
  let words = first.split(/\s+/).filter(Boolean);
  const long = words.length > 6;
  words = words.slice(0, 6);
  while (words.join(' ').length > 48 && words.length > 1) words.pop();
  if (long || cutShort) while (words.length > 1 && TITLE_SMALL.includes(words[words.length - 1].toLowerCase())) words.pop();
  const out = words.join(' ').slice(0, 48).replace(/[^\p{L}\p{N})"]+$/u, '');
  return out ? out.charAt(0).toUpperCase() + out.slice(1) : '';
}
// The name to show: the saved title, or a short one made from it when it's a long sentence.
const taskTitle = t => {
  const title = (t && t.title) || '';
  if (!title) return 'Untitled task';
  if (title.length <= 60) return title;
  return shortTitle(title) || title;
};
// Heading with a rename control, plus the full request folded underneath.
function titleBlock(t, lead) {
  const req = (t.instruction || '').trim();
  return `<div class="title-block">
    <div class="title-line"><h1 id="title-${esc(t.id)}">${esc(taskTitle(t))}</h1>
      <button class="btn btn-link btn-small" data-act="task-rename" data-id="${esc(t.id)}" data-keep-enabled="1">Rename</button></div>
    <div class="rename hidden" id="task-ren-${esc(t.id)}"><span class="input-row">
      <input type="text" id="task-ren-in-${esc(t.id)}" value="${esc(taskTitle(t))}" maxlength="120" aria-label="New task name">
      <button class="btn btn-primary btn-small" data-act="task-rename-save" data-id="${esc(t.id)}">Save</button></span></div>
    ${lead ? `<p class="lead">${lead}</p>` : ''}
    ${req ? `<details class="orig-req"><summary>Original request</summary><p>${esc(req)}</p></details>` : ''}
  </div>`;
}
const CHECK_STATUS = {
  passed: ['ok', 'Passed'],
  failed: ['bad', 'Failed'],
  unable_to_verify: ['warn', "Couldn't confirm"],
  not_applicable: ['muted', 'Not applicable'],
  insufficient_coverage: ['muted', 'Insufficient coverage'],
};

const CONNECTOR_NAMES = { claude_code: 'Claude Code', cursor: 'Cursor', copilot_vscode: 'GitHub Copilot', manual: 'Just a folder (manual)' };
const connectorName = n => CONNECTOR_NAMES[n] || n;

const CHECKS = {
  originals_unchanged: {
    name: 'Protected files stayed the same',
    desc: 'The files you protect must be exactly as they were in the snapshot.',
    params: { paths: [], allow_additions: true },
  },
  required_outputs_exist: {
    name: 'Required files exist',
    desc: 'Each file you list must be there afterwards (and not empty).',
    params: { paths: ['output/result.txt'], min_size_bytes: 1 },
  },
  inputs_preserved_in_output: {
    name: 'Nothing got lost on the way',
    desc: 'Every input file’s content must turn up in the output folder.',
    params: { input: 'input', output: 'output' },
  },
  changes_within_scope: {
    name: 'Changes stayed where they should',
    desc: 'Every change must be inside the folders you allow.',
    params: { allowed: [] },
  },
  folder_structure_matches: {
    name: 'The folder looks right',
    desc: 'Files in a folder must match the name patterns you give.',
    params: { root: 'output', patterns: ['*.txt'], required_patterns: [], allow_other: false },
  },
  no_unexpected_deletions: {
    name: 'Nothing got deleted by surprise',
    desc: 'Every file from the “before” snapshot must still be there afterwards.',
    params: { allowed_deletions: [] },
  },
  edits_inside_folder: {
    name: 'The agent only changed files in the project folder',
    desc: 'Every file the agent said it made or edited must be inside the project folder.',
    params: {},
  },
};
const autoLabel = t => (t && t.origin === 'auto' ? 'Started automatically' : '');
const checkName = t => (CHECKS[t] && CHECKS[t].name) || t;

const EVENT_LABELS = {
  session_started: 'Agent session started',
  task_submitted: 'You gave the agent a request',
  tool_started: 'Agent started an action',
  tool_finished: 'Agent finished an action',
  agent_reported_completion: "Agent said it's done (unverified)",
  session_ended: 'Agent session ended',
  capture_error: 'A message couldn’t be read',
};

function evSummary(e) {
  const p = e.payload_redacted || {};
  const input = p.input || p.tool_input || p.toolArgs || {};
  const bits = [p.tool || p.tool_name || p.toolName, input.file_path || input.path || input.command, p.prompt]
    .filter(Boolean)
    .map(x => String(x).slice(0, 140));
  return bits.length ? `<span class="small muted">${esc(bits.join(' · '))}</span>` : '';
}

// =====================================================================================================
// Router + delegated actions
// =====================================================================================================
const SCREENS = ['home', 'connections', 'create', 'live', 'results', 'saved', 'settings'];

function navigate(screen, param) {
  const target = '#' + screen + (param ? '/' + param : '');
  if (location.hash === target) route(); else location.hash = target;
}

async function route() {
  clearInterval(state.pollTimer);
  state.pollTimer = null;
  const [raw, param] = (location.hash || '#home').slice(1).split('/');
  const screen = SCREENS.includes(raw) ? raw : 'home';
  state.screen = screen;
  state.param = param || null;
  $$('#nav a[data-screen]').forEach(a => {
    const on = a.dataset.screen === screen;
    a.classList.toggle('active', on);
    if (on) a.setAttribute('aria-current', 'page'); else a.removeAttribute('aria-current');
  });
  try {
    await RENDER[screen]();
  } catch (e) {
    screenEl.innerHTML = `<div class="card">${emptyState('ear', 'That didn’t load', e.message)}</div>`;
  }
}
window.addEventListener('hashchange', route);

// <button data-act="name" data-...> anywhere in the page.
const actions = {};
document.addEventListener('click', async ev => {
  const el = ev.target.closest('[data-act]');
  if (!el || el.disabled) return;
  const fn = actions[el.dataset.act];
  if (!fn) return;
  ev.preventDefault();
  const busy = el.tagName === 'BUTTON' && !el.dataset.keepEnabled;
  if (busy) el.disabled = true;
  try {
    await fn(el.dataset, el);
  } catch (e) {
    showToast(e.message, 'error');
  } finally {
    if (busy && el.isConnected) el.disabled = false;
  }
});
// Pressing Enter in the rename box saves, Escape cancels.
document.addEventListener('keydown', ev => {
  const el = ev.target;
  if (!el || !el.id || !el.id.startsWith('task-ren-in-')) return;
  if (ev.key === 'Enter') {
    ev.preventDefault();
    const btn = el.parentElement && el.parentElement.querySelector('[data-act="task-rename-save"]');
    if (btn) btn.click();
  } else if (ev.key === 'Escape') {
    ev.preventDefault();
    route();
  }
});
// <select data-change="name">, <input type=checkbox data-change="name">
const changes = {};
document.addEventListener('change', async ev => {
  const el = ev.target.closest('[data-change]');
  if (!el) return;
  const fn = changes[el.dataset.change];
  if (fn) {
    try { await fn(el); } catch (e) { showToast(e.message, 'error'); }
  }
});

actions.go = d => navigate(d.screen, d.param);

// =====================================================================================================
// Collection status (sidebar pill + pause banner), polled every 5s
// =====================================================================================================
async function updateCollectStatus() {
  const pillEl = $('#collect-pill');
  const text = $('#collect-text');
  const btn = $('#collect-toggle');
  try {
    const s = await api('GET', '/api/ui/status');
    state.status = s;
    pillEl.classList.toggle('on', !s.paused);
    pillEl.classList.toggle('paused', !!s.paused);
    text.textContent = s.paused ? 'Paused' : 'Collecting';
    btn.textContent = s.paused ? 'Resume collection' : 'Pause collection';
    btn.className = s.paused ? 'btn btn-primary btn-small btn-block' : 'btn btn-secondary btn-small btn-block';
    $('#pause-banner').classList.toggle('hidden', !s.paused);
    return s;
  } catch (e) {
    pillEl.classList.remove('on', 'paused');
    text.textContent = 'Not connected';
    return null;
  }
}

async function setCollection(paused) {
  await api('POST', `/api/ui/collection/${paused ? 'pause' : 'resume'}`);
  await updateCollectStatus();
  showToast(paused ? 'Collection paused. Messages already queued are kept.' : 'Collection is back on.', paused ? 'warn' : 'success');
}
$('#collect-toggle').addEventListener('click', async ev => {
  const btn = ev.currentTarget;
  btn.disabled = true;
  try {
    await setCollection(!(state.status && state.status.paused));
  } catch (e) {
    showToast(e.message, 'error');
  } finally {
    btn.disabled = false;
  }
});
actions['collection-toggle'] = async () => {
  await setCollection(!(state.status && state.status.paused));
  if (state.screen === 'settings') await RENDER.settings();
};


// =====================================================================================================
// Welcome wizard (full-panel overlay, 3 steps)
// =====================================================================================================
const WIZARD_CHOICES = [
  { id: 'claude_code', title: 'Connect Claude Code', desc: 'Receive activity reported by Claude Code hooks.' },
  { id: 'cursor', title: 'Connect Cursor', desc: 'Receive activity reported by Cursor’s agent (experimental).' },
  { id: 'copilot_vscode', title: 'Connect GitHub Copilot', desc: 'For Copilot’s agent hooks (experimental).' },
  { id: 'manual', title: 'Just check a folder', desc: 'No agent required. Files are checked afterwards.' },
];

function openWizard() {
  state.wizard = { step: 1, choice: null };
  $('#wizard-overlay').classList.remove('hidden');
  renderWizard();
}

function closeWizard() {
  $('#wizard-overlay').classList.add('hidden');
  $('#wizard-overlay').innerHTML = '';
}

function renderWizard() {
  const w = state.wizard;
  const dots = [1, 2, 3].map(n => `<span class="wstep ${n === w.step ? 'active' : n < w.step ? 'done' : ''}"></span>`).join('');
  let body = '';
  if (w.step === 1) {
    body = `${ART.wave}
      <h2 id="wizard-title" class="center">Welcome to Houndbit</h2>
      <p>Houndbit records what an AI agent does on your computer and checks the outcome independently:</p>
      <ul>
        <li><b>Reported activity</b>: the agent's actions and its completion claims.</li>
        <li><b>Observed changes</b>: what actually changed in your folders, measured separately.</li>
        <li><b>Your checks</b>: rules such as "these files must exist" or "originals must not change".</li>
      </ul>
      <div class="note note-plain">Houndbit does not see a model's internal reasoning. For connected agents it observes and reports; it does not block or undo actions. The Agent Runtime additionally enforces tool policy for sessions it runs.</div>
      <div class="wizard-actions"><button class="btn btn-primary btn-big" data-act="wiz-next">Continue</button></div>`;
  } else if (w.step === 2) {
    body = `<h2 id="wizard-title">How do you want to start?</h2>
      <p class="muted">Pick one — you can add the others later from Connections.</p>
      <div class="choice-grid" role="radiogroup">
        ${WIZARD_CHOICES.map(c => `<button type="button" role="radio" aria-checked="${w.choice === c.id}" class="choice ${w.choice === c.id ? 'selected' : ''}" data-act="wiz-choose" data-choice="${c.id}">
          <span class="choice-check" aria-hidden="true">✓</span>
          <span class="choice-title">${esc(c.title)}</span>
          <span class="choice-desc">${esc(c.desc)}</span></button>`).join('')}
      </div>
      <div class="wizard-actions">
        <button class="btn btn-secondary" data-act="wiz-back">← Back</button>
        <button class="btn btn-primary btn-big" data-act="wiz-next" ${w.choice ? '' : 'disabled'}>Continue</button>
      </div>`;
  } else {
    body = `${ART.clipboard}
      <h2 id="wizard-title" class="center">Want to try a quick demo?</h2>
      <p class="center">A sample task is available: a disposable folder with a few files and two checks. It shows the full workflow without touching your own files.</p>
      <p class="center small muted">You can always find the demo in Settings later.</p>
      <div class="wizard-actions">
        <button class="btn btn-secondary" data-act="wiz-finish" data-demo="0">Skip for now</button>
        <button class="btn btn-primary btn-big" data-act="wiz-finish" data-demo="1">Open sample task</button>
      </div>`;
  }
  $('#wizard-overlay').innerHTML = `<div class="wizard"><div class="wizard-steps" aria-label="Step ${w.step} of 3">${dots}</div>${body}</div>`;
  const focusable = $('#wizard-overlay .btn-primary:not(:disabled)') || $('#wizard-overlay .choice');
  if (focusable) focusable.focus();
}

actions['wiz-next'] = () => { state.wizard.step = Math.min(3, state.wizard.step + 1); renderWizard(); };
actions['wiz-back'] = () => { state.wizard.step = Math.max(1, state.wizard.step - 1); renderWizard(); };
actions['wiz-choose'] = d => { state.wizard.choice = d.choice; renderWizard(); };
actions['wiz-finish'] = async d => {
  const choice = state.wizard.choice || 'manual';
  localStorage.setItem('at_wizard_done', 'true');
  try {
    await api('POST', '/api/ui/settings/save', { wizard_done: true, work_mode: choice });
  } catch (e) {
    showToast('I couldn’t save that you’ve seen the welcome — it may show again next time.', 'warn');
  }
  closeWizard();
  if (d.demo === '1') {
    const r = await api('POST', '/api/ui/demo');
    state.taskId = r.task.id;
    showToast('Demo task is ready. Take a snapshot to begin!', 'success');
    navigate('live', r.task.id);
    return;
  }
  if (choice === 'manual') {
    state.draft = null;
    navigate('create', 'manual');
  } else {
    navigate('connections', choice);
  }
};

actions['show-wizard'] = () => openWizard();

// =====================================================================================================
// Home
// =====================================================================================================
async function latestVerdict(tasks) {
  const done = tasks.filter(t => t.lifecycle === 'completed').sort((a, b) => b.updated_at - a.updated_at)[0];
  if (!done) return null;
  const d = await api('GET', `/api/ui/tasks/${done.id}`);
  const run = (d.runs || []).slice().sort((a, b) => (b.finished_at || 0) - (a.finished_at || 0))[0];
  return run ? { task: done, run } : null;
}

function taskRow(t) {
  return `<button class="task-row" data-act="open-task" data-id="${esc(t.id)}" data-lc="${esc(t.lifecycle)}">
    <span class="task-row-main">
      <span class="task-row-title">${esc(taskTitle(t))}</span>
      <span class="task-row-meta">${esc(connectorName(t.connector))}${autoLabel(t) ? ` · ${autoLabel(t)}` : ''} · updated ${esc(when(t.updated_at))}</span>
    </span>
    ${lifecyclePill(t.lifecycle)}
  </button>`;
}

actions['task-rename'] = d => {
  const box = $(`#task-ren-${CSS.escape(d.id)}`);
  box.classList.toggle('hidden');
  if (!box.classList.contains('hidden')) { const i = $(`#task-ren-in-${CSS.escape(d.id)}`); i.focus(); i.select(); }
};
actions['task-rename-save'] = async d => {
  const title = $(`#task-ren-in-${CSS.escape(d.id)}`).value.trim();
  if (!title) { showToast('Please give the task a name.', 'warn'); return; }
  await api('PUT', `/api/ui/tasks/${d.id}/title`, { title });
  showToast('Renamed.', 'success');
  await route();
};

actions['open-task'] = async d => {
  state.taskId = d.id;
  if (['completed', 'error', 'cancelled'].includes(d.lc)) {
    const t = await api('GET', `/api/ui/tasks/${d.id}`);
    if ((t.runs || []).length) {
      const run = t.runs.slice().sort((a, b) => (b.finished_at || 0) - (a.finished_at || 0))[0];
      state.runId = run.id;
      navigate('results', run.id);
      return;
    }
  }
  navigate('live', d.id);
};

async function render_home() {
  const [tasks, s, conns] = await Promise.all([
    api('GET', '/api/ui/tasks'),
    api('GET', '/api/ui/status'),
    api('GET', '/api/ui/connectors').catch(() => []),
  ]);
  state.connectors = conns;
  const sorted = tasks.slice().sort((a, b) => b.updated_at - a.updated_at);
  const weekAgo = Date.now() - 7 * 86400 * 1000;
  const recentCount = tasks.filter(t => t.created_at >= weekAgo).length;
  const connected = conns.filter(c => c.info.name !== 'manual' && c.state === 'connected');
  const lv = await latestVerdict(tasks).catch(() => null);
  const v = lv && runVerdict(lv.run);

  screenEl.innerHTML = `
    <section class="hero hero-compact">
      <div class="spacer">
        <h1>See what your AI agent does. Check that the job is done.</h1>
        <p>Snapshot a folder, let the agent work, then check the result against your own rules.</p>
      </div>
      <div class="row">
        <button class="btn btn-primary btn-big" data-act="go" data-screen="create">New task</button>
        ${tasks.length ? '' : '<button class="btn btn-secondary" data-act="demo">Try the demo</button>'}
      </div>
    </section>

    <div class="stat-row" role="list">
      <div class="stat-chip" role="listitem"><span class="stat-num">${s.active_task_count}</span><span class="stat-label">Active tasks</span></div>
      <div class="stat-chip" role="listitem"><span class="stat-num">${recentCount}</span><span class="stat-label">Tasks this week</span></div>
      <div class="stat-chip" role="listitem"><span class="stat-num">${connected.length}</span><span class="stat-label">${connected.length ? 'Agents set up' : '<a href="#connections">Set up an agent</a>'}</span></div>
      <div class="stat-chip stat-wide" role="listitem"><span class="stat-label">Last check</span>
        ${v ? `${status(v.kind, v.short)}<span class="small muted">${esc(taskTitle(lv.task))} · ${esc(when(lv.run.finished_at))}</span>` : '<span class="small muted">Nothing checked yet</span>'}</div>
    </div>

    <div class="home-grid">
      <div class="card">
        <div class="card-head"><h2>Recent tasks</h2>${tasks.length ? '<button class="btn btn-link" data-act="go" data-screen="results">See results</button>' : ''}</div>
        ${sorted.length ? `<div class="task-list">${sorted.slice(0, 6).map(taskRow).join('')}</div>`
          : `<div class="empty-compact"><p><b>No tasks yet.</b> Start one, or open the sample task to see how it works.</p>
             <div class="row"><button class="btn btn-primary" data-act="go" data-screen="create">New task</button><button class="btn btn-secondary" data-act="demo">Try the demo</button></div></div>`}
      </div>
      <div class="card card-accent">
        <h2>Inspect agent decisions</h2>
        <p>Run a local model and inspect its recorded justification before each action.</p>
        <div class="row"><button class="btn btn-primary" data-act="go" data-screen="runtime">Open Agent Runtime</button></div>
      </div>
    </div>

    <div class="card card-soft mt">
      <div class="health-strip">
        <strong>Capture status</strong>
        <span class="health-item">${s.paused ? status('warn', 'Collection paused') : status('ok', 'Collecting')}</span>
        <span class="health-item">${s.hook_binary_exists ? status('ok', 'Hook program found') : status('warn', 'Hook program not found')}</span>
        <span class="health-item">${s.queue_pending ? status('info', `${s.queue_pending} message(s) waiting`) : status('ok', 'Nothing waiting')}</span>
        <span class="health-item">${status('muted', `${s.event_count} agent messages recorded`)}</span>
      </div>
    </div>`;
}

actions.demo = async () => {
  const r = await api('POST', '/api/ui/demo');
  state.taskId = r.task.id;
  showToast('Demo task is ready. Take a snapshot to begin!', 'success');
  navigate('live', r.task.id);
};

// =====================================================================================================
// Connections
// =====================================================================================================
const CONN_STATE = {
  connected: ['ok', 'Configured'],
  disconnected: ['muted', 'Not connected'],
  degraded: ['warn', 'Needs a look'],
  unsupported_version: ['bad', 'Unsupported version'],
};
const LOGO = { claude_code: ['logo-claude', 'C'], cursor: ['logo-cursor', 'Cu'], copilot_vscode: ['logo-copilot', 'Co'], manual: ['logo-manual', '✓'] };

function connectorCard(c) {
  const name = c.info.name;
  const [logoCls, logoTxt] = LOGO[name] || ['logo-manual', '?'];
  const lastEvent = c.health && c.health.last_seen ? `Last event received: ${when(c.health.last_seen)}` : 'No events yet';
  const head = (sub) => `<div class="conn-head">
      <div class="conn-logo ${logoCls}" aria-hidden="true">${esc(logoTxt)}</div>
      <div class="spacer">
        <h3>${esc(c.info.display_name)}</h3>
        <div class="row row-tight">${status(...(CONN_STATE[c.state] || ['muted', c.state]))}
          ${c.info.maturity === 'experimental' ? pill('amber', 'Experimental') : ''}</div>
        ${sub ? `<div class="small muted mt-xs">${esc(sub)}</div>` : ''}
      </div>
    </div>`;
  if (name === 'manual') {
    return `<div class="card conn-card" id="conn-${name}">
      ${head('')}
      <p class="small">Checks your files only. No agent activity is recorded.</p>
      <div class="row"><button class="btn btn-primary" data-act="go" data-screen="create" data-param="manual">Start manual task</button></div>
    </div>`;
  }
  const det = c.detect || {};
  const caps = (c.capabilities || []).map(cap =>
    `<li class="${cap.available ? 'cap-yes' : 'cap-no'}" title="${esc(cap.detail || '')}">${cap.available ? '' : '✗ '}${esc(EVENT_LABELS[cap.event_type] || cap.event_type)}</li>`).join('');
  const buttons = det.hooks_installed
    ? `<button class="btn btn-primary" data-act="conn-test" data-name="${name}">Test connection</button>
       <button class="btn btn-secondary" data-act="conn-preview" data-name="${name}">Show config</button>
       <button class="btn btn-danger" data-act="conn-uninstall" data-name="${name}">Disconnect</button>`
    : `<button class="btn btn-primary" data-act="conn-preview" data-name="${name}" data-install="1">Connect…</button>
       <button class="btn btn-secondary" data-act="conn-preview" data-name="${name}">Show what will change</button>`;
  const warn = c.state_note && c.state !== 'connected' ? `<div class="note note-warn">${esc(c.state_note)}</div>` : '';
  return `<div class="card conn-card" id="conn-${name}">
    ${head(det.hooks_installed ? lastEvent : '')}
    ${warn}
    <div class="row">${buttons}</div>
    <div class="conn-out" id="conn-out-${name}"></div>
    <details class="conn-details">
      <summary>Connection details</summary>
      <dl class="kv small">
        <dt>Version</dt><dd>${det.agent_found ? esc(det.agent_version || 'Found (version unknown)') : 'Not found on this PC'}</dd>
        <dt>Last event</dt><dd>${esc(lastEvent)}</dd>
        <dt>Settings file</dt><dd>${det.config_path ? `<code>${esc(det.config_path)}</code>` : '—'}</dd>
      </dl>
      ${c.state_note && c.state === 'connected' ? `<p class="small muted">${esc(c.state_note)}</p>` : ''}
      ${(det.notes || []).length ? `<ul class="small muted conn-notes">${det.notes.map(n => `<li>${esc(n)}</li>`).join('')}</ul>` : ''}
      ${caps ? `<div class="small muted">Events this agent can send</div><ul class="cap-list">${caps}</ul>` : ''}
    </details>
  </div>`;
}

async function render_connections() {
  const conns = await api('GET', '/api/ui/connectors');
  state.connectors = conns;
  const order = ['claude_code', 'cursor', 'copilot_vscode', 'manual'];
  conns.sort((a, b) => order.indexOf(a.info.name) - order.indexOf(b.info.name));
  screenEl.innerHTML = `
    <div class="page-head">
      <h1>Connections</h1>
      <p class="lead">Connecting an agent adds a small, clearly labelled entry to its settings file so Houndbit hears what it does. You see the exact change first. “Configured” means the entry is in place; the last event time shows whether anything has actually arrived.</p>
    </div>
    <div class="grid-2">${conns.map(connectorCard).join('')}</div>
    <div id="relay-card" class="mt"></div>
    <p class="small muted mt">None of these connections have been tested against a live agent yet. If something looks off, “Test connection” will tell you plainly what worked and what didn’t.</p>`;
  if (window.renderRelayCard) await window.renderRelayCard($('#relay-card'));
  if (state.param) {
    const card = $(`#conn-${CSS.escape(state.param)}`);
    const det = card && card.querySelector('details');
    if (det) det.open = true;
    if (card) { card.scrollIntoView({ block: 'center' }); card.classList.add('focus-card'); }
  }
}

actions['conn-preview'] = async d => {
  const out = $(`#conn-out-${d.name}`);
  out.innerHTML = '<p class="small muted"><span class="spinner"></span> Looking at the settings file…</p>';
  const p = await api('GET', `/api/ui/connectors/${d.name}/preview`);
  const added = (p.added || []).length, updated = (p.updated || []).length;
  const nothing = p.changes === false || (!added && !updated);
  const n = added + updated;
  const lines = Array.isArray(p.diff) ? p.diff : String(p.diff || '').split('\n');
  const diffHtml = lines.filter(l => !nothing || l.trim()).map(l => {
    const cls = l.startsWith('+') ? 'diff-add' : l.startsWith('-') ? 'diff-del' : 'diff-ctx';
    return `<span class="${cls}">${esc(l) || ' '}</span>`;
  }).join('');
  out.innerHTML = `<div class="stack-sm mt">
    <div class="small">${p.file_exists ? 'This will update' : 'This will create'} <code>${esc(p.config_path)}</code>
      ${nothing ? ' — no changes are required.' : ` — ${n} hook entr${n === 1 ? 'y' : 'ies'} (${esc([...(p.added || []), ...(p.updated || [])].join(', '))}). ${p.file_exists ? 'A backup of your current file is kept.' : ''}`}</div>
    <pre class="diff">${diffHtml || '(no changes)'}</pre>
    ${d.install === '1' || !nothing ? `<div class="row"><button class="btn btn-primary" data-act="conn-install" data-name="${d.name}">${nothing ? 'Reinstall' : 'Yes, connect it'}</button>
      <button class="btn btn-secondary" data-act="conn-close" data-name="${d.name}">Not now</button></div>` : ''}
  </div>`;
};
actions['conn-close'] = d => { $(`#conn-out-${d.name}`).innerHTML = ''; };
actions['conn-install'] = async d => {
  const r = await api('POST', `/api/ui/connectors/${d.name}/install`);
  showToast(`${connectorName(d.name)} is set up. ${r.backup_path ? 'Your old settings were backed up.' : ''}`, 'success');
  await render_connections();
};
actions['conn-uninstall'] = async d => {
  if (!confirm(`Disconnect ${connectorName(d.name)}? Only Houndbit's own entries are removed from its settings file.`)) return;
  const r = await api('POST', `/api/ui/connectors/${d.name}/uninstall`);
  showToast(r.removed ? `${connectorName(d.name)} disconnected.` : 'There was nothing of mine to remove.', 'success');
  await render_connections();
};
actions['conn-test'] = async d => {
  const out = $(`#conn-out-${d.name}`);
  out.innerHTML = '<p class="small muted"><span class="spinner"></span> Testing… this takes a few seconds.</p>';
  const r = await api('POST', `/api/ui/connectors/${d.name}/test`);
  out.innerHTML = `<div class="card card-soft mt">
    <div class="mb-sm">${r.ok ? status('ok', 'All available tests passed') : status('warn', 'Some steps need review')}</div>
    <ul class="test-steps">${(r.steps || []).map(s => `<li>${status(s.ok ? 'ok' : 'bad', s.name)}<span class="small muted">${esc(s.detail || '')}</span></li>`).join('')}</ul>
    <p class="small muted mt-xs">This checks the hook program, the settings file and my own pipeline. It doesn’t start your agent.</p>
  </div>`;
};


// =====================================================================================================
// Create Task — inline 5-step stepper
// =====================================================================================================
const CREATE_STEPS = ['The job', 'Folder', 'Protect', 'Checks', 'Start'];

function freshDraft(connector) {
  return {
    step: 1,
    title: '',
    instruction: '',
    connector: connector || 'manual',
    folder: '',
    protectedText: '',
    checks: { originals_unchanged: true, required_outputs_exist: true, inputs_preserved_in_output: false, changes_within_scope: false, folder_structure_matches: false },
    outputsText: '',
    inputDir: 'input',
    outputDir: 'output',
    allowedText: '',
    structRoot: 'output',
    structPatterns: '*.txt',
    templates: {},
  };
}

// Copies whatever fields are on screen into the draft.
function readDraft() {
  const d = state.draft;
  const val = id => { const el = $('#' + id); return el ? el.value : undefined; };
  const set = (key, id) => { const v = val(id); if (v !== undefined) d[key] = v; };
  set('title', 'c-title'); set('instruction', 'c-instruction'); set('connector', 'c-connector');
  set('folder', 'c-folder'); set('protectedText', 'c-protected');
  set('outputsText', 'c-outputs'); set('inputDir', 'c-input'); set('outputDir', 'c-output');
  set('allowedText', 'c-allowed'); set('structRoot', 'c-root'); set('structPatterns', 'c-patterns');
  $$('[data-check]').forEach(cb => { d.checks[cb.dataset.check] = cb.checked; });
  $$('[data-template]').forEach(cb => { d.templates[cb.dataset.template] = cb.checked; });
}

function validateStep(step) {
  const d = state.draft;
  if (step === 1 && !d.instruction.trim() && !d.title.trim()) return 'Describe the task first. One sentence is enough.';
  if (step === 2 && !d.folder.trim()) return 'Which folder will the agent work in? Type or paste its path.';
  if (step === 4) {
    const any = Object.values(d.checks).some(Boolean) || Object.values(d.templates).some(Boolean);
    if (!any) return 'Select at least one check.';
    if (d.checks.required_outputs_exist && !lines(d.outputsText).length) return 'List the files that should exist afterwards, or untick that check.';
    if (d.checks.changes_within_scope && !lines(d.allowedText).length) return 'List the folders the agent may change, or untick that check.';
  }
  return null;
}

const isAbs = p => /^([a-zA-Z]:[\\/]|\\\\|\/)/.test(p);

function buildTask() {
  const d = state.draft;
  const folder = d.folder.trim();
  const prot = lines(d.protectedText);
  const allowed = lines(d.allowedText);
  const folders = [{ folder_path: folder, role: 'working' }];
  prot.filter(isAbs).forEach(p => folders.push({ folder_path: p, role: 'protected' }));
  allowed.filter(isAbs).forEach(p => folders.push({ folder_path: p, role: 'allowed_write' }));
  const checks = [];
  if (d.checks.originals_unchanged) {
    checks.push(prot.length
      ? { check_type: 'originals_unchanged', params: { paths: prot, allow_additions: false }, is_required: true }
      : { check_type: 'originals_unchanged', params: { paths: [folder], allow_additions: true }, is_required: true });
  }
  if (d.checks.required_outputs_exist) checks.push({ check_type: 'required_outputs_exist', params: { paths: lines(d.outputsText), min_size_bytes: 1 }, is_required: true });
  if (d.checks.inputs_preserved_in_output) checks.push({ check_type: 'inputs_preserved_in_output', params: { input: d.inputDir.trim() || 'input', output: d.outputDir.trim() || 'output' }, is_required: true });
  if (d.checks.changes_within_scope) checks.push({ check_type: 'changes_within_scope', params: { allowed }, is_required: true });
  if (d.checks.folder_structure_matches) {
    checks.push({ check_type: 'folder_structure_matches', params: { root: d.structRoot.trim() || '.', patterns: d.structPatterns.split(/[,\n]/).map(s => s.trim()).filter(Boolean), required_patterns: [], allow_other: false }, is_required: true });
  }
  state.templates.filter(t => d.templates[t.id]).forEach(t => checks.push({ check_type: t.check_type, params: t.params, is_required: true }));
  const title = d.title.trim() || shortTitle(d.instruction) || 'My task';
  return { title, instruction: d.instruction.trim(), connector: d.connector, folders, checks };
}

function checkOption(type, extra = '') {
  const on = state.draft.checks[type];
  return `<label class="check-option ${on ? 'checked' : ''}">
    <input type="checkbox" data-check="${type}" data-change="check-toggle" ${on ? 'checked' : ''}>
    <span class="spacer"><span class="opt-title">${esc(CHECKS[type].name)}</span><br><span class="opt-desc">${esc(CHECKS[type].desc)}</span>
    ${on && extra ? `<span class="opt-extra">${extra}</span>` : ''}</span>
  </label>`;
}

function createStepBody() {
  const d = state.draft;
  const conns = state.connectors || [];
  switch (d.step) {
    case 1:
      return `<h2>What’s the job?</h2>
        <label class="field"><span class="field-label">What are you asking the agent to do?</span>
          <textarea id="c-instruction" placeholder="For example: Sort the photos in my Holiday folder into one folder per month, and don’t touch the originals.">${esc(d.instruction)}</textarea>
          <span class="field-hint">Plain language is fine. This is stored locally as a note and is not sent anywhere.</span></label>
        <label class="field"><span class="field-label">A short name <span class="muted">(optional)</span></span>
          <input id="c-title" type="text" value="${esc(d.title)}" placeholder="Sort holiday photos">
          <span class="field-hint">Leave it empty and a few words are picked from your request. You can rename it later.</span></label>
        <label class="field"><span class="field-label">Which agent is doing it?</span>
          <select id="c-connector">${['claude_code', 'cursor', 'copilot_vscode', 'manual'].map(n => {
            const c = conns.find(x => x.info.name === n);
            const tag = n === 'manual' ? '' : c && c.state === 'connected' ? ' — connected' : ' — not connected yet';
            return `<option value="${n}" ${d.connector === n ? 'selected' : ''}>${esc(connectorName(n) + tag)}</option>`;
          }).join('')}</select>
          <span class="field-hint">If no agent is connected, files are still checked; agent actions will not appear.</span></label>`;
    case 2:
      return `<h2>Which folder will it work in?</h2>
        <label class="field"><span class="field-label">Folder</span>
          <span class="input-row"><input id="c-folder" type="text" value="${esc(d.folder)}" placeholder="C:\\Users\\you\\Documents\\Holiday">
          <button type="button" class="btn btn-secondary" data-act="browse">Browse...</button></span>
          <span class="field-hint">This folder will be baselined before work starts.</span></label>
        <div class="note note-plain">"Baselined" means every file is hashed before the task starts, so changes can be measured afterwards.</div>`;
    case 3:
      return `<h2>Anything that must not change?</h2>
        <label class="field"><span class="field-label">Protected files or folders <span class="muted">(one per line)</span></span>
          <textarea id="c-protected" placeholder="originals&#10;notes/important.docx">${esc(d.protectedText)}</textarea>
          <span class="field-hint">These files must not be changed. You will be alerted if they are. Paths can be inside your folder (like <code>originals</code>) or full paths.</span></label>
        <div class="note note-plain">If left empty, Houndbit checks that no existing file in the folder changes. New files are allowed.</div>`;
    case 4: {
      const tpl = state.templates.length
        ? `<h3 class="mt-lg">Your saved checks</h3><div class="stack-sm">${state.templates.map(t => `<label class="check-option ${d.templates[t.id] ? 'checked' : ''}">
            <input type="checkbox" data-template="${esc(t.id)}" data-change="check-toggle" ${d.templates[t.id] ? 'checked' : ''}>
            <span class="spacer"><span class="opt-title">${esc(t.name)}</span><br><span class="opt-desc">${esc(checkName(t.check_type))}</span></span></label>`).join('')}</div>`
        : '';
      return `<h2>What should be checked afterwards?</h2>
        <div class="stack-sm">
          ${checkOption('originals_unchanged')}
          ${checkOption('required_outputs_exist', `<textarea id="c-outputs" class="mt-xs" placeholder="sorted/2026-07&#10;summary.txt">${esc(d.outputsText)}</textarea><span class="field-hint">One file per line, inside your folder.</span>`)}
          ${checkOption('inputs_preserved_in_output', `<span class="input-row mt-xs"><input id="c-input" type="text" value="${esc(d.inputDir)}" aria-label="Input folder"><input id="c-output" type="text" value="${esc(d.outputDir)}" aria-label="Output folder"></span><span class="field-hint">Input folder, then output folder.</span>`)}
          ${checkOption('changes_within_scope', `<textarea id="c-allowed" class="mt-xs" placeholder="output">${esc(d.allowedText)}</textarea><span class="field-hint">Folders the agent may change, one per line.</span>`)}
          ${checkOption('folder_structure_matches', `<span class="input-row mt-xs"><input id="c-root" type="text" value="${esc(d.structRoot)}" aria-label="Folder to look at"><input id="c-patterns" type="text" value="${esc(d.structPatterns)}" aria-label="Name patterns"></span><span class="field-hint">Folder to look at, then file name patterns (comma separated, like <code>*.jpg, *.txt</code>).</span>`)}
        </div>
        ${tpl}
        <div class="note mt">Only the checks you define are evaluated. Houndbit cannot judge whether the work is semantically correct.</div>`;
    }
    default: {
      const t = buildTask();
      return `<h2>All set? Here’s the plan</h2>
        <dl class="summary-list">
          <dt>Task</dt><dd>${esc(t.title)}</dd>
          ${t.instruction ? `<dt>The job</dt><dd>${esc(t.instruction)}</dd>` : ''}
          <dt>Agent</dt><dd>${esc(connectorName(t.connector))}</dd>
          <dt>Folder</dt><dd><code>${esc(state.draft.folder)}</code></dd>
          <dt>Protected</dt><dd>${lines(state.draft.protectedText).map(esc).join('<br>') || 'Existing files in the folder (new files are fine)'}</dd>
          <dt>Checks</dt><dd>${t.checks.map(c => '• ' + esc(checkName(c.check_type))).join('<br>')}</dd>
        </dl>
        <div class="note note-plain mt">Pressing Start snapshots the folder and begins monitoring. Give your agent the task once monitoring is active.</div>`;
    }
  }
}

function renderCreate() {
  const d = state.draft;
  const steps = CREATE_STEPS.map((s, i) => {
    const n = i + 1;
    return `<span class="step-pill ${n === d.step ? 'active' : n < d.step ? 'done' : ''}" ${n === d.step ? 'aria-current="step"' : ''}><span class="num">${n < d.step ? '✓' : n}</span>${esc(s)}</span>`;
  }).join('');
  screenEl.innerHTML = `
    <div class="page-head"><h1>New task</h1><p class="lead">Five quick steps. You can go back any time.</p></div>
    <div class="stepper" aria-label="Step ${d.step} of 5">${steps}</div>
    <div class="card" id="create-card">
      ${createStepBody()}
      <div class="step-actions">
        ${d.step > 1 ? '<button class="btn btn-secondary" data-act="create-back">← Back</button>' : '<span></span>'}
        ${d.step < 5 ? '<button class="btn btn-primary" data-act="create-next">Next</button>'
          : '<button class="btn btn-primary btn-big" data-act="create-submit">Snapshot and start</button>'}
      </div>
    </div>`;
  const first = $('#create-card textarea, #create-card input[type="text"]');
  if (first && d.step < 5) first.focus();
}

async function render_create() {
  const [conns, templates, s] = await Promise.all([
    state.connectors ? Promise.resolve(state.connectors) : api('GET', '/api/ui/connectors').catch(() => []),
    api('GET', '/api/ui/templates').catch(() => []),
    api('GET', '/api/ui/status').catch(() => ({})),
  ]);
  state.connectors = conns;
  state.templates = templates;
  if (!state.draft || state.param) {
    const preferred = state.param || (s.work_mode && CONNECTOR_NAMES[s.work_mode] ? s.work_mode : null)
      || (conns.find(c => c.info.name !== 'manual' && c.state === 'connected') || { info: { name: 'manual' } }).info.name;
    state.draft = freshDraft(preferred);
    state.param = null;
  }
  renderCreate();
}

changes['check-toggle'] = () => { readDraft(); renderCreate(); };
actions['create-next'] = () => {
  readDraft();
  const problem = validateStep(state.draft.step);
  if (problem) { showToast(problem, 'warn'); return; }
  state.draft.step += 1;
  renderCreate();
};
actions['create-back'] = () => { readDraft(); state.draft.step -= 1; renderCreate(); };
actions.browse = async () => {
  const r = await api('POST', '/api/ui/pick-folder');
  if (!r.supported) {
    showToast('The folder picker isn’t available here — please type or paste the folder path.', 'info');
    $('#c-folder').focus();
    return;
  }
  if (r.path) $('#c-folder').value = r.path;
};
actions['create-submit'] = async (_d, btn) => {
  const body = buildTask();
  btn.innerHTML = '<span class="spinner"></span> Saving the task…';
  let task;
  try {
    task = await api('POST', '/api/ui/tasks', body);
    btn.innerHTML = '<span class="spinner"></span> Taking a snapshot…';
    await api('POST', `/api/ui/tasks/${task.id}/baseline`);
    btn.innerHTML = '<span class="spinner"></span> Starting to watch…';
    await api('POST', `/api/ui/tasks/${task.id}/start`);
  } catch (e) {
    btn.textContent = 'Snapshot and start';
    if (task) {
      showToast(`Your task was saved, but: ${e.message}`, 'error');
      state.draft = null;
      navigate('live', task.id);
      return;
    }
    throw e;
  }
  state.draft = null;
  state.taskId = task.id;
  showToast('Snapshot taken. Monitoring is active; give your agent the task.', 'success');
  navigate('live', task.id);
};


// =====================================================================================================
// Live Task
// =====================================================================================================
function taskPicker(tasks, selected, changeName) {
  const sorted = tasks.slice().sort((a, b) => (ACTIVE.includes(b.lifecycle) - ACTIVE.includes(a.lifecycle)) || b.updated_at - a.updated_at);
  return `<select class="picker" data-change="${changeName}" aria-label="Choose a task">
    ${sorted.map(t => `<option value="${esc(t.id)}" ${t.id === selected ? 'selected' : ''}>${esc(taskTitle(t))} — ${esc((LIFECYCLE[t.lifecycle] || ['', t.lifecycle])[1])}</option>`).join('')}
  </select>`;
}

function liveActions(t) {
  const lc = t.lifecycle;
  const b = [];
  if (lc === 'draft') b.push(`<button class="btn btn-primary" data-act="task" data-op="baseline" data-id="${esc(t.id)}">Take a Snapshot</button>`);
  if (lc === 'ready') b.push(`<button class="btn btn-primary" data-act="task" data-op="start" data-id="${esc(t.id)}">Start watching</button>`);
  if (lc === 'ready' || lc === 'running') {
    b.push(`<button class="btn ${lc === 'running' ? 'btn-primary' : 'btn-secondary'}" id="verify-btn" data-act="verify" data-id="${esc(t.id)}">Verify now</button>`);
  }
  if (lc === 'completed' || lc === 'error') b.push(`<button class="btn btn-primary" data-act="open-task" data-id="${esc(t.id)}" data-lc="${lc}">View results</button>`);
  if (ACTIVE.includes(lc) && lc !== 'verifying') b.push(`<button class="btn btn-danger" data-act="task" data-op="cancel" data-id="${esc(t.id)}">Cancel task</button>`);
  return b.join('');
}

const LIVE_HINT = {
  draft: 'Step one: snapshot the folder to record its starting state.',
  baselining: 'Taking the snapshot now…',
  ready: 'Snapshot taken. Press “Start watching”, then give your agent the job.',
  running: 'Monitoring is active. When the agent finishes, press "Verify now" to run the checks.',
  verifying: 'Checking the results…',
  completed: 'This task has been checked.',
  cancelled: 'This task was cancelled. Its notes are kept in the history.',
  error: 'Something went wrong with this task. The details below may help.',
};

function liveMain(d) {
  const t = d.task;
  const ev = d.agent_events || [];
  const obs = d.observed || { recent_changes: [], change_count: 0, observing: false };
  const agentCol = d.manual
    ? emptyState('ear', 'No agent connected for this task', 'This is a manual task, so there is no agent activity to receive. Files are still checked.')
    : ev.length
      ? `<ul class="event-list">${ev.slice(-80).reverse().map(x => `<li><span class="ev-time">${esc(clock(x.event.source_ts || x.event.collector_ts))}</span><b>${esc(x.label || EVENT_LABELS[x.event.event_type] || x.event.event_type)}</b><br>${evSummary(x.event)}</li>`).join('')}</ul>`
      : `<p class="muted">Waiting for the agent. Messages from ${esc(connectorName(t.connector))} show up here as it works.</p>`;
  const changes = obs.recent_changes || [];
  const obsCol = changes.length
    ? `<ul class="event-list">${changes.slice().reverse().map(c => `<li><span class="ev-time">${esc(clock(c.ts))} · ${esc(changeLabel(c.kind))}</span><code>${esc(c.path)}</code></li>`).join('')}</ul>`
    : `<p class="muted">${obs.observing ? 'No file changes yet. They are listed here as they happen.' : 'Not watching right now. The folder is watched while the task is running.'}</p>`;
  return `
    <div class="card">
      <h3>${ICON.chat.replace('<svg', '<svg class="nav-ico"')}What the agent reported</h3>
      <p class="small muted">Its own words, not checked yet.</p>
      ${agentCol}
    </div>
    <div class="card">
      <h3>${ICON.eye.replace('<svg', '<svg class="nav-ico"')}Observed activity</h3>
      <p class="small muted">${obs.change_count || 0} change(s) so far. These are real file changes, not tied to any agent automatically.</p>
      ${obsCol}
    </div>`;
}

function liveGaps(d) {
  const gaps = d.gaps || [];
  const body = gaps.length
    ? `<ul class="event-list">${gaps.map(g => `<li>${g.ts ? `<span class="ev-time">${esc(clock(g.ts))}</span>` : ''}${esc(g.detail)}</li>`).join('')}</ul>`
    : `<p class="small">${status('ok', 'No gaps so far')}</p><p class="small muted">Gaps, such as the app being closed or a message that couldn’t be read, are listed here.</p>`;
  return `<div class="card ${gaps.length ? 'gap-card' : ''}"><h3>${ICON.warn.replace('<svg', '<svg class="nav-ico"')}Coverage</h3>${body}</div>`;
}

async function render_live() {
  const tasks = await api('GET', '/api/ui/tasks');
  if (state.param) state.taskId = state.param;
  if (!state.taskId || !tasks.some(t => t.id === state.taskId)) {
    const pick = tasks.slice().sort((a, b) => (ACTIVE.includes(b.lifecycle) - ACTIVE.includes(a.lifecycle)) || b.updated_at - a.updated_at)[0];
    state.taskId = pick ? pick.id : null;
  }
  if (!state.taskId) {
    screenEl.innerHTML = `<div class="page-head"><h1>Live task</h1></div>
      <div class="card">${emptyState('box', 'Nothing to watch yet', 'Start a task to see live activity here.',
        '<button class="btn btn-primary" data-act="go" data-screen="create">Start a task</button><button class="btn btn-secondary" data-act="demo">Try the demo</button>')}</div>`;
    return;
  }
  const d = await api('GET', `/api/ui/tasks/${state.taskId}/live`);
  const t = d.task;
  screenEl.innerHTML = `
    <div class="page-head row row-between">
      ${titleBlock(t, esc(LIVE_HINT[t.lifecycle] || ''))}
      ${tasks.length > 1 ? taskPicker(tasks, t.id, 'live-pick') : ''}
    </div>
    <div class="live-layout">
      <div class="live-main" id="live-main">${liveMain(d)}</div>
      <aside class="live-side">
        <div class="card">
          <h3>Task status</h3>
          <div class="row">${lifecyclePill(t.lifecycle)}<span class="small muted">${esc(connectorName(t.connector))}${autoLabel(t) ? ` · ${autoLabel(t)}` : ''}</span></div>
          <p class="small muted mt-xs">Folder: ${t.folders.map(f => `<code>${esc(f.folder_path)}</code>`).join(' ')}</p>
          <div class="stack-sm mt-xs">${liveActions(t)}</div>
        </div>
        <div id="live-gaps">${liveGaps(d)}</div>
      </aside>
    </div>`;
  if (['running', 'baselining', 'verifying', 'ready'].includes(t.lifecycle)) {
    state.pollTimer = setInterval(async () => {
      if (state.screen !== 'live') return;
      try {
        const fresh = await api('GET', `/api/ui/tasks/${state.taskId}/live`);
        if (fresh.task.lifecycle !== t.lifecycle) { route(); return; }
        const main = $('#live-main');
        if (main) main.innerHTML = liveMain(fresh);
        const gaps = $('#live-gaps');
        if (gaps) gaps.innerHTML = liveGaps(fresh);
      } catch (_) { /* keep the last view */ }
    }, 4000);
  }
}

changes['live-pick'] = el => navigate('live', el.value);

actions.task = async d => {
  if (d.op === 'cancel' && !confirm('Cancel this task? Everything recorded so far stays in the history.')) return;
  const msg = { baseline: 'Snapshot taken.', start: 'Monitoring is active. Give your agent the task.', cancel: 'Task cancelled.' }[d.op];
  await api('POST', `/api/ui/tasks/${d.id}/${d.op}`);
  showToast(msg, d.op === 'cancel' ? 'info' : 'success');
  await route();
};

actions.verify = async (d, btn) => {
  const now = Date.now();
  let k = state.verifyKeys[d.id];
  if (!k || now - k.at > 30000) k = state.verifyKeys[d.id] = { key: newKey(), at: now };
  btn.innerHTML = '<span class="spinner"></span> Checking…';
  try {
    const r = await api('POST', `/api/ui/tasks/${d.id}/verify`, { idempotency_key: k.key });
    const run = r.run;
    if (r.deduplicated) {
      // Same key (double click) or a run less than 30s old: don't re-run, offer a fresh key next time.
      delete state.verifyKeys[d.id];
      btn.textContent = 'Verify again';
      showToast('Already verified just now — click again to re-run', 'info');
      return;
    }
    state.runId = run.id;
    const v = runVerdict(run);
    showToast(v.text, v.kind === 'ok' ? 'success' : v.kind === 'bad' ? 'error' : v.kind === 'muted' ? 'info' : 'warn');
    navigate('results', run.id);
  } catch (e) {
    btn.textContent = 'Verify now';
    throw e;
  }
};

// =====================================================================================================
// Results
// =====================================================================================================
function checkCard(r, runId, task) {
  const [kind, label] = CHECK_STATUS[r.status] || ['muted', r.status];
  const def = (task.checks || []).find(c => c.id === r.check_id);
  const ev = (r.evidence_refs || []).map(x => `<li>${esc(typeof x === 'string' ? x : JSON.stringify(x))}</li>`).join('');
  const lim = (r.limitations || []).map(x => `<li>${esc(x)}</li>`).join('');
  return `<div class="card check-card" id="check-${esc(r.check_id)}">
    <div class="check-head">
      ${status(kind, label)}
      <span class="check-name">${esc(checkName(r.check_type))}</span>
      ${def && !def.is_required ? pill('', 'Optional') : ''}
    </div>
    <p class="mt-xs">${esc(r.summary)}</p>
    <div class="row">
      <button class="btn btn-link" data-act="toggle-evidence" data-id="${esc(r.check_id)}" data-keep-enabled="1" aria-expanded="false">Show evidence ▼</button>
      ${['failed', 'unable_to_verify'].includes(r.status) ? `<button class="btn btn-link" data-act="propose" data-run="${esc(runId)}" data-check="${esc(r.check_id)}">Create a check from this +</button>` : ''}
    </div>
    <div class="evidence hidden" id="ev-${esc(r.check_id)}">
      ${ev ? `<b>Evidence examined</b><ul>${ev}</ul>` : '<p class="muted">No extra evidence was recorded for this check.</p>'}
      ${lim ? `<b>What this check can’t tell you</b><ul>${lim}</ul>` : ''}
      ${def ? `<p class="small muted">Settings used: <code>${esc(JSON.stringify(def.params))}</code></p>` : ''}
    </div>
    <div class="proposal" id="prop-${esc(r.check_id)}"></div>
  </div>`;
}

async function render_results() {
  const tasks = await api('GET', '/api/ui/tasks');
  if (state.param) state.runId = state.param;
  let runData = null;
  if (state.runId) runData = await api('GET', `/api/ui/runs/${state.runId}`).catch(() => null);
  let taskDetail = null;
  if (runData) {
    state.taskId = runData.run.task_id;
  } else {
    // No run chosen: pick the most recent task that has a run.
    const candidates = tasks.slice().sort((a, b) => b.updated_at - a.updated_at);
    const ordered = state.taskId ? [tasks.find(t => t.id === state.taskId), ...candidates].filter(Boolean) : candidates;
    for (const t of ordered.slice(0, 15)) {
      const det = await api('GET', `/api/ui/tasks/${t.id}`);
      if ((det.runs || []).length) {
        taskDetail = det;
        const run = det.runs.slice().sort((a, b) => (b.finished_at || 0) - (a.finished_at || 0))[0];
        state.runId = run.id;
        state.taskId = t.id;
        runData = await api('GET', `/api/ui/runs/${run.id}`);
        break;
      }
    }
  }
  if (!runData) {
    screenEl.innerHTML = `<div class="page-head"><h1>Results</h1></div>
      <div class="card">${emptyState('clipboard', 'No results yet', 'Once you press “Verify now” on a task, my findings will appear here — in plain English.',
        '<button class="btn btn-primary" data-act="go" data-screen="live">Go to live task</button>')}</div>`;
    return;
  }
  if (!taskDetail) taskDetail = await api('GET', `/api/ui/tasks/${state.taskId}`);
  const { task, run, events } = runData;
  const runs = (taskDetail.runs || []).slice().sort((a, b) => (b.finished_at || 0) - (a.finished_at || 0));
  const v = runVerdict(run);
  const reqIds = new Set((task.checks || []).filter(c => c.is_required).map(c => c.id));
  const required = run.results.filter(r => reqIds.has(r.check_id) || !(task.checks || []).some(c => c.id === r.check_id));
  const optional = run.results.filter(r => !required.includes(r));
  const agentEvents = (events || []).filter(e => e.event_type !== 'capture_error');
  const said = agentEvents.some(e => e.event_type === 'agent_reported_completion');
  const tasksWithRuns = tasks.filter(t => ['completed', 'error'].includes(t.lifecycle) || t.id === task.id);

  screenEl.innerHTML = `
    <div class="page-head row row-between">
      ${titleBlock(task, `Checked ${esc(when(run.finished_at))} · ${esc(connectorName(task.connector))}${autoLabel(task) ? ` · ${esc(autoLabel(task))}` : ''}`)}
      <div class="row">
        ${tasksWithRuns.length > 1 ? taskPicker(tasksWithRuns, task.id, 'results-task') : ''}
        ${runs.length > 1 ? `<select data-change="results-run" aria-label="Choose a check run">${runs.map((r, i) => `<option value="${esc(r.id)}" ${r.id === run.id ? 'selected' : ''}>${i === 0 ? 'Latest' : 'Earlier'} check · ${esc(new Date(r.finished_at || r.started_at).toLocaleString())}</option>`).join('')}</select>` : ''}
      </div>
    </div>

    <div class="verdict ${v.cls}" role="status">
      <span class="verdict-icon" aria-hidden="true">${v.icon}</span>
      <span>${esc(v.text)}<span class="verdict-sub">${run.results.length ? verdictSub(run.results) : 'Add checks to evaluate this task.'}</span></span>
      <span class="spacer"></span>
      <button class="btn btn-secondary btn-small" data-act="export" data-fmt="html" data-id="${esc(run.id)}">Save report (HTML)</button>
      <button class="btn btn-secondary btn-small" data-act="export" data-fmt="json" data-id="${esc(run.id)}">Save data (JSON)</button>
    </div>

    <div id="svh-box">${svhCard(runData.said_vs_happened)}</div>

    <div class="grid-2">
      <div class="card">
        <h3>What you asked for</h3>
        ${task.instruction ? `<p>${esc(task.instruction.length > 280 ? task.instruction.slice(0, 280) + '…' : task.instruction)}</p>${task.instruction.length > 280 ? '<p class="small muted">The full text is under “Original request” at the top.</p>' : ''}` : '<p class="muted">No description was written for this task.</p>'}
        <p class="small muted">Folder: ${task.folders.map(f => `<code>${esc(f.folder_path)}</code>`).join(' ')}</p>
        <p class="small muted">Checks: ${(task.checks || []).map(c => esc(checkName(c.check_type))).join(', ') || 'none'}</p>
      </div>
      <div class="card">
        <h3>What the agent reported</h3>
        ${task.connector === 'manual' ? '<p class="muted">Manual task — no agent was listening, so there’s nothing reported.</p>'
          : agentEvents.length ? `<p>${said ? status('muted', "Agent said it's done (unverified)") : status('muted', 'The agent didn’t say it was finished')}</p>
            <ul class="event-list">${agentEvents.slice(-8).reverse().map(e => `<li><b>${esc(EVENT_LABELS[e.event_type] || e.event_type)}</b> ${evSummary(e)}</li>`).join('')}</ul>`
          : `<p class="muted">I didn’t hear anything from the agent during this task.</p>
             ${task.connector === 'claude_code' ? `<p class="small muted">Claude only reports in when you use <b>Claude Code</b>: the <b>Code</b> tab in the Claude desktop app, or <code>claude</code> in a terminal. The regular Claude chat (even when it saves files to your PC) doesn’t send updates. The file checks below still show whether the job really got done.</p>` : ''}`}
      </div>
    </div>

    <h2 class="section-title">What we found</h2>
    ${required.map(r => checkCard(r, run.id, task)).join('') || (run.results.length ? '<p class="muted">No required checks were set.</p>' : '<div class="card card-soft"><p><b>Verification wasn’t run.</b> This task has no checks yet. Add checks to evaluate this task, for example when you set up a new task or from Saved checks.</p></div>')}
    ${optional.length ? `<h2 class="section-title">Optional checks</h2><p class="small muted">These don’t change the overall result.</p>${optional.map(r => checkCard(r, run.id, task)).join('')}` : ''}

    ${timelineCard(runData.timeline)}

    <h2 class="section-title">Conversation</h2>
    <div class="card" id="conversation-box"><p class="muted">Looking for Claude Code’s record of this conversation…</p></div>

    <details class="history card card-soft">
      <summary>History for this task (${runs.length} check${runs.length === 1 ? '' : 's'})</summary>
      <ul class="history-list">${runs.map(r => {
        const rv = runVerdict(r);
        return `<li class="row">${status(rv.kind, rv.short)}<span class="small muted">${esc(new Date(r.finished_at || r.started_at).toLocaleString())}</span>
          ${r.id === run.id ? pill('blue', 'Showing') : `<button class="btn btn-link" data-act="go" data-screen="results" data-param="${esc(r.id)}">View</button>`}</li>`;
      }).join('')}</ul>
    </details>`;
  loadConversation(run.id);
}

// ---- What it said vs what really happened (one line at the top of each report) -----------------
// "When things happened": request, quick name list, full snapshot, first action (quick_list.rs).
function timelineCard(t) {
  if (!t || !(t.rows || []).length) return '';
  const rows = t.rows.map(r => `<tr><th scope="row">${esc(r.label)}</th><td>${r.at ? esc(new Date(r.at).toLocaleString()) : 'Not known'}${r.text ? ` <span class="small muted">${esc(r.text)}</span>` : ''}</td></tr>`).join('');
  return `<h2 class="section-title">When things happened</h2>
    <div class="card"><table class="glance">${rows}</table>${t.note ? `<p><b>${esc(t.note)}</b></p>` : ''}</div>`;
}

const SVH_TONE = { ok: 'svh-ok', bad: 'svh-bad', warn: 'svh-warn', neutral: 'svh-neutral' };
const ITEM_KIND = { ok: 'ok', bad: 'bad', warn: 'warn', neutral: 'muted' };
const svhItem = (title, x) => (x && x.label ? `<div class="svh-item"><span class="svh-label">${esc(title)}</span>
  <span>${status(ITEM_KIND[x.tone] || 'muted', x.label)}${x.detail ? `<span class="small muted svh-detail">${esc(x.detail)}</span>` : ''}</span></div>` : '');

function verdictSub(results) {
  const n = st => results.filter(r => r.status === st).length;
  const parts = [`${results.length} check(s)`, `${n('passed')} passed`];
  if (n('insufficient_coverage')) parts.push(`${n('insufficient_coverage')} insufficient coverage`);
  if (n('not_applicable')) parts.push(`${n('not_applicable')} not applicable`);
  return parts.join(' · ');
}

// Three plain rows for presenting a run (from at_a_glance in report.rs).
function glanceTable(rows) {
  if (!Array.isArray(rows) || !rows.length) return '';
  return `<table class="glance"><caption>At a glance</caption>${rows.map(r => `<tr><th scope="row">${esc(r.category)}</th><td>${esc(r.result)}</td></tr>`).join('')}</table>`;
}

function svhCard(s) {
  if (!s || !s.headline) return '';
  return `<div class="card svh ${SVH_TONE[s.tone] || 'svh-warn'}">
    <h3>What it said vs what really happened</h3>
    <p class="svh-head">${esc(s.headline)}</p>
    ${glanceTable(s.at_a_glance)}
    <div class="svh-rows">
      <div class="svh-row"><span class="svh-label">Said</span><span>${esc(s.said)}${s.quote ? ` <q class="svh-quote">${esc(s.quote)}</q>` : ''}</span></div>
      <div class="svh-row"><span class="svh-label">Really happened</span><span>${esc(s.happened)}</span></div>
    </div>
    <div class="svh-items">${svhItem('Session', s.session)}${svhItem('Verification', s.verification)}</div>
  </div>`;
}

// ---- Conversation (Claude Code's own record, shown next to the checks) -------------------------
const CONV_PREVIEW = 600;

function convText(e, id) {
  const t = e.text || '';
  const cutNote = e.cut ? '<span class="small muted"> (very long, so it was cut short here)</span>' : '';
  if (t.length <= CONV_PREVIEW) return `<div class="conv-text">${esc(t)}${cutNote}</div>`;
  return `<div class="conv-text" id="${id}"><span class="conv-short">${esc(t.slice(0, CONV_PREVIEW))}…</span><span class="conv-full hidden">${esc(t)}${cutNote}</span></div>
    <button class="btn btn-link" data-act="conv-more" data-id="${id}" aria-expanded="false">Show more</button>`;
}

function convEntry(e, i) {
  const id = `conv-${i}`;
  const time = e.ts ? `<span class="conv-time">${esc(new Date(e.ts).toLocaleTimeString())}</span>` : '';
  switch (e.kind) {
    case 'you':
      return `<div class="conv conv-you"><div class="conv-who">You ${time}</div>${convText(e, id)}</div>`;
    case 'claude':
      return `<div class="conv conv-claude"><div class="conv-who">Claude ${time}</div>${convText(e, id)}</div>`;
    case 'thinking':
      return `<details class="conv conv-thinking"><summary class="conv-who">Claude’s thinking ${time}</summary>
        <p class="small muted">This is the thinking Claude chose to show. It may be a summary.</p>${convText(e, id)}</details>`;
    case 'action':
      return `<div class="conv conv-action"><div class="conv-who">Action: ${esc(e.tool || 'a tool')} ${time}</div>${e.text ? `<code class="conv-input">${esc(e.text)}</code>` : ''}
        ${e.note_before ? `<p class="conv-note"><span class="muted">Claude’s written note before this action:</span> ${esc(e.note_before)}</p>` : '<p class="small muted conv-note">No note written before this action.</p>'}
        ${e.instruction ? `<p class="small muted">In reply to: ${esc(e.instruction)}</p>` : ''}
        ${e.tool_use_id ? `<p class="small muted">Tool call ${esc(e.tool_use_id)}${e.matched_hook ? ' · matches the start/finish records' : ''}</p>` : ''}</div>`;
    case 'result':
      return `<details class="conv conv-result${e.is_error ? ' conv-bad' : ''}"><summary class="conv-who">Result${e.tool ? ` from ${esc(e.tool)}` : ''}${e.is_error ? ' — it reported a problem' : ''} ${time}</summary>
        ${e.text ? convText(e, id) : '<p class="small muted">Nothing came back.</p>'}</details>`;
    default:
      return '';
  }
}

async function loadConversation(runId) {
  const box = $('#conversation-box');
  if (!box) return;
  let c;
  try {
    c = await api('GET', `/api/ui/runs/${runId}/conversation`);
  } catch (e) {
    box.innerHTML = `<p class="muted">I couldn’t load the conversation just now.</p>`;
    return;
  }
  if (!document.body.contains(box) || state.runId !== runId) return;
  const svh = $('#svh-box');
  if (svh && c.said_vs_happened) svh.innerHTML = svhCard(c.said_vs_happened);
  const entries = c.entries || [];
  if (!entries.length) {
    box.innerHTML = `<p class="muted">${esc(c.message || 'There’s no conversation to show for this task.')}</p>
      ${c.path ? `<p class="small muted">Record file: <code>${esc(c.path)}</code></p>` : ''}`;
    return;
  }
  const notes = [];
  if (c.windowed) notes.push('Showing the part of the conversation from while this task ran.');
  if (c.capped || c.partial_read) notes.push('This conversation is long, so only the newest part is shown.');
  if (c.skipped_lines) notes.push(`${c.skipped_lines} line(s) of the record were housekeeping or a kind I don’t show, so I skipped them.`);
  box.innerHTML = `
    <p class="small muted">This is Claude Code’s own record of the conversation, shown as it was saved. Claude’s words are what it said, not proof. The checks above show what really happened to your files.</p>
    <div class="conv-list">${entries.map(convEntry).join('')}</div>
    <p class="small muted mt">${notes.map(esc).join(' ')} Record file: <code>${esc(c.path || '')}</code></p>`;
}

actions['conv-more'] = (d, btn) => {
  const box = $(`#${CSS.escape(d.id)}`);
  if (!box) return;
  const open = box.querySelector('.conv-full').classList.toggle('hidden') === false;
  box.querySelector('.conv-short').classList.toggle('hidden', open);
  btn.textContent = open ? 'Show less' : 'Show more';
  btn.setAttribute('aria-expanded', String(open));
};

changes['results-run'] = el => navigate('results', el.value);
changes['results-task'] = el => { state.runId = null; state.taskId = el.value; navigate('results'); };

actions['toggle-evidence'] = (d, btn) => {
  const box = $(`#ev-${CSS.escape(d.id)}`);
  const open = box.classList.toggle('hidden') === false;
  btn.textContent = open ? 'Hide evidence ▲' : 'Show evidence ▼';
  btn.setAttribute('aria-expanded', String(open));
};

actions.export = async d => {
  const name = await downloadPost(`/api/ui/export/${d.fmt}/${d.id}`, `houndbit-report.${d.fmt}`);
  showToast(`Saved ${name} to your downloads.`, 'success');
};

actions.propose = async d => {
  const box = $(`#prop-${CSS.escape(d.check)}`);
  const p = await api('POST', '/api/ui/incident/propose', { run_id: d.run, check_id: d.check });
  if (!p.mappable) {
    box.innerHTML = `<div class="note note-plain">${esc(p.explanation)}</div>`;
    return;
  }
  box.innerHTML = `<div class="card card-soft">
    <h4>Save this as a reusable check</h4>
    <p class="small">${esc(p.explanation)}</p>
    <label class="field"><span class="field-label">Name</span><input type="text" id="pname-${esc(d.check)}" value="${esc(p.name)}"></label>
    <p class="small muted">${esc(checkName(p.check_type))} · <code>${esc(JSON.stringify(p.params))}</code></p>
    <div class="row"><button class="btn btn-primary btn-small" data-act="save-proposal" data-check="${esc(d.check)}">Save check</button>
      <button class="btn btn-secondary btn-small" data-act="close-proposal" data-check="${esc(d.check)}">Never mind</button></div>
  </div>`;
  box.dataset.proposal = JSON.stringify(p);
};
actions['close-proposal'] = d => { $(`#prop-${CSS.escape(d.check)}`).innerHTML = ''; };
actions['save-proposal'] = async d => {
  const box = $(`#prop-${CSS.escape(d.check)}`);
  const p = JSON.parse(box.dataset.proposal || '{}');
  const name = $(`#pname-${CSS.escape(d.check)}`).value.trim() || p.name;
  await api('POST', '/api/ui/templates', { name, check_type: p.check_type, params: p.params });
  box.innerHTML = `<div class="note">${status('ok', 'Saved!')} You’ll find “${esc(name)}” in Saved Checks and when you create a task.</div>`;
};


// =====================================================================================================
// Saved Checks
// =====================================================================================================
function describeParams(type, p) {
  p = p || {};
  const list = a => (Array.isArray(a) && a.length ? a.join(', ') : '—');
  switch (type) {
    case 'originals_unchanged': return `Protects: ${list(p.paths)}${p.allow_additions ? ' (new files allowed)' : ''}`;
    case 'required_outputs_exist': return `Must exist: ${list(p.paths)}`;
    case 'inputs_preserved_in_output': return `From “${p.input || '?'}” into “${p.output || '?'}”`;
    case 'changes_within_scope': return `Allowed to change: ${list(p.allowed)}`;
    case 'folder_structure_matches': return `In “${p.root || '.'}”: ${list(p.patterns)}`;
    case 'no_unexpected_deletions': return `Allowed to delete: ${list(p.allowed_deletions)}`;
    case 'edits_inside_folder': return p.folder ? `Folder: ${p.folder}` : 'Folder: the project folder';
    default: return JSON.stringify(p);
  }
}

async function render_saved() {
  const templates = await api('GET', '/api/ui/templates');
  state.templates = templates;
  const types = Object.keys(CHECKS);
  screenEl.innerHTML = `
    <div class="page-head"><h1>Saved checks</h1>
      <p class="lead">Checks you use often, ready to tick when you create a task. You can also make one straight from a result with “Create a check from this +”.</p></div>
    <div class="card">
      ${templates.length ? templates.map(t => `<div class="saved-row" id="tpl-${esc(t.id)}">
          <div class="saved-main">
            <div class="row"><strong>${esc(t.name)}</strong>${t.source && t.source !== 'user' ? pill('blue', 'From a result') : ''}</div>
            <div class="small">${esc(checkName(t.check_type))} — <span class="muted">${esc((CHECKS[t.check_type] || {}).desc || '')}</span></div>
            <div class="params">${esc(describeParams(t.check_type, t.params))}</div>
            <div class="rename hidden" id="ren-${esc(t.id)}"><span class="input-row mt-xs"><input type="text" value="${esc(t.name)}" id="ren-in-${esc(t.id)}" aria-label="New name">
              <button class="btn btn-primary btn-small" data-act="tpl-rename-save" data-id="${esc(t.id)}">Save</button></span></div>
          </div>
          <button class="btn btn-secondary btn-small" data-act="tpl-rename" data-id="${esc(t.id)}" data-keep-enabled="1">Rename</button>
          <button class="btn btn-danger btn-small" data-act="tpl-delete" data-id="${esc(t.id)}" data-name="${esc(t.name)}">Delete</button>
        </div>`).join('')
        : '<p class="empty-compact"><b>No saved checks yet.</b> <span class="muted">Save one with the form below, or make one from a result with “Create a check from this +”.</span></p>'}
    </div>

    <div class="card">
      <h2>Save a new check</h2>
      <div class="grid-2">
        <label class="field"><span class="field-label">Name</span><input type="text" id="n-name" placeholder="Keep my photos safe"></label>
        <label class="field"><span class="field-label">What kind of check?</span>
          <select id="n-type" data-change="n-type">${types.map(t => `<option value="${t}">${esc(CHECKS[t].name)}</option>`).join('')}</select></label>
      </div>
      <p class="small muted" id="n-desc">${esc(CHECKS[types[0]].desc)}</p>
      <label class="field"><span class="field-label">Details <span class="muted">(advanced — the settings in JSON)</span></span>
        <textarea id="n-params" spellcheck="false">${esc(JSON.stringify(CHECKS[types[0]].params, null, 2))}</textarea>
        <span class="field-hint">Paths can be inside the task folder (like <code>output</code>) or full paths.</span></label>
      <div class="row row-end"><button class="btn btn-primary" data-act="tpl-create">Save as template</button></div>
    </div>`;
}

changes['n-type'] = el => {
  $('#n-params').value = JSON.stringify(CHECKS[el.value].params, null, 2);
  $('#n-desc').textContent = CHECKS[el.value].desc;
};
actions['tpl-create'] = async () => {
  const name = $('#n-name').value.trim();
  if (!name) { showToast('Please give your check a name.', 'warn'); $('#n-name').focus(); return; }
  let params;
  try { params = JSON.parse($('#n-params').value || '{}'); } catch (_) {
    showToast('The details aren’t valid JSON — check for a missing comma or quote.', 'warn');
    return;
  }
  await api('POST', '/api/ui/templates', { name, check_type: $('#n-type').value, params });
  showToast(`Saved “${name}”.`, 'success');
  await render_saved();
};
actions['tpl-rename'] = d => {
  const box = $(`#ren-${CSS.escape(d.id)}`);
  box.classList.toggle('hidden');
  if (!box.classList.contains('hidden')) $(`#ren-in-${CSS.escape(d.id)}`).focus();
};
actions['tpl-rename-save'] = async d => {
  const name = $(`#ren-in-${CSS.escape(d.id)}`).value.trim();
  await api('PUT', `/api/ui/templates/${d.id}`, { name });
  showToast('Renamed.', 'success');
  await render_saved();
};
actions['tpl-delete'] = async d => {
  if (!confirm(`Delete “${d.name}”? Tasks that already use it keep their own copy.`)) return;
  await api('DELETE', `/api/ui/templates/${d.id}`);
  showToast('Deleted.', 'info');
  await render_saved();
};

// =====================================================================================================
// Settings
// =====================================================================================================
const RETENTION = [['7', '1 week'], ['30', '1 month'], ['90', '3 months'], ['365', '1 year'], ['0', 'Forever']];

async function render_settings() {
  const s = await api('GET', '/api/ui/settings');
  const v = s.values || {};
  const lic = s.license || {};
  const licOk = lic.state === 'format_valid';
  screenEl.innerHTML = `
    <div class="page-head"><h1>Settings</h1><p class="lead">Change what you like. Each section has its own Save button.</p></div>
    <div class="grid-2">
      <div class="card">
        <h3>Where your data lives</h3>
        <label class="field"><span class="field-label">Storage folder</span><input type="text" readonly value="${esc(s.data_dir)}"></label>
        <label class="field"><span class="field-label">Connector folder</span><input type="text" readonly value="${esc(s.agenttracker_home)}"></label>
      </div>

      <div class="card">
        <h3>Keep data for</h3>
        <label class="field"><select id="s-retention" aria-label="Keep data for">${RETENTION.map(([d, l]) => `<option value="${d}" ${String(v.retention_days || '0') === d ? 'selected' : ''}>${l}</option>`).join('')}</select>
          <span class="field-hint">Evidence for active tasks is never deleted.</span></label>
        <div class="row"><button class="btn btn-primary" data-act="save-retention">Save</button><button class="btn btn-secondary" data-act="purge">Clean up now</button></div>
      </div>

      <div class="card">
        <h3>What stays on this computer</h3>
        <p class="small">Your tasks, agent messages, snapshots and reports are saved in the storage folder on this computer. Houndbit doesn’t upload them anywhere, and its window talks to it only over this computer’s own local connection.</p>
        <p class="small"><b>What can use the internet, only if you use it:</b></p>
        <ul class="small plain-list">
          <li><b>Code changes:</b> “Check the remote now” runs <code>git fetch</code> to your project’s remote (for example GitHub). If you add a GitHub token, Houndbit also asks GitHub about your commits and pull requests.</li>
          <li><b>Agent Runtime:</b> sends prompts and file excerpts to the model app you set up. That is normally on this computer (like LM Studio); another computer is used only if you allow it.</li>
          <li><b>Links</b> you open, such as a commit on GitHub, open in your web browser.</li>
        </ul>
      </div>

      <div class="card">
        <h3>Privacy</h3>
        <p class="small">${status('ok', 'Built-in redaction is always on')}</p>
        <p class="small">Before an agent message is saved, common secret patterns are blanked out: private key blocks, API keys like <code>sk-…</code>, AWS, GitHub, Slack and Google keys, sign-in tokens (JWT and “Bearer”), and lines like <code>password=…</code> or <code>api_key: …</code>. Settings whose names look secret are blanked too. The same is done to Claude’s conversation when it’s shown.</p>
        <p class="small muted">Not every secret can be caught, so it’s best not to paste secrets into prompts. Copies of files kept for Code changes are your files’ real contents (stored on this computer, not redacted).</p>
        <label class="field"><span class="field-label">Extra words or phrases to hide <span class="muted">(one per line)</span></span>
          <textarea id="s-redact" placeholder="My client’s name&#10;Project Bluebird">${esc(v.redaction_patterns || '')}</textarea>
          <span class="field-hint">Applies to new messages from now on. Phrases need at least 3 letters.</span></label>
        <div class="row"><button class="btn btn-primary" data-act="save-redaction">Save</button></div>
      </div>

      <div class="card">
        <h3>Startup</h3>
        <label class="toggle-row"><input type="checkbox" id="s-autostart" ${v.start_with_windows === '1' ? 'checked' : ''}>
          <span><b>Start with Windows</b><br><span class="small muted">Opens quietly in the tray when you sign in.</span></span></label>
        <label class="toggle-row"><input type="checkbox" id="s-tray" ${v.keep_in_tray !== '0' ? 'checked' : ''}>
          <span><b>Keep running in the tray when the window is closed</b><br><span class="small muted">Collection continues while the window is closed. Turn off to quit on close.</span></span></label>
        <div class="row mt-xs"><button class="btn btn-primary" data-act="save-startup">Save</button></div>
      </div>

      <div class="card">
        <h3>Automatic tasks</h3>
        <label class="toggle-row"><input type="checkbox" id="s-autotasks" ${v.auto_tasks !== '0' ? 'checked' : ''}>
          <span><b>Start a task automatically when I give an agent a job</b><br><span class="small muted">Takes a “before” snapshot of the folder the agent works in, then checks the result when the agent says it’s done. You can still set up a task yourself for stricter rules.</span></span></label>
        <div class="row mt-xs"><button class="btn btn-primary" data-act="save-autotasks">Save</button></div>
      </div>

      <div class="card">
        <h3>Code changes (Git)</h3>
        <label class="toggle-row"><input type="checkbox" id="s-gittrack" ${v.git_tracking !== '0' ? 'checked' : ''}>
          <span><b>Remember agent edits in Git projects</b><br><span class="small muted">Keeps a copy of each file just before and after the agent edits it, so Code changes can compare it with what you commit. The copies are saved on this computer.</span></span></label>
        <div class="row mt-xs"><button class="btn btn-primary" data-act="save-gittrack">Save</button></div>
      </div>

      <div class="card">
        <h3>Collection</h3>
        <p>${s.paused ? status('warn', 'Paused — agent messages are queued but not recorded') : status('ok', 'On: receiving agent messages')}</p>
        <button class="btn ${s.paused ? 'btn-primary' : 'btn-secondary'}" data-act="collection-toggle">${s.paused ? 'Resume collection' : 'Pause collection'}</button>
      </div>

      <div class="card">
        <h3>Reports</h3>
        <label class="field"><span class="field-label">Default export folder</span>
          <input type="text" id="s-export" value="${esc(v.export_dir || '')}" placeholder="Leave empty to use your Downloads folder">
          <span class="field-hint">In this version, saved reports go to your browser’s download location; this folder is remembered for when a save dialog is added.</span></label>
        <div class="row"><button class="btn btn-primary" data-act="save-export">Save</button></div>
      </div>

      <div class="card">
        <h3>Getting started</h3>
        <p class="small muted">Replay the welcome guide or create a new sample task.</p>
        <div class="row"><button class="btn btn-secondary" data-act="show-wizard">Show the welcome wizard again</button>
          <button class="btn btn-secondary" data-act="demo">Open demo task</button></div>
      </div>

      <div class="card">
        <h3>License</h3>
        <p>${status(licOk ? 'ok' : 'info', lic.message || 'No license key yet')}</p>
        <div class="input-row"><input type="text" id="s-license" placeholder="AT-XXXX-XXXX-XXXX" aria-label="License key">
          <button class="btn btn-primary" data-act="save-license">Save</button></div>
      </div>

      <div class="card">
        <h3>Activity record</h3>
        <p class="small muted">Each saved message is hash-chained to the previous one, so local tampering can be detected.</p>
        <button class="btn btn-secondary" data-act="chain-verify">Check the record</button>
        <div id="chain-out" class="mt-xs"></div>
      </div>

      <div class="card">
        <h3>About</h3>
        <dl class="kv small">
          <dt>Version</dt><dd>${esc(s.version)}</dd>
          <dt>Hook program</dt><dd>${s.hook_binary ? `<code>${esc(s.hook_binary)}</code>` : 'Not found'}</dd>
          <dt>Machine ID</dt><dd>${esc(s.machine_id_prefix)}…</dd>
        </dl>
        <p class="small muted mt-xs">Houndbit only watches and reports. It never blocks or undoes anything your agent does.</p>
      </div>
    </div>`;
}

async function saveSettings(obj, okMsg) {
  const r = await api('POST', '/api/ui/settings/save', obj);
  (r.notes || []).forEach(n => showToast(n, 'warn'));
  showToast(okMsg, 'success');
}
actions['save-retention'] = () => saveSettings({ retention_days: Number($('#s-retention').value) }, 'Saved. Data will be retained for that period.');
actions['save-redaction'] = () => saveSettings({ redaction_patterns: $('#s-redact').value }, 'Saved. Those phrases will be hidden from now on.');
actions['save-startup'] = () => saveSettings({ start_with_windows: $('#s-autostart').checked, keep_in_tray: $('#s-tray').checked }, 'Startup settings saved.');
actions['save-autotasks'] = () => saveSettings({ auto_tasks: $('#s-autotasks').checked }, 'Saved.');
actions['save-gittrack'] = () => saveSettings({ git_tracking: $('#s-gittrack').checked }, 'Saved.');
actions['save-export'] = () => saveSettings({ export_dir: $('#s-export').value.trim() }, 'Export folder saved.');
actions.purge = async () => {
  if (!confirm('Delete finished tasks older than your “Keep data for” setting? Active tasks are never deleted.')) return;
  const r = await api('POST', '/api/ui/retention/purge');
  if (r.note) { showToast(r.note, 'info'); return; }
  const kept = (r.kept_active || []).length;
  showToast(`Cleaned up ${r.tasks_deleted} task(s) and ${r.events_deleted} message(s).${kept ? ` Kept ${kept} active task(s).` : ''}`, 'success');
};
actions['save-license'] = async () => {
  const r = await api('POST', '/api/ui/license', { key: $('#s-license').value });
  showToast(r.message || 'License checked.', r.state === 'format_valid' ? 'success' : 'warn');
  await render_settings();
};
actions['chain-verify'] = async () => {
  const r = await api('POST', '/api/ui/chain/verify');
  $('#chain-out').innerHTML = r.ok
    ? status('ok', `All good — ${r.checked} record(s) checked`)
    : `${status('bad', `The record looks altered${r.first_bad_seq ? ` at entry ${r.first_bad_seq}` : ''}`)}<p class="small muted">${esc(r.detail || '')}</p>`;
};

// =====================================================================================================
// Boot
// =====================================================================================================
const RENDER = {
  home: render_home,
  connections: render_connections,
  create: render_create,
  live: render_live,
  results: render_results,
  saved: render_saved,
  settings: render_settings,
};

(async function boot() {
  const s = await updateCollectStatus();
  state.statusTimer = setInterval(updateCollectStatus, 5000);
  await route();
  // The loopback port changes between launches, so localStorage alone would forget; the server copy wins.
  const localDone = localStorage.getItem('at_wizard_done') === 'true';
  if (s && s.wizard_done && !localDone) localStorage.setItem('at_wizard_done', 'true');
  if (!localDone && !(s && s.wizard_done)) openWizard();
})();
