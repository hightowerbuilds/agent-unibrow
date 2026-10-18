// The home page's live sections (homepage roadmap H2) and actions (H3).
// Reads /api/state and draws it; buttons send a short list of commands to
// /api/cmd with this launch's token. Everything from the data goes in as
// text, never as HTML: titles come from web pages.
'use strict';

const $ = (sel) => document.querySelector(sel);
const TOKEN = (document.querySelector('meta[name="unibrow-token"]') || {}).content || '';

function el(tag, attrs, ...kids) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v === null || v === undefined || v === false) continue;
    if (k === 'class') e.className = v;
    else if (k === 'onclick') e.addEventListener('click', v);
    else e.setAttribute(k, v);
  }
  for (const k of kids) {
    if (k === null || k === undefined || k === false) continue;
    e.append(k instanceof Node ? k : document.createTextNode(String(k)));
  }
  return e;
}

/// Asks the app to do something. The app checks it, does it, and has the
/// page refresh.
async function cmd(body) {
  try {
    const r = await fetch('/api/cmd', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', 'X-Unibrow-Token': TOKEN },
      body: JSON.stringify(body),
    });
    return r.ok;
  } catch (e) {
    return false;
  }
}

function button(label, body, cls) {
  return el('button', { type: 'button', class: cls || 'small', onclick: (ev) => { ev.preventDefault(); cmd(body); } }, label);
}

/// "3 min ago", "yesterday", "12 Oct".
function ago(secs) {
  if (!secs) return '';
  const d = Date.now() / 1000 - secs;
  if (d < 60) return 'just now';
  if (d < 3600) return `${Math.floor(d / 60)} min ago`;
  if (d < 86400) return `${Math.floor(d / 3600)} h ago`;
  if (d < 2 * 86400) return 'yesterday';
  if (d < 7 * 86400) return `${Math.floor(d / 86400)} days ago`;
  return new Date(secs * 1000).toLocaleDateString(undefined, { day: 'numeric', month: 'short' });
}

/// A site's icon, or its first letter when there's none.
function icon(site) {
  const letter = el('span', { class: 'letter' }, (site || '?').replace(/^www\./, '').charAt(0).toUpperCase());
  const img = el('img', { class: 'icon', alt: '', src: `/icon/${encodeURIComponent(site)}.png` });
  const box = el('span', { class: 'iconbox' }, img, letter);
  img.addEventListener('load', () => box.classList.add('has-icon'));
  return box;
}

/// Only web addresses become links.
function link(url, ...kids) {
  const ok = typeof url === 'string' && /^https?:\/\//i.test(url);
  return ok ? el('a', { href: url, class: 'rowmain' }, ...kids) : el('span', { class: 'rowmain' }, ...kids);
}

function row(main, ...actions) {
  const a = actions.filter(Boolean);
  return el('li', {}, main, a.length ? el('span', { class: 'actions' }, ...a) : null);
}

function section(id, title, hint, rows) {
  const box = $(`#${id}`);
  box.replaceChildren(el('h2', {}, title));
  box.hidden = false;
  if (!rows.length) {
    box.append(el('p', { class: 'hint' }, hint));
    return;
  }
  box.append(el('ul', { class: 'rows' }, ...rows));
}

const plural = (n, word) => `${n} ${word}${n === 1 ? '' : 's'}`;

function jevButton(f) {
  const line = f.jev || '';
  if (line.startsWith('Jev learning')) return el('button', { type: 'button', class: 'small', disabled: 'disabled' }, 'Learning…');
  if (line.startsWith('Jev ready')) return button('Jev off', { cmd: 'toggle_jev', site: f.site });
  if (line.startsWith('Jev off')) return button('Jev on', { cmd: 'toggle_jev', site: f.site });
  if (line.startsWith("Jev couldn't")) return button('Retry Jev', { cmd: 'learn_jev', site: f.site });
  return button('Learn with Jev', { cmd: 'learn_jev', site: f.site });
}

let last = null;

function draw(s) {
  last = s;
  drawPins(s.pins);
  const pinned = new Set(s.pins.map((p) => p.url));
  section('recent', 'Recent', 'Sites you visit show up here.', s.recent.map((r) =>
    row(link(r.url, icon(r.site), el('span', { class: 'main' }, r.title || r.site), el('span', { class: 'meta' }, `${r.site} · ${ago(r.at)}`)),
      pinned.has(r.url) ? null : button('Pin', { cmd: 'pin', url: r.url, title: r.title || r.site }))));

  section('bookmarks', 'Bookmarks', 'Star a page in the toolbar to keep it here.', s.bookmarks.map((b) =>
    row(link(b.url, icon(b.site), el('span', { class: 'main' }, b.title || b.url), el('span', { class: 'meta' }, b.site)),
      button('Remove', { cmd: 'remove_bookmark', url: b.url }))));

  section('focus', 'Focus sites', 'No focus sites yet. Use ◎ Focus in the toolbar on a site you use a lot.', s.focus.map((f) =>
    row(link(f.url, icon(f.site), el('span', { class: 'main' }, f.label || f.site),
      el('span', { class: 'meta' }, [f.site, f.jev || 'Not learned by Jev', f.playbooks ? plural(f.playbooks, 'playbook') : null].filter(Boolean).join(' · '))),
      jevButton(f),
      button('View', { cmd: 'show', tab: 'site', site: f.site }))));

  section('watches', 'Watches', 'Ask the agent to watch a page, and what it finds shows here.', s.watches.map((w) =>
    row(link(w.url, el('span', { class: 'main' }, w.label),
      el('span', { class: w.error ? 'meta warn' : 'meta' }, w.error ? `Couldn't check: ${w.error}` : [w.value, w.checked ? `checked ${ago(w.checked)}` : 'not checked yet'].filter(Boolean).join(' · '))),
      button('Check now', { cmd: 'check_watch', id: w.id }),
      button('Remove', { cmd: 'remove_watch', id: w.id }))));

  section('playbooks', 'Playbooks', 'Do something on a focus site and ask the agent to save it as a playbook.', s.playbooks.map((p) =>
    row(el('span', { class: 'rowmain' }, icon(p.site), el('span', { class: 'main' }, p.site), el('span', { class: 'meta' }, plural(p.count, 'playbook'))),
      button('Run…', { cmd: 'fill_chat', text: `Run my playbook on ${p.site} for ` }))));

  section('chats', 'Recent chats', 'Your chats with the agent show up here.', s.chats.map((c) =>
    row(el('span', { class: 'rowmain' }, el('span', { class: 'main' }, c.title || 'Untitled chat'), el('span', { class: 'meta' }, `${plural(c.items, 'message')} · ${ago(c.updated)}`)),
      button('Open', { cmd: 'show', tab: 'chat' }))));

  drawStatus(s.status);
  drawGuide(s.guide);
  drawActivity(s.trace, s.evals);
  applyLayout(s.layout, s.sections);
}

/// Pinned sites as tiles (H5.2), with move and unpin.
function drawPins(pins) {
  const box = $('#pins');
  if (!pins.length) {
    box.replaceChildren();
    box.hidden = true;
    box.dataset.empty = '1';
    return;
  }
  delete box.dataset.empty;
  box.hidden = false;
  box.replaceChildren(el('h2', {}, 'Pinned'), el('ul', { class: 'pins' }, ...pins.map((p, i) =>
    el('li', {},
      link(p.url, icon(p.site), el('span', { class: 'main' }, p.title || p.site)),
      el('span', { class: 'tools' },
        i > 0 ? el('button', { type: 'button', title: 'Move left', 'aria-label': 'Move left', onclick: () => cmd({ cmd: 'move_pin', url: p.url, by: -1 }) }, '←') : null,
        i < pins.length - 1 ? el('button', { type: 'button', title: 'Move right', 'aria-label': 'Move right', onclick: () => cmd({ cmd: 'move_pin', url: p.url, by: 1 }) }, '→') : null,
        el('button', { type: 'button', title: 'Unpin', 'aria-label': `Unpin ${p.title || p.site}`, onclick: () => cmd({ cmd: 'unpin', url: p.url }) }, '×'))))));
}

const NAMES = {
  pins: 'Pinned', recent: 'Recent', focus: 'Focus sites', bookmarks: 'Bookmarks', watches: 'Watches', playbooks: 'Playbooks',
  chats: 'Recent chats', ask: 'Ask the agent', tinker: 'Make it yours', guide: 'How to use it', activity: 'Recent activity',
  keys: 'Shortcuts', status: 'Agent and keys',
};

/// The order sections show in: the user's first, then the rest.
function orderOf(layout, sections) {
  const first = (layout.order || []).filter((id) => sections.includes(id));
  return first.concat(sections.filter((id) => !first.includes(id)));
}

/// Puts sections in the user's order and hides the ones they hid (H5.1).
function applyLayout(layout, sections) {
  if (!layout || !sections) return;
  const main = document.querySelector('main');
  const footer = $('#customizer');
  for (const id of orderOf(layout, sections)) {
    const sec = document.getElementById(id);
    if (!sec) continue;
    main.insertBefore(sec, footer);
    const hide = layout.hidden.includes(id) || sec.dataset.empty === '1';
    if (hide) sec.hidden = true;
    else if (!['pins'].includes(id)) sec.hidden = false;
    sec.classList.toggle('user-hidden', layout.hidden.includes(id));
  }
}

/// The Customize panel: show or hide, reorder, density, accent color.
function openCustomizer() {
  if (!last) return;
  const box = $('#customizer');
  const layout = last.layout;
  let order = orderOf(layout, last.sections);
  const hidden = new Set(layout.hidden);
  const list = el('ul', { class: 'layout-rows' });
  const drawList = () => {
    list.replaceChildren(...order.map((id, i) => {
      const check = el('input', { type: 'checkbox', id: `show-${id}` });
      check.checked = !hidden.has(id);
      check.addEventListener('change', () => (check.checked ? hidden.delete(id) : hidden.add(id)));
      const move = (by) => () => { const j = i + by; if (j < 0 || j >= order.length) return; [order[i], order[j]] = [order[j], order[i]]; drawList(); };
      return el('li', {}, check, el('label', { for: `show-${id}` }, NAMES[id] || id),
        el('button', { type: 'button', class: 'small', 'aria-label': `Move ${NAMES[id]} up`, onclick: move(-1) }, '↑'),
        el('button', { type: 'button', class: 'small', 'aria-label': `Move ${NAMES[id]} down`, onclick: move(1) }, '↓'));
    }));
  };
  drawList();
  const density = el('select', { id: 'density' }, el('option', { value: 'roomy' }, 'Roomy'), el('option', { value: 'compact' }, 'Compact'));
  density.value = layout.density || 'roomy';
  const accent = el('input', { type: 'color', id: 'accent' });
  accent.value = layout.accent || getComputedStyle(document.documentElement).getPropertyValue('--accent').trim() || '#5fc8c0';
  const close = () => { box.hidden = true; box.replaceChildren(); };
  const save = async () => {
    await cmd({ cmd: 'save_layout', layout: { hidden: [...hidden], order, density: density.value, accent: accent.value } });
    close();
    location.reload();
  };
  const reset = async () => { await cmd({ cmd: 'reset_layout' }); close(); location.reload(); };
  box.hidden = false;
  box.replaceChildren(
    el('h2', {}, 'Customize this page'),
    el('p', { class: 'hint' }, 'Tick what to show, and move sections up or down. Your own CSS or a whole page of your own go in the home folder: see Settings and files.'),
    list,
    el('div', { class: 'layout-opts' },
      el('label', {}, 'Spacing ', density),
      el('label', {}, 'Accent color ', accent)),
    el('p', { class: 'buttons' },
      el('button', { type: 'button', class: 'small', id: 'layout-save', onclick: save }, 'Save'),
      el('button', { type: 'button', class: 'small', onclick: close }, 'Cancel'),
      el('button', { type: 'button', class: 'small', onclick: reset }, 'Reset to default')));
  box.scrollIntoView({ block: 'nearest' });
}

/// The Explained guide as cards (H4.1), its examples as the Ask box's
/// suggestions, and its shortcut list (H4.4). The text lives in the app.
function drawGuide(g) {
  if (!g) return;
  const box = $('#guide');
  box.hidden = false;
  box.replaceChildren(el('h2', {}, 'How to use it'), el('ul', { class: 'guide' }, ...g.pages.map((p) =>
    el('li', {}, el('button', { type: 'button', onclick: () => cmd({ cmd: 'show', tab: 'explained', page: p.page }) },
      el('b', {}, p.tab), el('span', {}, p.lead || p.sections.join(' · ')))))));
  if (g.examples && g.examples.length) {
    const starters = $('.starters');
    starters.replaceChildren(...g.examples.slice(0, 6).map((text) => {
      const b = el('button', { type: 'button' }, text);
      b.addEventListener('click', () => { const box = $('#asktext'); box.value = text; box.focus(); });
      return b;
    }));
  }
  if (g.keys && g.keys.length) {
    $('.shortcuts').replaceChildren(...g.keys.flatMap((k) => [el('dt', {}, k.keys), el('dd', {}, k.what)]));
  }
}

/// What the agent did lately (H4.3), and in a source checkout the latest
/// eval run.
function drawActivity(trace, evals) {
  const box = $('#activity');
  box.hidden = false;
  const kids = [el('h2', {}, 'Recent activity')];
  if (trace && trace.length) {
    kids.push(el('ul', { class: 'tracelist' }, ...trace.map((t) =>
      el('li', {}, el('span', { class: 'kind' }, t.kind.replace('_', ' ')), el('span', { class: 'what' }, t.summary), el('span', { class: 'when' }, ago(t.at))))));
  } else {
    kids.push(el('p', { class: 'hint' }, 'Nothing yet this session.'));
  }
  kids.push(el('p', { class: 'buttons' }, button('Open the full trace', { cmd: 'show', tab: 'trace' })));
  if (evals) {
    const line = `Latest eval run (${evals.run}): ${evals.passed} of ${evals.total} passed` + (evals.failed.length ? `; failed: ${evals.failed.join(', ')}` : '');
    kids.push(el('p', { class: 'hint', id: 'evals' }, line));
  }
  box.replaceChildren(...kids);
}

function drawStatus(st) {
  const yes = (v) => (v ? 'set' : 'not set');
  const agent = el('select', { id: 'agent', 'aria-label': 'Agent' });
  const model = el('select', { id: 'model', 'aria-label': 'Model' });
  const fillModels = () => {
    const c = st.choices.find((c) => c.backend === agent.value);
    const models = c ? c.models : [];
    model.replaceChildren(...models.map((m) => el('option', { value: m.id }, m.label)));
    if (!models.length) model.append(el('option', { value: '' }, 'Default'));
    if (agent.value === st.backend) model.value = st.model_id;
  };
  for (const c of st.choices || []) agent.append(el('option', { value: c.backend }, c.label));
  agent.value = st.backend;
  agent.addEventListener('change', fillModels);
  fillModels();
  const use = el('button', { type: 'button', class: 'small', onclick: () => cmd({ cmd: 'set_agent', backend: agent.value, model: model.value }) }, 'Use');

  const status = $('#status');
  status.hidden = false;
  status.replaceChildren(
    el('h2', {}, 'Agent and keys'),
    el('dl', { class: 'facts' },
      el('dt', {}, 'Agent'), el('dd', {}, st.agent ? `${st.agent}${st.model ? ` · ${st.model}` : ''}${st.busy ? ' · working' : ' · idle'}` : 'Not started'),
      el('dt', {}, 'Change to'), el('dd', { class: 'picker' }, agent, model, use),
      el('dt', {}, 'Anthropic API key'), el('dd', {}, yes(st.keys.anthropic)),
      el('dt', {}, 'Jev'), el('dd', {}, yes(st.keys.jev)),
      el('dt', {}, 'SubQ'), el('dd', {}, yes(st.keys.subq)),
      el('dt', {}, 'Gmail'), el('dd', {}, st.keys.gmail)));
}

async function refresh() {
  try {
    const r = await fetch('/api/state', { cache: 'no-store' });
    if (r.ok) draw(await r.json());
  } catch (e) {
    // The static parts of the page still work.
  }
}

// Static parts: the ask box, its examples, and the cards.
$('#askform').addEventListener('submit', async (ev) => {
  ev.preventDefault();
  const box = $('#asktext');
  const text = box.value.trim();
  if (text && (await cmd({ cmd: 'ask', text }))) box.value = '';
});
for (const b of document.querySelectorAll('.starters button')) {
  b.addEventListener('click', () => { const box = $('#asktext'); box.value = b.dataset.text; box.focus(); });
}
for (const b of document.querySelectorAll('[data-show]')) {
  b.addEventListener('click', () => cmd({ cmd: 'show', tab: b.dataset.show }));
}

$('#customize').addEventListener('click', openCustomizer);

// The app calls this when something on the page changed.
window.home = Object.freeze({ refresh });
refresh();
