// unibrow://home/settings (homepage roadmap H4.2): read-only. Draws
// /api/settings; the only action is opening one of the app's folders.
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

function openFolder(which) {
  fetch('/api/cmd', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', 'X-Unibrow-Token': TOKEN },
    body: JSON.stringify({ cmd: 'open_folder', which }),
  }).catch(() => {});
}

const NAMES = { data: 'App data', traces: 'Traces', downloads: 'Downloads', config: 'Settings folder' };

function draw(s) {
  $('#where').append(
    el('dl', { class: 'facts paths' }, ...s.files.flatMap((f) => [el('dt', {}, f.what), el('dd', {}, el('code', {}, f.path || 'none'))])),
    el('p', { class: 'buttons' }, ...s.folders.map((w) => el('button', { type: 'button', class: 'small', onclick: () => openFolder(w) }, `Open ${NAMES[w] || w}`))));
  const rows = Object.entries(s.settings || {}).map(([k, v]) => [el('dt', {}, k), el('dd', {}, el('code', {}, typeof v === 'string' ? (v || '(default)') : JSON.stringify(v)))]);
  $('#settings').append(el('dl', { class: 'facts' }, ...rows.flat()));
  $('#policy').append(el('pre', { class: 'json' }, JSON.stringify(s.policy, null, 2)));
  $('#versions').append(el('dl', { class: 'facts' },
    el('dt', {}, 'agent-unibrow'), el('dd', {}, s.versions.app),
    el('dt', {}, 'Chromium (CEF build)'), el('dd', {}, s.versions.chromium)));
}

fetch('/api/settings', { cache: 'no-store' }).then((r) => (r.ok ? r.json() : null)).then((s) => s && draw(s)).catch(() => {});
