// Houndbit "Code changes" screen: local Git tracking and optional GitHub links.
// Loaded after app.js; reuses its helpers (api, esc, when, pill, status, emptyState, showToast, actions, changes, navigate, SCREENS, RENDER).
// Routes: #code (projects), #code/<repoId> (commits), #code/<repoId>_<sha> (one commit).
'use strict';

const FACT_PILL = {
  confirmed: ['green', 'Confirmed'],
  likely: ['blue', 'Likely'],
  no: ['red', 'No'],
  not_observed: ['amber', 'Not seen'],
  unknown: ['', 'Unknown'],
};
const OUTCOME_PILL = {
  shipped_as_is: 'green', shipped: 'green', revised: 'amber', unknown: '',
  not_staged: 'blue', staged_not_committed: 'blue', later_commit: 'blue', not_shipped: 'red',
};
const CONF_TEXT = { high: 'High confidence', medium: 'Medium confidence', low: 'Low confidence' };
const repoName = r => String(r.root || '').replace(/[\\/]+$/, '').split(/[\\/]/).pop() || r.root;
const commitWhen = secs => (secs ? `${when(secs * 1000)} (${new Date(secs * 1000).toLocaleString()})` : '');

function diffBlock(lines, empty = '(no line changes)') {
  if (!lines || !lines.length) return `<pre class="diff">${esc(empty)}</pre>`;
  const sign = { add: '+ ', del: '- ', ctx: '  ' };
  const html = lines.map(l => (l.kind === 'gap'
    ? `<span class="diff-gap">${esc(l.text || '...')}</span>`
    : `<span class="diff-${l.kind}">${esc(sign[l.kind] + l.text)}</span>`)).join('');
  return `<pre class="diff">${html}</pre>`;
}

SCREENS.push('code');
RENDER.code = async function render_code() {
  const p = state.param || '';
  if (p.includes('_')) {
    const [repo, sha] = p.split('_');
    return renderCommit(repo, sha);
  }
  if (p) return renderCommits(p);
  return renderProjects();
};

// =====================================================================================================
// Projects
// =====================================================================================================
async function renderProjects() {
  const d = await api('GET', '/api/ui/git/repos');
  const repos = d.repos || [];
  screenEl.innerHTML = `
    <div class="page-head">
      <h1>Code changes</h1>
      <p class="lead">Pick a Git project folder to see which commits hold your agent's edits, and whether what shipped is what the agent proposed. Nothing in your project is changed.</p>
    </div>
    ${d.tracking ? '' : `<div class="note note-warn mb-sm">Recording agent edits for Git is turned off, so new edits won't show up here.
      <button class="btn btn-small btn-secondary" data-act="git-tracking-on">Turn it on</button></div>`}
    <div class="card">
      <div class="card-head row row-between"><h3>Your projects</h3>
        ${repos.length ? `<button class="btn btn-secondary btn-small" data-act="git-fetch-all" data-count="${repos.length}">Check all projects</button>` : ''}</div>
      ${repos.length ? checkAllBox(repos) : ''}
      ${repos.length ? `<div class="task-list">${repos.map(projectRow).join('')}</div>`
        : emptyState('box', 'No projects yet', 'Add the folder of a project that uses Git. Houndbit will remember edits your agent makes there from now on.')}
      <label class="field mt"><span class="field-label">Add a project folder</span>
        <div class="input-row"><input type="text" id="git-path" placeholder="For example C:\\Users\\Greg\\projects\\frontier-warp" aria-label="Project folder">
          <button class="btn btn-secondary" data-act="git-browse">Browse...</button>
          <button class="btn btn-primary" data-act="git-add">Add</button></div>
        <span class="field-hint">Any folder inside the project works. Houndbit finds the top of the project for you.</span></label>
    </div>
    <div class="card">
      <h3>GitHub links <span class="muted small">(optional)</span></h3>
      <p class="small">With a read-only access token, a commit page can show its link on GitHub, any pull requests that include it, and whether GitHub's branch has it. Only the project name, commit id and branch names are sent, and only for projects where you turn it on.</p>
      <p class="small">${d.has_token ? status('ok', 'A token is saved on this computer') : status('muted', 'No token saved')}</p>
      <div class="input-row"><input type="password" id="git-token" placeholder="${d.has_token ? 'Paste a new token to replace it' : 'Paste a GitHub token'}" aria-label="GitHub token" autocomplete="off">
        <button class="btn btn-primary" data-act="git-token-save">Save</button>
        ${d.has_token ? '<button class="btn btn-secondary" data-act="git-token-clear">Remove</button>' : ''}</div>
      <p class="field-hint mt-xs">Suggested: a fine-grained token limited to the projects you choose, with read-only "Contents" and "Pull requests" access. Public projects work without a token, with GitHub's lower request limit.</p>
    </div>
    <div class="note note-plain mt">Your prompts and the agent's reasoning stay on this computer. They are never part of this screen or sent to GitHub.</div>`;
}

function projectRow(r) {
  const gh = r.github_owner ? `${r.github_owner}/${r.github_repo}` : '';
  return `
    <div class="task-row">
      <div class="task-row-main">
        <div class="task-row-title">${esc(repoName(r))}</div>
        <div class="task-row-meta mono">${esc(r.root)}</div>
        <div class="task-row-meta">${r.remote_url ? `Remote: ${esc(r.remote_url)}` : 'No remote'}</div>
        ${r.remote_url ? `<div class="task-row-meta">${r.last_fetched_at ? `Last checked: ${esc(when(r.last_fetched_at))}` : 'Not checked from here yet'}</div>` : ''}
        ${gh ? `<label class="toggle-row small"><input type="checkbox" data-change="git-github" data-id="${esc(r.id)}" ${r.github_enabled ? 'checked' : ''}>
          <span>Show GitHub links for <b>${esc(gh)}</b></span></label>` : ''}
      </div>
      <div class="row">
        <button class="btn btn-primary btn-small" data-act="go" data-screen="code" data-param="${esc(r.id)}">See commits</button>
        <button class="btn btn-secondary btn-small" data-act="git-remove" data-id="${esc(r.id)}" data-name="${esc(repoName(r))}">Remove</button>
      </div>
    </div>`;
}

// "Check all projects": the last result stays on screen until the page is left.
let lastCheckAll = null;
const CHECK_PILL = {
  changed: ['green', 'New changes'],
  up_to_date: ['blue', 'Up to date'],
  skipped: ['', 'Only on this computer'],
  failed: ['red', 'Couldn\'t check'],
};
function checkAllBox(repos) {
  if (!lastCheckAll) {
    return '<p class="small muted mt-xs">"Check all projects" asks every project\'s remote for its latest branches, a few at a time. Your files and branches are not changed.</p>';
  }
  const ids = new Set(repos.map(r => r.id));
  const rows = lastCheckAll.results.filter(x => ids.has(x.repo_id));
  return `<div class="card card-soft mt-xs" aria-live="polite">
      <p class="m0"><b>${esc(lastCheckAll.summary)}</b></p>
      <ul class="small mt-xs">${rows.map(x => `<li>${pill(...(CHECK_PILL[x.status] || ['', x.status]))} <b>${esc(x.name)}</b>: ${esc(x.message)}${x.last_fetched_at ? ` <span class="muted">(last checked ${esc(when(x.last_fetched_at))})</span>` : ''}</li>`).join('')}</ul>
    </div>`;
}
actions['git-fetch-all'] = async (d, el) => {
  const n = Number(d.count) || 0;
  el.textContent = `Checking ${n} project${n === 1 ? '' : 's'}...`;
  try {
    lastCheckAll = await api('POST', '/api/ui/git/fetch-all');
    showToast(lastCheckAll.summary, lastCheckAll.failed ? 'warn' : 'success');
  } finally {
    if (el.isConnected) el.textContent = 'Check all projects';
  }
  if ((state.param || '') === '' && location.hash.startsWith('#code')) await renderProjects();
};

actions['git-browse'] = async () => {
  const r = await api('POST', '/api/ui/pick-folder');
  if (!r.supported) {
    showToast('The folder picker isn\'t available here. Please type or paste the folder path.', 'info');
    $('#git-path').focus();
    return;
  }
  if (r.path) $('#git-path').value = r.path;
};
actions['git-add'] = async () => {
  const path = $('#git-path').value.trim();
  if (!path) { showToast('Type or pick a project folder first.', 'warn'); return; }
  const r = await api('POST', '/api/ui/git/repos', { path });
  showToast(`Added ${repoName(r)}.`, 'success');
  await renderProjects();
};
actions['git-remove'] = async d => {
  if (!confirm(`Stop showing ${d.name} here? Your project folder isn't touched, and recorded edits are kept.`)) return;
  await api('DELETE', `/api/ui/git/repos/${encodeURIComponent(d.id)}`);
  await renderProjects();
};
actions['git-token-save'] = async () => {
  const token = $('#git-token').value.trim();
  if (!token) { showToast('Paste a token first.', 'warn'); return; }
  await api('POST', '/api/ui/git/github-token', { token });
  showToast('Token saved on this computer.', 'success');
  await renderProjects();
};
actions['git-token-clear'] = async () => {
  await api('POST', '/api/ui/git/github-token', { token: '' });
  showToast('Token removed.', 'success');
  await renderProjects();
};
actions['git-tracking-on'] = async () => {
  await api('POST', '/api/ui/settings/save', { git_tracking: true });
  showToast('Agent edits will be recorded for Git from now on.', 'success');
  await renderProjects();
};
changes['git-github'] = async el => {
  try {
    await api('POST', `/api/ui/git/repos/${encodeURIComponent(el.dataset.id)}/github`, { enabled: el.checked });
    showToast(el.checked ? 'GitHub links turned on for this project.' : 'GitHub links turned off for this project.', 'success');
  } catch (e) {
    el.checked = !el.checked;
    throw e;
  }
};

// =====================================================================================================
// Check the remote now (git fetch; only the remote-tracking branches change)
// =====================================================================================================
function remoteCheckBox(r) {
  if (!r || !r.remote_url) {
    return '<p class="small muted m0">This project has no remote, so there is nothing to check.</p>';
  }
  const last = r.last_fetched_at ? `Last checked: ${esc(when(r.last_fetched_at))}` : 'Not checked from here yet';
  return `<div class="row row-between">
      <p class="small m0"><span class="muted">${last}.</span> Checking asks the remote for its latest branches. Your files and branches are not changed.</p>
      <button class="btn btn-secondary btn-small" data-act="git-fetch" data-id="${esc(r.id)}">Check the remote now</button>
    </div>`;
}
actions['git-fetch'] = async (d, el) => {
  const label = el.textContent;
  el.textContent = 'Checking...';
  try {
    const r = await api('POST', `/api/ui/git/repos/${encodeURIComponent(d.id)}/fetch`);
    showToast(r.message, r.changed ? 'success' : 'info');
  } finally {
    if (el.isConnected) el.textContent = label;
  }
  await RENDER.code();
};

// =====================================================================================================
// Commits
// =====================================================================================================
async function renderCommits(repoId) {
  const [{ repos }, { commits }] = await Promise.all([api('GET', '/api/ui/git/repos'), api('GET', `/api/ui/git/repos/${encodeURIComponent(repoId)}/commits?limit=100`)]);
  const r = (repos || []).find(x => x.id === repoId) || { root: '' };
  screenEl.innerHTML = `
    <div class="page-head">
      <button class="btn btn-link" data-act="go" data-screen="code">&larr; All projects</button>
      <h1>${esc(repoName(r))}</h1>
      <p class="lead">Pick a commit to see which agent edits are in it and how they compare with what the agent proposed.</p>
    </div>
    <div class="card card-soft">${remoteCheckBox(r)}</div>
    <div class="card">${commits.length ? `<table class="table">
      <thead><tr><th>Commit</th><th>Author</th><th>When</th></tr></thead>
      <tbody>${commits.map(c => `
        <tr class="clickable" data-act="go" data-screen="code" data-param="${esc(repoId)}_${esc(c.sha)}">
          <td><div class="task-row-title">${esc(c.subject || '(no message)')}</div><div class="mono muted">${esc(c.short)}</div></td>
          <td class="small">${esc(c.author_name)}</td>
          <td class="small">${esc(when(c.commit_time * 1000))}</td>
        </tr>`).join('')}</tbody></table>`
      : emptyState('clipboard', 'No commits yet', 'Once something is committed in this project, it shows up here.')}</div>`;
}

// =====================================================================================================
// One commit
// =====================================================================================================
const FACT_TITLES = [
  ['edit_attribution', 'Agent edits in this commit'],
  ['commit_authorship', 'Who made the commit'],
  ['agent_push', 'Did the agent run git push?'],
  ['remote_update', 'Does the remote branch have it?'],
];

function factCard([key, title], f, repo) {
  return `
    <div class="card fact-card">
      <div class="row row-between"><h3>${esc(title)}</h3>${pill(...(FACT_PILL[f.status] || ['', f.status]))}</div>
      <p>${esc(f.label)}</p>
      ${f.evidence && f.evidence.length ? `<details class="small"><summary>How we know</summary><ul>${f.evidence.map(e => `<li>${esc(e)}</li>`).join('')}</ul></details>` : ''}
      ${key === 'remote_update' && repo && repo.remote_url ? `<div class="mt-xs">${remoteCheckBox(repo)}</div>` : ''}
    </div>`;
}

function decisionLine(d) {
  const open = d.open ? `<button class="btn btn-link btn-small" data-act="code-open" data-href="${esc(d.open)}">Open the record</button>` : '';
  return `<dl class="kv small">
    <dt>Decision record</dt><dd><span class="mono">${esc(d.decision_id)}</span> ${open}</dd>
    <dt>Where it came from</dt><dd>${esc(d.source_label)}, ${esc(connectorName(d.connector))}${d.tool ? `, ${esc(d.tool)} tool` : ''}</dd>
    ${d.task_title ? `<dt>Task</dt><dd>${esc(d.task_title)}</dd>` : ''}
    <dt>Seen</dt><dd>${esc(when(d.observed_at))}</dd>
  </dl>`;
}
actions['git-link'] = async d => {
  await api('POST', '/api/ui/git/open-link', { url: d.url });
  showToast('Opened in your web browser.', 'info');
};
actions['code-open'] = d => { location.hash = d.href; };

function evidenceLine(m) {
  const b = m.before_evidence || {};
  const a = m.after_evidence || {};
  return `<dl class="kv small">
    <dt>File before the edit</dt><dd>${esc(b.label || 'Not available')}${b.blob ? ` <span class="mono muted">${esc(String(b.blob).slice(0, 12))}</span>` : ''}</dd>
    <dt>File after the edit</dt><dd>${esc(a.label || 'Not available')}${a.blob ? ` <span class="mono muted">${esc(String(a.blob).slice(0, 12))}</span>` : ''}</dd>
  </dl>`;
}

function editCard(m, shippedFile) {
  const head = `
    <div class="row row-between">
      <div><div class="task-row-title mono">${esc(m.path)}</div></div>
      <div class="row">${pill(OUTCOME_PILL[m.outcome] || '', m.outcome_label)}${pill('', CONF_TEXT[m.confidence] || m.confidence)}</div>
    </div>`;
  const basis = `<div class="mt-xs"><b class="small">Why we think so</b><ul class="small">${(m.basis || []).map(b => `<li>${esc(b)}</li>`).join('')}</ul></div>`;
  let compare;
  if (m.outcome === 'revised') {
    const reworded = (m.agent_lines_reworded || []).map(r => `
      <tr><td><span class="mono line-proposed">${esc(r.proposed)}</span></td><td><span class="mono line-shipped">${esc(r.shipped)}</span></td></tr>`).join('');
    compare = `
      <div class="note note-warn mt-xs">Someone changed this edit before it was committed. The left side is what the agent proposed; the right side is what actually shipped.</div>
      <div class="compare mt-xs">
        <div><div class="compare-title">What the agent proposed</div>${diffBlock(m.proposed)}</div>
        <div><div class="compare-title">What actually shipped</div>${shippedFile ? diffBlock(shippedFile.diff) : '<pre class="diff">(not available)</pre>'}</div>
      </div>
      ${reworded ? `<table class="table mt-xs"><thead><tr><th>The agent wrote</th><th>It shipped as</th></tr></thead><tbody>${reworded}</tbody></table>` : ''}
      ${(m.agent_lines_not_shipped || []).length ? `<p class="small mt-xs"><b>Agent lines that didn't ship:</b></p>${diffBlock(m.agent_lines_not_shipped.map(t => ({ kind: 'add', text: t })))}` : ''}
      ${(m.agent_removals_kept || []).length ? `<p class="small mt-xs"><b>Lines the agent removed that are still there:</b></p>${diffBlock(m.agent_removals_kept.map(t => ({ kind: 'del', text: t })))}` : ''}`;
  } else {
    compare = `<details class="mt-xs"><summary class="small">What the agent proposed</summary>${diffBlock(m.proposed)}</details>`;
  }
  return `<div class="card edit-card">${head}${decisionLine(m.decision)}${basis}${compare}
    <details class="mt-xs"><summary class="small">Before and after evidence</summary>${evidenceLine(m)}</details></div>`;
}

async function renderCommit(repoId, sha) {
  screenEl.innerHTML = `<div class="card"><span class="spinner"></span> Comparing this commit with the agent's recorded edits...</div>`;
  const r = await api('GET', `/api/ui/git/repos/${encodeURIComponent(repoId)}/commits/${encodeURIComponent(sha)}`);
  const c = r.commit;
  const s = r.summary;
  const gh = r.github;
  const byPath = Object.fromEntries((r.files || []).map(f => [f.path, f]));
  const ghLinks = gh ? `
    <div class="card">
      <h3>On GitHub</h3>
      ${gh.error ? `<p>${status('warn', gh.error)}</p>` : ''}
      ${gh.commit_found === false ? `<p>${status('warn', 'GitHub doesn\'t have this commit (yet).')}</p>` : ''}
      ${gh.commit_found ? `<p><a href="${esc(gh.commit_url)}" data-act="git-link" data-url="${esc(gh.commit_url)}">Open this commit on GitHub</a></p>` : ''}
      ${(gh.pulls || []).length ? `<p class="small"><b>Pull requests that include it</b></p><ul>${gh.pulls.map(p => `<li><a href="${esc(p.url)}" data-act="git-link" data-url="${esc(p.url)}">#${esc(p.number)} ${esc(p.title)}</a> ${pill(p.merged ? 'green' : p.state === 'open' ? 'blue' : '', p.merged ? 'Merged' : p.state)}</li>`).join('')}</ul>`
        : gh.commit_found ? '<p class="small muted">No pull requests include this commit.</p>' : ''}
    </div>` : '';
  screenEl.innerHTML = `
    <div class="page-head">
      <button class="btn btn-link" data-act="go" data-screen="code" data-param="${esc(repoId)}">&larr; ${esc(repoName(r.repo))} commits</button>
      <h1>${esc(c.subject || '(no message)')}</h1>
      <p class="lead"><span class="mono">${esc(c.short)}</span> by ${esc(c.author_name)}, ${esc(commitWhen(c.commit_time))}</p>
    </div>
    <div class="card card-soft">
      <p class="m0">This commit adds <b>${s.lines_added}</b> line(s) in ${s.files} file(s):
        <b>${s.agent_lines_as_proposed}</b> match the agent's edits word for word,
        <b>${s.agent_lines_reworded}</b> are agent lines someone reworded, and
        <b>${s.other_lines}</b> come from someone else.</p>
    </div>
    <h2 class="mt">Four separate questions</h2>
    <div class="grid-2">${FACT_TITLES.map(t => factCard(t, r.facts[t[0]], r.repo)).join('')}</div>
    ${ghLinks}
    <h2 class="mt">Agent edits in this commit</h2>
    ${r.edits.length ? r.edits.map(m => editCard(m, byPath[m.path])).join('')
      : `<div class="card">${emptyState('ear', 'No recorded agent edits are in this commit', 'Either a person made these changes, or Houndbit wasn\'t recording when the agent made them.')}</div>`}
    ${r.not_in_commit.length ? `<h2 class="mt">Agent edits that aren't in this commit</h2>
      <p class="muted small">The agent made these since the previous commit, but they didn't go into this one.</p>
      ${r.not_in_commit.map(m => editCard(m, null)).join('')}` : ''}
    ${r.other_sources.length ? `<h2 class="mt">Work from someone else</h2>
      <div class="card"><p class="small muted">These added lines don't come from any recorded agent edit. A person, another tool, or an agent Houndbit wasn't watching made them.</p>
      ${r.other_sources.map(o => `<p class="small mt-xs"><b class="mono">${esc(o.path)}</b>: ${o.lines} line(s)</p>${diffBlock(o.sample.map(t => ({ kind: 'add', text: t })))}`).join('')}</div>` : ''}
    <h2 class="mt">Everything in this commit</h2>
    <div class="card">${(r.files || []).map(f => `
      <details><summary><span class="mono">${esc(f.path)}</span> <span class="muted small">${esc(f.status)}, +${f.added} -${f.removed}</span></summary>
        ${f.binary ? '<p class="small muted">Binary or very large file; no line view.</p>' : diffBlock(f.diff)}</details>`).join('') || '<p class="muted">No file changes.</p>'}</div>
    <div class="note note-plain mt">${esc(r.privacy)}</div>`;
}
