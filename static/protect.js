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
      <p class="lead">Protect keeps an AI helper's file work inside a safe copy of a folder you pick. The helper reads a snapshot and its changes are set aside for you to look at. Your real files are never written.</p>
    </div>
    <div id="protect-body"><div class="card"><p class="muted">Checking on Protect…</p></div></div>`;
  await refreshProtect();
};

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
      <div class="card-head"><h2>Protect is on</h2>${pill('green', 'Running')}</div>
      <p class="small muted">${hosted ? 'It started together with Houndbit and stops when you quit Houndbit.' : 'Houndbit is using a separate Protect program that was already running.'}</p>
    </div>
    <div class="card">
      <div class="card-head"><h2>1. Connect Houndbit</h2></div>
      ${step1}
    </div>
    ${s.paired ? `<div class="card" id="protect-folders">
      <div class="card-head"><h2>2. Pick folders Houndbit may use</h2></div>
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
    box.innerHTML = `<div class="card-head"><h2>2. Pick folders Houndbit may use</h2></div><p class="small" style="color:var(--danger)">${esc(e.message)}</p>`;
    return;
  }
  const grants = (g && g.grants) || [];
  const addRow = hosted ? `
    <div class="row mt">
      <input class="input" id="protect-new-folder" placeholder="Full folder path, e.g. C:\\Users\\you\\Projects\\demo" style="min-width:22rem">
      <button class="btn btn-primary" data-act="protectAddFolder" data-app="com.houndbit.app">Add folder</button>
    </div>` : '<p class="small muted mt">Add folders in the separate Protect program.</p>';

  const list = grants.length ? `
    <p class="muted">3. Try the safe sandbox: type the name of a file inside the folder and press <b>Try it</b>. Houndbit reads the snapshot and sets a change aside. Your real file stays as it was.</p>
    <table class="table"><thead><tr><th>Folder</th><th>Try the safe sandbox</th></tr></thead>
    <tbody>${grants.map((x, i) => `
      <tr>
        <td class="mono">${esc(x.folder || '')}</td>
        <td>
          <div class="row">
            <input class="input" id="protect-path-${i}" placeholder="a file in that folder, e.g. notes.txt" style="min-width:14rem">
            <button class="btn btn-primary btn-small" data-act="protectCheck" data-grant="${esc(x.grant_id || '')}" data-idx="${i}">Try it</button>
          </div>
          <div id="protect-result-${i}"></div>
        </td>
      </tr>`).join('')}</tbody></table>`
    : '<p class="muted">No folders yet. Add one below.</p>';

  box.innerHTML = `<div class="card-head"><h2>2. Pick folders Houndbit may use</h2></div>${list}${addRow}`;
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
          const fs = (a.grants || []).filter(g => g.app_id === x.app_id).map(g => `<div class="mono small">${esc(g.folder)}</div>`).join('') || '<span class="muted small">none yet</span>';
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
  if (out) out.innerHTML = '<p class="small muted mt-xs">Trying it…</p>';
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
      ${changes.length ? `<p class="small muted">Changes set aside: ${changes.map(c => `${esc(c.path)} (${esc(c.kind)})`).join(', ')}</p>` : ''}
    </div>`;
};
