// Houndbit Protect panel. Loaded after app.js and reuses its helpers
// (api, esc, pill, showToast, emptyState, navigate, actions, SCREENS, RENDER, screenEl).
// Protect starts together with Houndbit. Houndbit's own file work goes through Protect with its
// own key, like any other app; this page is also where you answer Protect's questions.
'use strict';

SCREENS.push('protect');
RENDER.protect = async function render_protect() {
  screenEl.innerHTML = `
    <div class="page-head">
      <h1>Protect</h1>
      <p class="lead">Keep AI helpers like Claude Code and Cursor out of folders that matter to you. Add a folder below and they can still read it, but they can't change anything in it unless you say so.</p>
    </div>
    <div class="card" id="addon-card"><p class="muted">Loading…</p></div>
    <div class="card" id="guard-card">
      <div class="card-head"><h2>Folders AI helpers can't change</h2></div>
      <div id="guard-claude"></div>
      <div id="guard-asks"></div>
      <div id="guard-folders"><p class="muted">Loading…</p></div>
      <div class="row mt" id="guard-add-row">
        <input class="input" id="guard-new-folder" placeholder="Full folder path, e.g. F:\\Test" style="min-width:20rem">
        <select class="input" id="guard-new-mode" style="width:auto">
          <option value="block">Always block</option>
          <option value="ask">Ask me first</option>
        </select>
        <button class="btn btn-primary" data-act="guardAdd">Guard this folder</button>
      </div>
      <p class="small muted mt">"Ask me first" waits up to 90 seconds for you to press Allow on this page. If you don't answer in time, the change is stopped.</p>
      <details class="mt"><summary class="small">What this does and doesn't catch</summary>
        <p class="small muted">It works through each app's own hooks, after Houndbit is connected to that app on the Connections page. It covers Claude Code (the terminal app, and the Code tab in the Claude desktop app when it uses your normal Claude Code settings) and Cursor's AI agent. It stops file tools (Write, Edit, Delete and similar) and any command that names the folder, runs inside it, or runs a small script that is stored there or names it. Reading is still allowed.</p>
        <p class="small muted">The chat recorder also checks tool requests from chat apps that go through it. It can't stop and wait for you halfway through a reply, so there "Ask me first" folders are blocked too.</p>
        <p class="small muted">What can still slip past: a command that builds the folder name while it runs, a program that isn't a script, a big script, or a script that fetches another one. In Cursor, "Ask me first" asks here in Houndbit, not in Cursor's own window. Cursor support hasn't been tried with a live Cursor yet.</p>
      </details>
    </div>
    <div class="card" id="guard-log-card">
      <div class="card-head"><h2>What Protect stopped or let through</h2></div>
      <div id="guard-log"><p class="muted">Nothing yet.</p></div>
    </div>
    <h2 class="mt">Houndbit's safe copy</h2>
    <p class="muted small">This part is for Houndbit's own file work, not for Claude or Cursor. Houndbit works in a safe copy of a folder and its changes are set aside for you to look at.</p>
    <div id="protect-body"><div class="card"><p class="muted">Checking on Protect…</p></div></div>`;
  await Promise.all([refreshGuard(), refreshProtect()]);
};

const shownPath = (p) => String(p || '').replace(/^\\\\\?\\UNC\\/, '\\\\').replace(/^\\\\\?\\/, '');

function guardWhen(ts) {
  try { return new Date(ts).toLocaleString(); } catch (_) { return ''; }
}

let guardSeenAsks = new Set();
async function refreshGuard() {
  let g;
  try {
    g = await api('GET', '/api/ui/runtime/protect/guard');
  } catch (e) {
    const f = document.getElementById('guard-folders');
    if (f) f.innerHTML = `<p class="small" style="color:var(--danger)">${esc(e.message)}</p>`;
    return null;
  }
  const on = g.addon_enabled !== false;
  const addon = document.getElementById('addon-card');
  if (addon) {
    addon.innerHTML = `
      <div class="row" style="justify-content:space-between">
        <div>
          <p><b>Protect add-on: ${on ? 'on for testing' : 'off'}</b></p>
          <p class="small muted">${on
            ? 'Protect is switched on for testing, free for now. Later it may become a paid add-on.'
            : 'Protect add-on is off. Guarded folders are not checked, so AI helpers can change them.'}</p>
        </div>
        <label class="row" style="gap:.5rem;cursor:pointer">
          <input type="checkbox" id="addon-toggle"${on ? ' checked' : ''}>
          <span>${on ? 'On' : 'Off'}</span>
        </label>
      </div>`;
    const t = document.getElementById('addon-toggle');
    if (t) t.addEventListener('change', () => guardPost('/api/ui/runtime/protect/addon', { enabled: t.checked }, t.checked ? 'Protect add-on is on.' : 'Protect add-on is off.'));
  }
  const card = document.getElementById('guard-card');
  if (card) {
    card.style.opacity = on ? '' : '0.55';
    card.querySelectorAll('#guard-add-row input, #guard-add-row select, #guard-add-row button').forEach(el => { el.disabled = !on; });
  }
  const claude = document.getElementById('guard-claude');
  if (claude) {
    const line = (name, state, extra) => state === 'ready'
      ? `<p class="small">${pill('green', name + ': guarded')} Houndbit is connected to ${name}, so these folders are checked before ${name} changes anything.</p>`
      : state === 'needs_update'
        ? `<div class="note note-plain"><p class="small">${pill('amber', name + ': one step left')} Houndbit's ${name} connection is from an older version. Restart Houndbit, or go to <b>Connections</b> and press <b>Connect...</b> on ${name} again.</p></div>`
        : `<p class="small muted">${pill('gray', name + ': not connected')} To guard ${name} too, go to <b>Connections</b> and press <b>Connect...</b> on ${name}${extra}.</p>`;
    claude.innerHTML = on
      ? line('Claude Code', g.claude, ', then start a new Claude session') + line('Cursor', g.cursor, ', then restart Cursor')
      : `<div class="note note-plain"><p class="small">${pill('gray', 'Protect add-on is off')} Switch it on above to guard these folders again.</p></div>`;
  }
  const asks = g.asks || [];
  const asksBox = document.getElementById('guard-asks');
  if (asksBox) {
    asksBox.innerHTML = asks.length ? `
      <div class="note note-plain mt-xs">
        <p><b>Waiting for your OK</b> ${pill('amber', String(asks.length))}</p>
        ${asks.map(a => `
          <div class="row" style="justify-content:space-between">
            <div><b>${esc(a.app || 'Claude Code')}</b> wants to change something in <span class="mono">${esc(a.folder)}</span>
              <div class="mono muted small">${esc(a.tool)}: ${esc(a.target)}</div></div>
            <div class="row">
              <button class="btn btn-primary btn-small" data-act="guardAnswer" data-id="${esc(a.id)}" data-ok="1">Allow once</button>
              <button class="btn btn-secondary btn-small" data-act="guardAnswer" data-id="${esc(a.id)}" data-ok="0">Block</button>
            </div>
          </div>`).join('')}
      </div>` : '';
  }
  const folders = g.folders || [];
  const fbox = document.getElementById('guard-folders');
  if (fbox) {
    fbox.innerHTML = folders.length ? `
      <table class="table"><thead><tr><th>Folder</th><th>When an AI helper tries to change it</th><th></th></tr></thead>
      <tbody>${folders.map(f => `
        <tr>
          <td class="mono">${esc(shownPath(f.path))}</td>
          <td>
            <select class="input guard-mode" data-folder="${esc(f.path)}" style="width:auto">
              <option value="block"${f.mode === 'block' ? ' selected' : ''}>Always block</option>
              <option value="ask"${f.mode === 'ask' ? ' selected' : ''}>Ask me first</option>
            </select>
          </td>
          <td><button class="btn btn-secondary btn-small" data-act="guardRemove" data-folder="${esc(f.path)}">Stop guarding</button></td>
        </tr>`).join('')}</tbody></table>`
      : '<p class="muted">No folders guarded yet. Add one below.</p>';
    fbox.querySelectorAll('select.guard-mode').forEach(sel => {
      sel.addEventListener('change', () => guardPost('/api/ui/runtime/protect/guard/folder', { folder: sel.dataset.folder, mode: sel.value }, 'Saved.'));
    });
  }
  const log = g.log || [];
  const lbox = document.getElementById('guard-log');
  if (lbox) {
    lbox.innerHTML = log.length ? `
      <table class="table"><thead><tr><th>When</th><th>What happened</th><th>Why</th></tr></thead>
      <tbody>${log.map(x => `
        <tr>
          <td class="small">${esc(guardWhen(x.ts))}</td>
          <td>${x.decision === 'allowed' ? pill('green', 'Let through') : pill('red', 'Stopped')}
            ${esc(x.app || 'Claude Code')} tried to change <span class="mono">${esc(x.folder || '')}</span>
            <div class="mono muted small">${esc(x.tool || '')}: ${esc(x.target || '')}</div></td>
          <td class="small">${esc(x.why || '')}</td>
        </tr>`).join('')}</tbody></table>`
      : '<p class="muted">Nothing yet. When an AI helper tries to change a guarded folder, it shows up here.</p>';
  }
  return g;
}

async function guardPost(path, payload, okMsg) {
  try {
    await api('POST', path, payload);
    showToast(okMsg, 'success');
  } catch (e) {
    showToast(e.message, 'error');
  }
  await refreshGuard();
}

actions.guardAdd = async () => {
  const input = document.getElementById('guard-new-folder');
  const mode = document.getElementById('guard-new-mode');
  const folder = (input && input.value || '').trim();
  if (!folder) { showToast('Type the folder\u2019s full path first.', 'error'); return; }
  await guardPost('/api/ui/runtime/protect/guard/folder', { folder, mode: mode ? mode.value : 'block' }, 'Folder guarded.');
  if (input) input.value = '';
};
actions.guardRemove = (d) => guardPost('/api/ui/runtime/protect/guard/remove', { folder: d.folder }, 'No longer guarded.');
actions.guardAnswer = (d) => guardPost('/api/ui/runtime/protect/guard/answer', { id: d.id, allow: d.ok === '1' }, d.ok === '1' ? 'Allowed this once.' : 'Blocked.');

// Keep an eye out for "Ask me first" requests on every page, so they don't sit unseen.
setInterval(async () => {
  if (document.hidden) return;
  if (document.getElementById('guard-card')) { await refreshGuard(); return; }
  try {
    const g = await api('GET', '/api/ui/runtime/protect/guard');
    for (const a of (g.asks || [])) {
      if (guardSeenAsks.has(a.id)) continue;
      guardSeenAsks.add(a.id);
      showToast(`${a.app || 'An AI helper'} wants to change ${a.folder}. Open Protect to allow or block it.`, 'error');
    }
  } catch (_) { /* Houndbit's page will show its own error */ }
}, 3000);

async function refreshProtect() {
  const body = document.getElementById('protect-body');
  if (!body) return;
  let s, admin;
  try {
    [s, admin] = await Promise.all([
      api('GET', '/api/ui/runtime/protect/status'),
      api('GET', '/api/ui/runtime/protect/admin'),
    ]);
  } catch (e) {
    body.innerHTML = `<div class="card">${emptyState('shield', 'Could not check Protect', e.message)}</div>`;
    return;
  }

  if (!s.running) {
    body.innerHTML = `<div class="card">
      ${emptyState('shield', 'Protect isn\u2019t on yet', s.message || 'Protect starts with Houndbit. Give it a moment.')}
      <div class="row"><button class="btn btn-secondary" data-act="protectRefresh">Check again</button></div>
    </div>`;
    return;
  }

  const hosted = admin && admin.hosted;
  const linked = s.paired
    ? `<p class="small">${pill('green', 'Linked')} Houndbit is linked to Protect automatically. There is nothing to set up.</p>`
    : `<p class="small muted">${esc(s.message || 'Houndbit is linking itself to Protect.')}</p>
       <div class="row"><button class="btn btn-secondary btn-small" data-act="protectRefresh">Check again</button></div>`;

  body.innerHTML = `
    <div class="card">
      <div class="card-head"><h2>Safe copy is on</h2>${pill('green', 'Running')}</div>
      <p class="small muted">${hosted ? 'It started together with Houndbit and stops when you quit Houndbit.' : 'Houndbit is using a separate Protect program that was already running.'}</p>
      ${linked}
    </div>
    ${s.paired ? `<div class="card" id="protect-folders">
      <div class="card-head"><h2>1. Folders Houndbit may work on (in a safe copy)</h2></div>
      <p class="muted">Loading…</p></div>` : ''}
    ${hosted ? '<div id="protect-waiting"></div><div id="protect-apps"></div>' : ''}`;

  if (s.paired) await refreshFolders(hosted);
  if (hosted) renderAdmin(admin);
}

async function refreshFolders(hosted) {
  const box = document.getElementById('protect-folders');
  if (!box) return;
  let g;
  try {
    g = await api('GET', '/api/ui/runtime/protect/grants');
  } catch (e) {
    box.innerHTML = `<div class="card-head"><h2>1. Folders Houndbit may work on (in a safe copy)</h2></div><p class="small" style="color:var(--danger)">${esc(e.message)}</p>`;
    return;
  }
  const grants = (g && g.grants) || [];
  const addRow = hosted ? `
    <div class="row mt">
      <input class="input" id="protect-new-folder" placeholder="Full folder path, e.g. C:\\Users\\you\\Projects\\demo" style="min-width:22rem">
      <button class="btn btn-primary" data-act="protectAddFolder" data-app="com.houndbit.app">Add folder</button>
    </div>` : '<p class="small muted mt">Add folders in the separate Protect program.</p>';

  const list = grants.length ? `
    <p class="muted">2. Test the safe copy: type the name of a file inside the folder and press <b>Test</b>. Houndbit makes a small test change in the copy only. This tests Houndbit\u2019s own file work. It does not test Claude or Cursor; use the guard above for that.</p>
    <table class="table"><thead><tr><th>Folder</th><th>Test the safe copy</th></tr></thead>
    <tbody>${grants.map((x, i) => `
      <tr>
        <td class="mono">${esc(shownPath(x.folder))}</td>
        <td>
          <div class="row">
            <input class="input" id="protect-path-${i}" placeholder="a file in that folder, e.g. notes.txt" style="min-width:14rem">
            <button class="btn btn-primary btn-small" data-act="protectCheck" data-grant="${esc(x.grant_id || '')}" data-idx="${i}">Test</button>
          </div>
          <div id="protect-result-${i}"></div>
        </td>
      </tr>`).join('')}</tbody></table>`
    : '<p class="muted">No folders yet. Add one below.</p>';

  box.innerHTML = `<div class="card-head"><h2>1. Folders Houndbit may work on (in a safe copy)</h2></div>${list}${addRow}`;
  if (hosted) await renderModelCard(grants);
  await renderLaunchCard();
}

// ---- Start an AI tool in a locked job ---------------------------------------------------------
// Houndbit starts the tool inside a Windows "job". The whole thing is killed if Houndbit closes,
// you can cap its memory, and you can stop it from starting any other program at all. This only
// covers tools Houndbit launches here; it does nothing to tools you opened yourself.

function launchWhen(ts) {
  if (!ts) return '';
  const secs = Math.max(0, Math.floor(Date.now() / 1000) - ts);
  if (secs < 60) return 'just now';
  if (secs < 3600) return `${Math.floor(secs / 60)} min ago`;
  return `${Math.floor(secs / 3600)} h ago`;
}

async function renderLaunchCard() {
  let card = document.getElementById('launch-card');
  if (!card) {
    const after = document.getElementById('model-card') || document.getElementById('protect-folders');
    if (!after) return;
    card = document.createElement('div');
    card.className = 'card';
    card.id = 'launch-card';
    after.after(card);
  }
  let m;
  try {
    m = await api('GET', '/api/ui/runtime/protect/launch');
  } catch (e) {
    card.innerHTML = `<div class="card-head"><h2>Start an AI tool in a locked job</h2></div><p class="small text-danger">${esc(e.message)}</p>`;
    return;
  }
  const running = (m && m.running) || [];
  const notWin = m && m.supported === false ? `
    <div class="note note-plain mt-xs"><p class="small">${pill('amber', 'Windows only')} This only works in the Windows app. On this build Houndbit can\u2019t start a locked job.</p></div>` : '';
  const list = running.length ? `
    <table class="table mt"><thead><tr><th>Tool</th><th>Started</th><th>Locked</th><th></th></tr></thead><tbody>
    ${running.map(r => `<tr>
      <td><span class="mono">${esc(r.program)}</span>${r.args && r.args.length ? ` <span class="small muted">${esc(r.args.join(' '))}</span>` : ''}<br><span class="small muted">id ${r.pid}</span></td>
      <td class="small muted">${launchWhen(r.started_unix)}</td>
      <td class="small">${r.no_child_processes ? 'can\u2019t start other programs' : 'normal'}${r.memory_mb ? `, ${r.memory_mb} MB cap` : ''}</td>
      <td><button class="btn btn-secondary btn-small" data-act="launchStop" data-pid="${r.pid}">Stop</button></td>
    </tr>`).join('')}</tbody></table>` : '<p class="muted mt">Nothing is running in a locked job right now.</p>';
  card.innerHTML = `
    <div class="card-head"><h2>Start an AI tool in a locked job</h2></div>
    <p class="muted">Pick a program (for example your coding AI, or <span class="mono">powershell</span>) and Houndbit starts it inside a locked Windows job. If Houndbit closes, the tool is closed too. You can cap its memory, and you can stop it from starting any other program. This only covers tools started here; it doesn\u2019t touch ones you opened yourself.</p>
    ${notWin}
    <div class="row mt">
      <label class="small" for="launch-prog">Program</label>
      <input class="input launch-prog" id="launch-prog" placeholder="e.g. claude, cursor, or C:\\path\\to\\tool.exe">
    </div>
    <div class="row mt-xs">
      <label class="small" for="launch-args">Extras</label>
      <input class="input launch-args" id="launch-args" placeholder="optional, e.g. --help">
    </div>
    <div class="row mt-xs">
      <label class="small" for="launch-dir">Folder</label>
      <input class="input launch-dir" id="launch-dir" placeholder="optional, e.g. F:\\Test">
    </div>
    <div class="row mt-xs">
      <label class="small" for="launch-mem">Memory cap (MB)</label>
      <input class="input launch-mem" id="launch-mem" type="number" min="64" placeholder="optional, e.g. 2048">
    </div>
    <label class="row small mt-xs"><input type="checkbox" id="launch-nochild"> Don\u2019t let it start other programs (strict \u2014 can break AI tools that use git or python)</label>
    <div class="row mt-xs">
      <button class="btn btn-primary" data-act="launchStart">Start in a locked job</button>
    </div>
    ${list}`;
}

actions.launchStart = async () => {
  const program = ((document.getElementById('launch-prog') || {}).value || '').trim();
  const argsRaw = ((document.getElementById('launch-args') || {}).value || '').trim();
  const dir = ((document.getElementById('launch-dir') || {}).value || '').trim();
  const memRaw = ((document.getElementById('launch-mem') || {}).value || '').trim();
  const nochild = !!(document.getElementById('launch-nochild') || {}).checked;
  if (!program) { showToast('Type the program to start.', 'error'); return; }
  const body = { program, block_child_processes: nochild };
  if (argsRaw) body.args = argsRaw.split(/\s+/);
  if (dir) body.working_dir = dir;
  if (memRaw) {
    const mb = parseInt(memRaw, 10);
    if (!Number.isNaN(mb)) body.memory_mb = mb;
  }
  try {
    const r = await api('POST', '/api/ui/runtime/protect/launch', body);
    showToast((r && r.note) || 'Started in a locked job.', 'success');
  } catch (e) {
    showToast(e.message, 'error');
    return;
  }
  await renderLaunchCard();
};

actions.launchStop = async (d) => {
  try {
    await api('POST', '/api/ui/runtime/protect/launch/stop', { pid: parseInt(d.pid, 10) });
    showToast('Stopped.', 'success');
  } catch (e) {
    showToast(e.message, 'error');
  }
  await renderLaunchCard();
};

// ---- Let a model on this PC work on a folder --------------------------------------------------
// The model only sees the safe copy and can only look at files and suggest new versions. Nothing
// goes into the real folder until you press Apply on a change.

let modelPoll = null;
let modelJobId = null;
// Remember what "Find models" found and what you picked, so the list survives the page redrawing.
let foundModels = [];
let pickedModel = '';

function modelOptions(m) {
  const pick = pickedModel || (m.job && m.job.model) || m.model || '';
  const list = foundModels.slice();
  if (pick && !list.includes(pick)) list.unshift(pick);
  if (!list.length) return '<option value="">Press "Find models"</option>';
  return list.map(x => `<option value="${esc(x)}"${x === pick ? ' selected' : ''}>${esc(x)}</option>`).join('');
}

async function renderModelCard(grants) {
  let card = document.getElementById('model-card');
  if (!card) {
    const folders = document.getElementById('protect-folders');
    if (!folders) return;
    card = document.createElement('div');
    card.className = 'card';
    card.id = 'model-card';
    folders.after(card);
  }
  let m;
  try {
    m = await api('GET', '/api/ui/runtime/protect/model');
  } catch (e) {
    card.innerHTML = `<div class="card-head"><h2>3. Let a model on this PC work on a folder</h2></div><p class="small text-danger">${esc(e.message)}</p>`;
    return;
  }
  const busy = m.job && (m.job.state === 'working' || m.job.state === 'stopping');
  const remoteNote = m.local ? '' : `
    <div class="note note-plain mt-xs">
      <p class="small">${pill('amber', 'Not on this PC')} Your model server (<span class="mono">${esc(m.endpoint)}</span>) is on another computer. Every file the model reads is sent there.</p>
      <label class="row small"><input type="checkbox" id="model-remote-ok"> That's OK for this folder</label>
    </div>`;
  card.innerHTML = `
    <div class="card-head"><h2>3. Let a model on this PC work on a folder</h2></div>
    <p class="muted">Ask your own model (LM Studio or Ollama) to do something in one of the folders above. It works in the safe copy, and it can only look at files and suggest new versions. It can't run programs, go online, or delete anything. When it's finished you see each change and choose <b>Apply</b> or <b>Don't apply</b>.</p>
    <p class="small muted">Model server: <span class="mono">${esc(m.endpoint || 'not set')}</span>. You can change it on the Runtime page.</p>
    ${remoteNote}
    <div class="row mt">
      <label class="small" for="model-pick">Model</label>
      <select class="input model-pick" id="model-pick">
        ${modelOptions(m)}
      </select>
      <button class="btn btn-secondary btn-small" data-act="modelFind">Find models</button>
    </div>
    <div class="row mt-xs">
      <label class="small" for="model-folder">Folder</label>
      <select class="input model-folder" id="model-folder">
        ${grants.length ? grants.map(g => `<option value="${esc(g.grant_id)}">${esc(shownPath(g.folder))}</option>`).join('') : '<option value="">Add a folder above first</option>'}
      </select>
    </div>
    <textarea class="input mt-xs model-request" id="model-request" rows="3" placeholder="What should it do? For example: fix the spelling mistakes in README.md"></textarea>
    <div class="row mt-xs">
      <button class="btn btn-primary" data-act="modelStart"${busy || !grants.length ? ' disabled' : ''}>Start</button>
      ${busy ? '<button class="btn btn-secondary" data-act="modelStop">Stop</button>' : ''}
    </div>
    <div id="model-job" class="mt"></div>`;
  if (m.job) showModelJob(m.job);
}

function modelStateLine(job) {
  switch (job.state) {
    case 'working': return `${pill('amber', 'Working')} The model is on it. This page updates by itself.`;
    case 'stopping': return `${pill('amber', 'Stopping')} It will stop after the step it's on.`;
    case 'review': return `${pill('amber', 'Your turn')} Look over the changes below. Nothing in your real folder has changed yet.`;
    case 'finished': return `${pill('green', 'Finished')} Nothing is waiting for you.`;
    default: return `${pill('red', 'Stopped with a problem')}`;
  }
}

function showModelJob(job) {
  const box = document.getElementById('model-job');
  if (!box) return;
  modelJobId = job.id;
  const changes = job.changes || [];
  const steps = job.steps || [];
  const textBlock = (label, text, note) => `
    <div>
      <div class="small"><b>${label}</b></div>
      ${text === null || text === undefined
        ? `<p class="small muted review-missing">${esc(note || '(no file)')}</p>`
        : `<pre class="diff mono review-text">${esc(text)}</pre>`}
    </div>`;
  box.innerHTML = `
    <div class="note note-plain">
      <p class="small">${modelStateLine(job)}</p>
      <p class="small muted">You asked: ${esc(job.request)} <span class="mono">(${esc(shownPath(job.folder))}, model ${esc(job.model)})</span></p>
      ${job.error ? `<p class="small text-danger">${esc(job.error)}</p>` : ''}
      ${job.summary ? `<p class="small"><b>The model says:</b> ${esc(job.summary)}</p>` : ''}
      ${job.changes_error ? `<p class="small text-danger">${esc(job.changes_error)}</p>` : ''}
    </div>
    ${changes.length ? `
      <div class="row mt row-spread">
        <p><b>Changes waiting for you</b> ${pill('amber', String(changes.length))}</p>
        <div class="row">
          <button class="btn btn-primary btn-small" data-act="modelApply" data-all="1">Apply all</button>
          <button class="btn btn-secondary btn-small" data-act="modelDiscard" data-all="1">Don't apply any</button>
        </div>
      </div>
      ${changes.map(c => `
        <details class="mt-xs" open>
          <summary><span class="mono">${esc(c.path)}</span> ${c.kind === 'added' ? pill('gray', 'new file') : pill('gray', 'changed')}
            ${c.original_changed_since_snapshot ? pill('red', 'changed in your folder since') : ''}</summary>
          ${c.original_changed_since_snapshot ? '<p class="small text-danger">This file was changed in your real folder after the model started. Applying it is refused so your newer edit isn\u2019t lost.</p>' : ''}
          <div class="review-cols">
            ${textBlock('Now', c.kind === 'added' ? null : c.old_text, c.kind === 'added' ? 'This file doesn\u2019t exist yet.' : c.old_note)}
            ${textBlock('After', c.new_text, c.new_note)}
          </div>
          <div class="row mt-xs">
            <button class="btn btn-primary btn-small" data-act="modelApply" data-path="${esc(c.path)}"${c.original_changed_since_snapshot ? ' disabled' : ''}>Apply</button>
            <button class="btn btn-secondary btn-small" data-act="modelDiscard" data-path="${esc(c.path)}">Don't apply</button>
          </div>
        </details>`).join('')}` : ''}
    <details class="mt"${job.state === 'working' ? ' open' : ''}><summary class="small">What happened, step by step (${steps.length})</summary>
      <ol class="small">${steps.map(s => `<li>${esc(s)}</li>`).join('')}</ol>
    </details>`;
  const working = job.state === 'working' || job.state === 'stopping';
  if (working && !modelPoll) {
    modelPoll = setInterval(pollModelJob, 1500);
  } else if (!working && modelPoll) {
    clearInterval(modelPoll);
    modelPoll = null;
  }
}

async function pollModelJob() {
  if (!document.getElementById('model-job') || !modelJobId) {
    clearInterval(modelPoll);
    modelPoll = null;
    return;
  }
  try {
    const job = await api('GET', '/api/ui/runtime/protect/model/job/' + encodeURIComponent(modelJobId));
    const wasWorking = document.querySelector('[data-act="modelStop"]');
    if (wasWorking && job.state !== 'working' && job.state !== 'stopping') {
      // Finished: redraw the whole card so Start comes back.
      await refreshProtect();
      return;
    }
    showModelJob(job);
    // If Protect's rules ask for your OK on a step, the question shows up without a refresh.
    renderAdmin(await api('GET', '/api/ui/runtime/protect/admin'));
  } catch (_) { /* keep the last view; the next poll tries again */ }
}

actions.modelFind = async () => {
  const sel = document.getElementById('model-pick');
  try {
    const r = await api('GET', '/api/ui/runtime/provider/models');
    const models = (r && r.models) || [];
    if (!models.length) { showToast('Your model server answered but has no models loaded. Load one in LM Studio or Ollama first.', 'error'); return; }
    const current = sel ? sel.value : '';
    foundModels = models;
    if (!pickedModel) pickedModel = models.includes(current) ? current : models[0];
    if (sel) sel.innerHTML = models.map(x => `<option value="${esc(x)}"${x === current ? ' selected' : ''}>${esc(x)}</option>`).join('');
    showToast(`Found ${models.length} model${models.length === 1 ? '' : 's'}.`, 'success');
  } catch (e) {
    showToast('Couldn\u2019t reach your model server. Is LM Studio or Ollama running? ' + e.message, 'error');
  }
};

actions.modelStart = async () => {
  const model = (document.getElementById('model-pick') || {}).value || '';
  const grant_id = (document.getElementById('model-folder') || {}).value || '';
  const request = ((document.getElementById('model-request') || {}).value || '').trim();
  const remote = document.getElementById('model-remote-ok');
  if (!model) { showToast('Pick a model first. Press "Find models".', 'error'); return; }
  pickedModel = model;
  if (!grant_id) { showToast('Pick a folder first.', 'error'); return; }
  if (!request) { showToast('Type what you\u2019d like the model to do.', 'error'); return; }
  try {
    await api('POST', '/api/ui/runtime/protect/model/run', { model, grant_id, request, confirm_remote: !!(remote && remote.checked) });
    showToast('Started. The model is working in the safe copy.', 'success');
  } catch (e) {
    showToast(e.message, 'error');
    return;
  }
  await refreshProtect();
};

actions.modelStop = async () => {
  if (!modelJobId) return;
  try { await api('POST', '/api/ui/runtime/protect/model/stop', { job_id: modelJobId }); } catch (e) { showToast(e.message, 'error'); }
};

async function modelDecide(path, d, okMsg) {
  if (!modelJobId) return;
  const payload = { job_id: modelJobId };
  if (d.all !== '1') payload.paths = [d.path];
  try {
    const r = await api('POST', path, payload);
    const refused = ((r && r.results) || []).filter(x => x.applied === false);
    if (refused.length) showToast(refused.map(x => `${x.path}: ${x.reason}`).join(' '), 'error');
    else showToast(okMsg, 'success');
    showModelJob(r);
  } catch (e) {
    showToast(e.message, 'error');
  }
}

actions.modelApply = (d) => modelDecide('/api/ui/runtime/protect/model/apply', d, d.all === '1' ? 'Applied. Your real files were updated.' : 'Applied. Your real file was updated.');
actions.modelDiscard = (d) => modelDecide('/api/ui/runtime/protect/model/discard', d, 'Thrown away. Your real folder wasn\u2019t touched.');

function renderAdmin(a) {
  const wait = document.getElementById('protect-waiting');
  const appsBox = document.getElementById('protect-apps');
  const pairings = a.pairings || [];
  const approvals = a.approvals || [];
  if (wait) {
    wait.innerHTML = (pairings.length || approvals.length) ? `
      <div class="card">
        <div class="card-head"><h2>Waiting for your OK</h2>${pill('amber', String(pairings.length + approvals.length))}</div>
        ${pairings.map(p => `
          <div class="row" style="justify-content:space-between">
            <div><b>${esc(p.display_name || p.app_id)}</b> wants to connect to Protect <span class="mono muted">${esc(p.app_id)}</span></div>
            <div class="row">
              <button class="btn btn-primary btn-small" data-act="protectPairing" data-id="${esc(p.pairing_id)}" data-ok="1">Allow</button>
              <button class="btn btn-secondary btn-small" data-act="protectPairing" data-id="${esc(p.pairing_id)}" data-ok="0">Say no</button>
            </div>
          </div>`).join('')}
        ${approvals.map(x => `
          <div class="row" style="justify-content:space-between">
            <div><b>${esc(x.app_id)}</b> wants to do <span class="mono">${esc(x.operation_id)}</span>
              <span class="mono muted small">${esc(JSON.stringify(x.arguments || {}))}</span></div>
            <div class="row">
              <button class="btn btn-primary btn-small" data-act="protectApproval" data-id="${esc(x.approval_id)}" data-ok="1">Allow</button>
              <button class="btn btn-secondary btn-small" data-act="protectApproval" data-id="${esc(x.approval_id)}" data-ok="0">Block</button>
            </div>
          </div>`).join('')}
      </div>` : '';
  }
  const others = (a.apps || []).filter(x => x.app_id !== 'com.houndbit.app');
  if (appsBox) {
    appsBox.innerHTML = others.length ? `
      <div class="card">
        <div class="card-head"><h2>Other apps using Protect</h2></div>
        <table class="table"><thead><tr><th>App</th><th>Folders</th><th></th></tr></thead>
        <tbody>${others.map((x, i) => {
          const fs = (a.grants || []).filter(g => g.app_id === x.app_id).map(g => `<div class="mono small">${esc(shownPath(g.folder))}</div>`).join('') || '<span class="muted small">none yet</span>';
          return `<tr>
            <td><b>${esc(x.display_name || x.app_id)}</b><div class="mono muted small">${esc(x.app_id)}</div></td>
            <td>${fs}
              <div class="row mt-xs">
                <input class="input" id="protect-app-folder-${i}" placeholder="Full folder path" style="min-width:14rem">
                <button class="btn btn-secondary btn-small" data-act="protectAddFolder" data-app="${esc(x.app_id)}" data-idx="${i}">Add folder</button>
              </div></td>
            <td><button class="btn btn-secondary btn-small" data-act="protectRemoveApp" data-app="${esc(x.app_id)}">Disconnect</button></td>
          </tr>`;
        }).join('')}</tbody></table>
      </div>` : '';
  }
}

async function protectDo(path, payload, okMsg) {
  try {
    const r = await api('POST', path, payload);
    showToast((r && r.message) || okMsg, 'success');
  } catch (e) {
    showToast(e.message, 'error');
  }
  await refreshProtect();
}

actions.protectRefresh = async () => { await refreshProtect(); };
actions.protectPairing = (d) => protectDo('/api/ui/runtime/protect/admin/pairing', { id: d.id, accept: d.ok === '1' }, d.ok === '1' ? 'Allowed.' : 'Said no.');
actions.protectApproval = (d) => protectDo('/api/ui/runtime/protect/admin/approval', { id: d.id, accept: d.ok === '1' }, d.ok === '1' ? 'Allowed.' : 'Blocked.');
actions.protectRemoveApp = (d) => protectDo('/api/ui/runtime/protect/admin/remove-app', { app_id: d.app }, 'Disconnected.');
actions.protectAddFolder = (d) => {
  const id = d.idx !== undefined ? 'protect-app-folder-' + d.idx : 'protect-new-folder';
  const input = document.getElementById(id);
  const folder = (input && input.value || '').trim();
  if (!folder) { showToast('Type the folder\u2019s full path first.', 'error'); return; }
  return protectDo('/api/ui/runtime/protect/admin/folder', { app_id: d.app, folder }, 'Folder added.');
};

actions.protectCheck = async (d) => {
  const input = document.getElementById('protect-path-' + d.idx);
  const out = document.getElementById('protect-result-' + d.idx);
  const path = (input && input.value || '').trim();
  if (!path) { showToast('Type the name of a file inside the folder first.', 'error'); return; }
  if (out) out.innerHTML = '<p class="small muted mt-xs">Testing…</p>';
  let r;
  try {
    r = await api('POST', '/api/ui/runtime/protect/sandbox-check', { grant_id: d.grant, path });
  } catch (e) {
    if (out) out.innerHTML = `<p class="small" style="color:var(--danger)">${esc(e.message)}</p>`;
    return;
  }
  const ok = r.originals_untouched === true;
  const changes = Array.isArray(r.diff) ? r.diff : [];
  if (out) out.innerHTML = `
    <div class="note ${ok ? 'note-ok' : 'note-plain'} mt-xs">
      <div>${ok ? pill('green', 'Your real file was not touched') : pill('amber', 'Check the result')}</div>
      <p class="small">${esc(r.message || '')}</p>
      ${changes.length ? `<p class="small muted">Houndbit\u2019s test change, kept in the safe copy only: ${changes.map(c => esc(c.path)).join(', ')}</p>` : ''}
    </div>`;
};
