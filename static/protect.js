// Houndbit Protect panel. Loaded after app.js and reuses its helpers
// (api, esc, pill, showToast, emptyState, navigate, actions, SCREENS, RENDER, screenEl).
// Houndbit talks to Protect as an ordinary paired app over Protect's own local endpoint.
'use strict';

SCREENS.push('protect');
RENDER.protect = async function render_protect() {
  screenEl.innerHTML = `
    <div class="page-head">
      <h1>Houndbit Protect</h1>
      <p class="lead">Protect is a separate little helper that keeps an agent's file work inside a safe copy of your folder. Houndbit connects to it the same way any other program would, with its own key. You pair it once, pick which folders it may see, and Protect runs the work against a snapshot so your real files are never written.</p>
    </div>
    <div id="protect-body"><div class="card"><p class="muted">Checking on Protect…</p></div></div>`;
  await refreshProtect();
};

async function refreshProtect() {
  const body = document.getElementById('protect-body');
  if (!body) return;
  let s;
  try {
    s = await api('GET', '/api/ui/runtime/protect/status');
  } catch (e) {
    body.innerHTML = `<div class="card">${emptyState('ear', 'Could not check Protect', e.message)}</div>`;
    return;
  }

  if (!s.running) {
    body.innerHTML = `<div class="card">
      ${emptyState('shield', 'Protect isn\u2019t running', s.message || 'Start the Houndbit Protect helper, then come back and pair it.')}
      <div class="row"><button class="btn btn-secondary" data-act="protectRefresh">Check again</button></div>
      <p class="small muted mt">Protect runs on its own, next to Houndbit. It listens at <span class="mono">${esc(s.endpoint || '')}</span>.</p>
    </div>`;
    return;
  }

  const statePill = s.paired ? pill('green', 'Connected') : pill('amber', 'Not paired yet');
  const scopes = Array.isArray(s.scopes) ? s.scopes.join(', ') : '';
  body.innerHTML = `
    <div class="card">
      <div class="card-head"><h2>Connection</h2>${statePill}</div>
      <dl class="kv">
        <dt>This app</dt><dd>${esc(s.display_name || 'Houndbit')} <span class="mono muted">${esc(s.app_id || '')}</span></dd>
        <dt>Protect endpoint</dt><dd class="mono">${esc(s.endpoint || '')}</dd>
        ${s.public_key ? `<dt>Houndbit's key</dt><dd class="mono">${esc(s.public_key)}</dd>` : ''}
        ${s.paired && scopes ? `<dt>What Houndbit may do</dt><dd>${esc(scopes)}</dd>` : ''}
      </dl>
      <p class="muted">${esc(s.message || '')}</p>
      <div class="row">
        ${s.paired
          ? '<button class="btn btn-secondary" data-act="protectRefresh">Refresh</button>'
          : '<button class="btn btn-primary" data-act="protectPair">Pair Houndbit with Protect</button><button class="btn btn-secondary" data-act="protectRefresh">Check again</button>'}
      </div>
      ${s.paired ? '' : '<p class="small muted mt">Pairing asks Protect for permission. You then accept it inside the Protect window, and choose which folders Houndbit may see.</p>'}
    </div>
    ${s.paired ? '<div id="protect-grants"><div class="card"><p class="muted">Loading your granted folders…</p></div></div>' : ''}`;

  if (s.paired) await refreshGrants();
}

async function refreshGrants() {
  const box = document.getElementById('protect-grants');
  if (!box) return;
  let g;
  try {
    g = await api('GET', '/api/ui/runtime/protect/grants');
  } catch (e) {
    box.innerHTML = `<div class="card">${emptyState('folder', 'Could not load folders', e.message)}</div>`;
    return;
  }
  const grants = (g && g.grants) || [];
  if (!grants.length) {
    box.innerHTML = `<div class="card">${emptyState('folder', 'No folders yet', 'Open the Protect window and grant Houndbit a folder. It will show up here, and you can try the safe sandbox on it.')}</div>`;
    return;
  }
  box.innerHTML = `
    <div class="card">
      <div class="card-head"><h2>Folders Protect lets Houndbit use</h2></div>
      <p class="muted">Try the sandbox on any file inside a granted folder. Houndbit reads a snapshot and stages a change; your real file is never touched.</p>
      <table class="table"><thead><tr><th>Folder</th><th>Try the safe sandbox</th></tr></thead>
      <tbody>${grants.map((x, i) => `
        <tr>
          <td class="mono">${esc(x.folder || '')}<div class="mono muted">${esc(x.grant_id || '')}</div></td>
          <td>
            <div class="row">
              <input class="input" id="protect-path-${i}" placeholder="file inside the folder, e.g. notes.txt" style="min-width:16rem">
              <button class="btn btn-primary btn-small" data-act="protectCheck" data-grant="${esc(x.grant_id || '')}" data-idx="${i}">Run safe check</button>
            </div>
            <div id="protect-result-${i}"></div>
          </td>
        </tr>`).join('')}</tbody></table>
    </div>`;
}

actions.protectRefresh = async () => { await refreshProtect(); };

actions.protectPair = async () => {
  const r = await api('POST', '/api/ui/runtime/protect/pair', {});
  showToast(r.message || 'Pairing requested. Accept it in the Protect window.', 'success');
  await refreshProtect();
};

actions.protectCheck = async (d) => {
  const input = document.getElementById('protect-path-' + d.idx);
  const out = document.getElementById('protect-result-' + d.idx);
  const path = (input && input.value || '').trim();
  if (!path) { showToast('Type a file path inside the folder first.', 'error'); return; }
  if (out) out.innerHTML = '<p class="small muted mt-xs">Running the safe check…</p>';
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
      <div>${ok ? pill('green', 'Your original was not touched') : pill('amber', 'Check the result')}</div>
      <p class="small">${esc(r.message || '')}</p>
      ${changes.length ? `<p class="small muted">Staged changes: ${changes.map(c => `${esc(c.path)} (${esc(c.kind)})`).join(', ')}</p>` : ''}
    </div>`;
};
