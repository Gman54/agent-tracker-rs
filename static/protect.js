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
      <p class="lead">Keep AI helpers like Claude Code out of folders that matter to you. Add a folder below and Claude Code can still read it, but it can't change anything in it unless you say so.</p>
    </div>
    <div class="card" id="guard-card">
      <div class="card-head"><h2>Folders AI helpers can't change</h2></div>
      <div id="guard-claude"></div>
      <div id="guard-asks"></div>
      <div id="guard-folders"><p class="muted">Loading…</p></div>
      <div class="row mt">
        <input class="input" id="guard-new-folder" placeholder="Full folder path, e.g. F:\\Test" style="min-width:20rem">
        <select class="input" id="guard-new-mode" style="width:auto">
          <option value="block">Always block</option>
          <option value="ask">Ask me first</option>
        </select>
        <button class="btn btn-primary" data-act="guardAdd">Guard this folder</button>
      </div>
      <p class="small muted mt">"Ask me first" waits up to 90 seconds for you to press Allow on this page. If you don't answer in time, the change is stopped.</p>
      <details class="mt"><summary class="small">What this does and doesn't catch</summary>
        <p class="small muted">It works through Claude Code's hooks, so it covers Claude Code (the terminal app, and the Code tab in the Claude desktop app when it uses your normal Claude Code settings) after Houndbit is connected to it on the Connections page. It stops Claude's file tools (Write, Edit and similar) and any command that names the folder or runs inside it. Reading is still allowed. A command that reaches the folder without naming it (for example through a script stored somewhere else) can slip past, and other apps aren't covered yet.</p>
      </details>
    </div>
    <div class="card" id="guard-log-card">
      <div class="card-head"><h2>What Protect stopped or let through</h2></div>
      <div id="guard-log"><p class="muted">Nothing yet.</p></div>
    </div>
    <h2 class="mt">Houndbit's safe copy</h2>
    <p class="muted small">This part is for Houndbit's own file work, not for Claude. Houndbit works in a safe copy of a folder and its changes are set aside for you to look at.</p>
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
  const claude = document.getElementById('guard-claude');
  if (claude) {
    claude.innerHTML = g.claude === 'ready'
      ? `<p class="small">${pill('green', 'Claude Code: guarded')} Houndbit is connected to Claude Code, so these folders are checked before Claude changes anything.</p>`
      : g.claude === 'needs_update'
        ? `<div class="note note-plain"><p class="small">${pill('amber', 'Claude Code: one step left')} Houndbit's Claude Code connection is from an older version. Restart Houndbit, or go to <b>Connections</b> and press <b>Connect...</b> on Claude Code again, so the guard has time to ask you.</p></div>`
        : `<div class="note note-plain"><p class="small">${pill('amber', 'Claude Code: not connected')} The guard only works once Houndbit is connected to Claude Code. Go to <b>Connections</b> and press <b>Connect...</b> on Claude Code, then start a new Claude session.</p></div>`;
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
      <table class="table"><thead><tr><th>Folder</th><th>When Claude tries to change it</th><th></th></tr></thead>
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
      : '<p class="muted">Nothing yet. When Claude Code tries to change a guarded folder, it shows up here.</p>';
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
      showToast(`Claude Code wants to change ${a.folder}. Open Protect to allow or block it.`, 'error');
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
  const step1 = s.paired
    ? `<p>${pill('green', 'Done')} Houndbit is connected to Protect.</p>`
    : `<p class="muted">${esc(s.message || '')}</p>
       <div class="row"><button class="btn btn-primary" data-act="protectConnect">Connect Houndbit</button></div>
       ${hosted ? '' : '<p class="small muted mt">A separate Protect program is running, so you\u2019ll need to accept the request there.</p>'}`;

  body.innerHTML = `
    <div class="card">
      <div class="card-head"><h2>Safe copy is on</h2>${pill('green', 'Running')}</div>
      <p class="small muted">${hosted ? 'It started together with Houndbit and stops when you quit Houndbit.' : 'Houndbit is using a separate Protect program that was already running.'}</p>
    </div>
    <div class="card">
      <div class="card-head"><h2>1. Connect Houndbit to its safe copy</h2></div>
      ${step1}
    </div>
    ${s.paired ? `<div class="card" id="protect-folders">
      <div class="card-head"><h2>2. Folders Houndbit may work on (in a safe copy)</h2></div>
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
    box.innerHTML = `<div class="card-head"><h2>2. Folders Houndbit may work on (in a safe copy)</h2></div><p class="small" style="color:var(--danger)">${esc(e.message)}</p>`;
    return;
  }
  const grants = (g && g.grants) || [];
  const addRow = hosted ? `
    <div class="row mt">
      <input class="input" id="protect-new-folder" placeholder="Full folder path, e.g. C:\\Users\\you\\Projects\\demo" style="min-width:22rem">
      <button class="btn btn-primary" data-act="protectAddFolder" data-app="com.houndbit.app">Add folder</button>
    </div>` : '<p class="small muted mt">Add folders in the separate Protect program.</p>';

  const list = grants.length ? `
    <p class="muted">3. Test the safe copy: type the name of a file inside the folder and press <b>Test</b>. Houndbit makes a small test change in the copy only. This tests Houndbit\u2019s own file work. It does not test Claude; use the guard above for that.</p>
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

  box.innerHTML = `<div class="card-head"><h2>2. Folders Houndbit may work on (in a safe copy)</h2></div>${list}${addRow}`;
}

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
actions.protectConnect = () => protectDo('/api/ui/runtime/protect/connect', {}, 'Houndbit is connected to Protect.');
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
