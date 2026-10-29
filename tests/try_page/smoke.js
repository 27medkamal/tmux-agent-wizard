// Smoke run of docs/try/index.html: loads the page's script in a vm with a small DOM stand-in and
// draws it. Run by tests/try_page.rs with `node tests/try_page/smoke.js docs/try/index.html`;
// exit 0 and one summary line when every drawing is sound, exit 1 and the problems otherwise.
// What it fails on: any exception, a row count other than 34, a row wider than the terminal,
// `undefined` or `NaN` in a row, and a glyph a screen draws that is not one cell wide by class.
'use strict';
const fs = require('fs');
const vm = require('vm');

function load(html, opts = {}) {
  let script = html.slice(html.indexOf('<script>') + 8, html.lastIndexOf('</script>'));
  if (opts.peek) script = script.replace(/\}\)\(\);\s*$/, 'globalThis.__peek = () => ({ agentScreen, SCREENS, PREV }); })();');
  let clock = 1_000_000; const errors = [];
  let measures = 0;
  const rect = () => (measures++, { top: 0, left: 0, bottom: opts.rectH || 0, right: 0, width: opts.rectW || 0, height: opts.rectH || 0 });
  class El {
    constructor(tag, attrs = {}) { this.tagName = tag.toUpperCase(); this.attrs = attrs; this.id = attrs.id || ''; this._html = ''; this.textContent = ''; this.value = attrs.value || ''; this.checked = !!attrs.checked; this.dataset = {}; this.listeners = {}; this.classSet = new Set(); this.parentElement = new Proxy({}, { get: (t, k) => k === 'classList' ? { toggle() {}, add() {}, remove() {}, contains: () => false } : undefined }); this.style = { setProperty() {} }; this.clientWidth = opts.clientWidth || 1400; this.offsetWidth = 100; this.offsetHeight = 20; }
    addEventListener(t, f) { (this.listeners[t] = this.listeners[t] || []).push(f); }
    fire(t, ev) { for (const f of (this.listeners[t] || [])) f(ev); }
    get classList() { const s = this.classSet; return { toggle: (c, on) => { if (on === undefined) on = !s.has(c); if (on) s.add(c); else s.delete(c); }, add: c => s.add(c), remove: c => s.delete(c), contains: c => s.has(c) }; }
    focus() { this.fire('focus', {}); } closest() { return null; }
    set innerHTML(v) { this._html = v; } get innerHTML() { return this._html; }
    get children() { return []; } querySelectorAll() { return []; } dispatchEvent() {} append() {} appendChild() {} setAttribute() {}
    getBoundingClientRect() { return rect(); }
  }
  class Row extends El { constructor(h) { super('div'); this._html = h; } closest(s) { return s === '.row' ? this : null; } }
  class Term extends El {
    set innerHTML(v) { const rows = []; const re = /<div class="row">([\s\S]*?)<\/div>/g; let m; while ((m = re.exec(v))) rows.push(new Row(m[1])); this.rows = rows; this._raw = v; for (const o of observers) o(); }
    get innerHTML() { return (this.rows || []).map(r => `<div class="row">${r.innerHTML}</div>`).join(''); }
    querySelectorAll(s) { return s === '.row' ? (this.rows || []) : []; } get children() { return this.rows || []; }
  }
  const observers = [];
  const byId = {}; const term = byId.term = new Term('div', { id: 'term' });
  byId.nameprobe = new El('input', { id: 'nameprobe', value: '~/oss/api' });
  const radios = [];
  for (const m of html.matchAll(/<input type="radio" name="(\w+)" value="([^"]+)"( checked)?>/g)) radios.push(new El('input', { name: m[1], value: m[2], checked: !!m[3] }));
  for (const m of html.matchAll(/<input type="checkbox" id="([\w-]+)"( checked)?>/g)) byId[m[1]] = new El('input', { id: m[1], checked: !!m[2] });
  const buttons = []; for (const m of html.matchAll(/<button data-sc="(\w+)">/g)) { const el = new El('button'); el.dataset.sc = m[1]; buttons.push(el); }
  const orderBody = new El('tbody'); const docListeners = []; const winListeners = {};
  const docEl = new El('html'); const body = new El('body'); const mainEl = new El('main');
  const document = { documentElement: docEl, body, getElementById: id => byId[id] || (byId[id] = new El('div', { id })), querySelector: s => (s === '#ordertable tbody' ? orderBody : s === 'main' ? mainEl : null), querySelectorAll: s => { const m = s.match(/^input\[name=(\w+)\]$/); if (m) return radios.filter(r => r.attrs.name === m[1]); if (s === '[data-sc]') return buttons; return []; }, addEventListener: (t, f) => docListeners.push([t, f]), createElement: t => new El(t) };
  const mq = q => ({ matches: q.includes('max-width: 599px') ? !!opts.phone : false, addEventListener() {} });
  const parentWin = opts.frame ? { innerHeight: opts.parentH || 860, document: { querySelector: () => ({ getBoundingClientRect: () => ({ bottom: 48 }) }), body: { getAttribute: () => null } }, getComputedStyle: () => ({ scrollMarginTop: '60px' }), addEventListener: (t, f) => (winListeners['p' + t] = winListeners['p' + t] || []).push(f) } : null;
  const window = { location: { search: opts.embed ? '?embed=1' : '' }, matchMedia: mq, scrollY: 0, innerHeight: opts.innerH || 860, frameElement: opts.frame ? { parentElement: new El('div'), style: {} } : null, addEventListener: (t, f) => (winListeners[t] = winListeners[t] || []).push(f) };
  window.parent = parentWin || window;
  class KeyboardEvent { constructor(t, e) { Object.assign(this, e); this.type = t; } }
  class MutationObserver { constructor(f) { this.f = f; } observe(t) { if (t === term) observers.push(this.f); } }
  class ResizeObserver { constructor(f) { this.f = f; } observe() {} }
  const ctx = { document, window, KeyboardEvent, MutationObserver, ResizeObserver, URLSearchParams, Intl, TextEncoder, getComputedStyle: () => ({ fontFamily: 'mono' }), setTimeout: () => 0, clearTimeout: () => {}, Date: { now: () => clock }, console };
  ctx.globalThis = ctx;
  vm.runInNewContext(script, ctx, { filename: 'try-script.js' });
  const SHIFT = new Set('ABCDEFGHIJKLMNOPQRSTUVWXYZ?!@#$%^&*()"<>:'.split(''));
  const api = {
    ctx,
    key(k, mods = {}) { clock += 100; const shiftKey = mods.shift !== undefined ? mods.shift : SHIFT.has(k); const key = shiftKey && /^[A-Z]$/.test(k) ? k.toLowerCase() : k; term.fire('keydown', { key, ctrlKey: !!mods.ctrl, shiftKey, altKey: !!mods.alt, metaKey: false, preventDefault() {}, target: term }); },
    type(t) { for (const c of t) api.key(c); },
    check(id, on) { const el = byId[id]; if (!el) throw new Error('no checkbox ' + id); el.checked = on; el.fire('change', {}); },
    radio(n, v) { const el = radios.find(r => r.attrs.name === n && r.attrs.value === v); if (!el) throw new Error(`no radio ${n}=${v}`); radios.filter(r => r.attrs.name === n).forEach(r => { r.checked = r === el; }); el.fire('change', {}); },
    scenario(sc) { const b = buttons.find(x => x.dataset.sc === sc); if (!b) throw new Error('no scenario ' + sc); b.fire('click', {}); },
    click(line) { const row = term.rows[line]; term.fire('click', { target: { closest: s => (s === '.row' ? row : null) } }); },
    rightClick(line) { const row = term.rows[line]; term.fire('contextmenu', { preventDefault() {}, target: { closest: s => (s === '.row' ? row : null) } }); },
    resize() { for (const f of (winListeners.resize || [])) f(); for (const f of (winListeners.presize || [])) f(); },
    text() { return (term.rows || []).map(r => r.innerHTML.replace(/<[^>]+>/g, '').replace(/&lt;/g, '<').replace(/&amp;/g, '&')); },
    rows() { return (term.rows || []).map(r => r.innerHTML); },
    raw() { return term._raw || ''; },
    frame() { return term.innerHTML; },
    winListeners,
    measures: () => measures,
    fontSize: () => term.style.fontSize,
  };
  return api;
}

const ROWS = 34;
const BANNED = /undefined|NaN|\[object/;
const problems = new Map();
let drawings = 0;
const glyphs = new Set();
const note = (where, what) => { const k = what; if (!problems.has(k)) problems.set(k, { n: 0, where }); problems.get(k).n++; };

function check(p, cols, where) {
  const rows = p.text();
  drawings++;
  if (rows.length !== ROWS) note(where, `${rows.length} rows, not ${ROWS}`);
  rows.forEach(r => {
    if (r.length > cols) note(where, `a row of ${r.length} columns in a terminal of ${cols}`);
    const bad = r.match(BANNED);
    if (bad) note(where, `"${bad[0]}" drawn: ${r.trim().slice(0, 60)}`);
    for (const c of r) if (c.charCodeAt(0) > 127) glyphs.add(c);
  });
}
function step(p, cols, where, fn) {
  try { fn(); } catch (e) { note(where, `exception ${String(e).slice(0, 90)} at ${((e.stack || '').split('\n')[1] || '').trim().slice(0, 60)}`); return false; }
  check(p, cols, where);
  return true;
}

const html = fs.readFileSync(process.argv[2], 'utf8');
const loadPage = (where, o, cols) => {
  try { const p = load(html, o); check(p, cols, where + ' first drawing'); return p; }
  catch (e) { note(where, `exception on load ${String(e).slice(0, 90)} at ${((e.stack || '').split('\n')[1] || '').trim().slice(0, 60)}`); return null; }
};

// 1. The first view in each way the page opens: embedded, standalone, on a phone.
loadPage('standalone', {}, 160);
loadPage('phone', { phone: true }, 60);
loadPage('embedded without a frame', { embed: true }, 160);

// 2. Each width with the preview on and off, then the views the owner meets.
const views = {
  'the first view': () => {},
  'the flat list': p => p.radio('layout', 'flat'),
  'a filter': p => { p.type('api'); },
  'titles off': p => p.check('opt-titles', false),
  'the away machine live': p => p.radio('rmpi', 'live'),
  'every state of the away machine': p => { for (const v of ['connecting', 'notaw', 'oldtaw', 'live', 'dropped']) p.radio('rmpi', v); },
  'the inside view': p => { p.key('j'); p.key('j'); p.key('Enter'); },
  'help': p => p.key('?'),
  'the sessions depth': p => p.radio('depth', 'sessions'),
  'the all depth': p => p.radio('depth', 'all'),
  'the search model': p => p.radio('model', 'search'),
};
for (const cols of [160, 100, 99, 60]) {
  for (const preview of [true, false]) {
    for (const [name, view] of Object.entries(views)) {
      const where = `${cols} columns, preview ${preview ? 'on' : 'off'}, ${name}`;
      const p = loadPage(where, { embed: true, frame: true }, 160);
      if (!p) continue;
      step(p, cols, where, () => { p.radio('width', String(cols)); p.check('opt-preview', preview); });
      if (!step(p, cols, where, () => view(p))) continue;
      if (name !== 'the first view' && name !== 'the flat list' && name !== 'the away machine live') continue;
      step(p, cols, where, () => { for (let i = 0; i < 40; i++) p.key('j'); });
      for (const sc of ['ask', 'finish', 'look', 'reset', 'rask']) step(p, cols, where + ', scenario ' + sc, () => p.scenario(sc));
    }
  }
}

// 3. Every agent kind in every state, at each width a pane can have: the screens themselves.
{
  let peeked = null;
  try { peeked = load(html, { embed: true, frame: true, peek: true }).ctx.__peek(); } catch (e) { note('the screens', `exception on load ${String(e).slice(0, 90)}`); }
  const { agentScreen, SCREENS } = peeked || { agentScreen: null, SCREENS: {} };
  for (const kind of Object.keys(SCREENS).concat(['unknown-kind'])) {
    for (const state of ['working', 'blocked', 'done', 'idle', 'unknown']) {
      for (const w of [158, 98, 97, 58, 30, 3, 0, -3]) {
        const where = `${kind} ${state} screen at ${w}`;
        let lines;
        try { lines = agentScreen(kind, state, w); } catch (e) { note(where, `exception ${String(e).slice(0, 90)}`); continue; }
        lines.forEach(l => {
          if (l.length > Math.max(w, 30)) note(where, `a screen line of ${l.length} columns at ${w}`);
          if (BANNED.test(l)) note(where, `"${l.match(BANNED)[0]}" drawn`);
          for (const c of l) if (c.charCodeAt(0) > 127) glyphs.add(c);
        });
      }
    }
  }
}

// 4. A seeded fuzz over keys, filters, every radio and checkbox, the scenarios and clicks.
{
  const rng = seed => { let a = seed >>> 0; return () => { a = (a + 0x6D2B79F5) >>> 0; let t = a; t = Math.imul(t ^ (t >>> 15), t | 1); t ^= t + Math.imul(t ^ (t >>> 7), t | 61); return ((t ^ (t >>> 14)) >>> 0) / 4294967296; }; };
  const KEYS = ['j', 'k', 'J', 'K', 'h', 'l', 'f', 'a', 'b', 'c', 'd', 'e', 'g', 'n', 'w', 'r', 'x', 'X', 'o', 'y', 'q', 's', 'z', '?', '/', ' ', '1', '2', '3', 'Tab', 'Enter', 'Escape', 'Backspace', 'Home', 'End', 'ArrowUp', 'ArrowDown', 'ArrowLeft', 'ArrowRight', 'O', 'C', 'Q', 'p', 'v', 'm', 'i', 't', 'u'];
  const CHORDS = [['s', 'ctrl'], ['b', 'ctrl'], ['n', 'ctrl'], ['p', 'ctrl'], ['j', 'ctrl'], ['k', 'ctrl'], ['w', 'ctrl'], ['a', 'ctrl'], ['e', 'ctrl'], ['u', 'ctrl'], ['Tab', 'shift']];
  const WORDS = ['api', '~/o', 'tax', 'docker', 'clau', 'codex', 'gem', 'open', 'studio', 'pi', 'not ans', 'working', 'idle'];
  const RADIOS = { model: ['modal', 'search'], depth: ['agents', 'all', 'sessions'], cursor: ['you', 'attention', 'previous'], naming: ['directory', 'full-path', 'short-path'], layout: ['tree', 'flat'], sort: ['attention', 'sessions'], bar: ['taw', 'own'], width: ['160', '100', '99', '60'], rmpi: ['dropped', 'live', 'connecting', 'notaw', 'oldtaw'], rmwhere: ['app', 'popup'] };
  const CHECKS = ['opt-pif', 'opt-openmark', 'opt-wrap', 'opt-git', 'opt-symbols', 'opt-statustop', 'opt-zoxide', 'opt-dirs', 'opt-labels', 'opt-titles', 'opt-remote', 'opt-preview'];
  const SC = ['ask', 'finish', 'look', 'reset', 'rask'];
  for (let run = 0; run < 6; run++) {
    const r = rng(7 + run), pick = xs => xs[Math.floor(r() * xs.length)];
    const p = loadPage(`fuzz run ${run}`, { embed: true, frame: true }, 160);
    if (!p) continue;
    let cols = 160;
    for (let i = 0; i < 60; i++) {
      const x = r(); let name, fn;
      if (x < 0.5) { const k = pick(KEYS); name = 'key ' + k; fn = () => p.key(k); }
      else if (x < 0.6) { const [k, m] = pick(CHORDS); name = m + '-' + k; fn = () => p.key(k, { [m]: true }); }
      else if (x < 0.68) { const w = pick(WORDS); name = 'type ' + w; fn = () => p.type(w); }
      else if (x < 0.8) { const n = pick(Object.keys(RADIOS)), v = pick(RADIOS[n]); name = `radio ${n}=${v}`; fn = () => { p.radio(n, v); if (n === 'width') cols = +v; }; }
      else if (x < 0.9) { const id = pick(CHECKS), on = r() < 0.5; name = `check ${id}=${on}`; fn = () => p.check(id, on); }
      else if (x < 0.95) { const s = pick(SC); name = 'scenario ' + s; fn = () => p.scenario(s); }
      else { const l = Math.floor(r() * ROWS); name = 'click ' + l; fn = () => p.click(l); }
      if (!step(p, cols, `fuzz run ${run} step ${i} (${name})`, fn)) break;
    }
  }
}

// 5. The inside view draws a pane as wide as the terminal: its rules reach the right edge, as a
// full-width pane's would.
for (const cols of [160, 100, 60]) {
  const where = `the inside view at ${cols} columns`;
  const p = loadPage(where, { embed: true, frame: true }, 160);
  if (!p) continue;
  if (!step(p, cols, where, () => { p.radio('width', String(cols)); p.key('Enter'); })) continue;
  const rules = p.text().map(r => r.trimEnd()).filter(r => /^\u2500+$/.test(r));
  if (!rules.length) note(where, 'no rule is drawn in the inside view');
  else if (Math.max(...rules.map(r => r.length)) !== cols) note(where, `the inside view's rules are ${Math.max(...rules.map(r => r.length))} columns, not ${cols}`);
}

// 6. The glyphs. Every non-ASCII glyph a screen draws is in the page's one-cell class, `esc`'s
// `.g` (or its two-cell class `.g2`), or is a no-break space or box drawing, which every monospace font draws in a cell.
{
  const re = /replace\(\/\[([^\]]+)\]\/g, c => `<span class="g">/.exec(html);
  if (!re) note('glyph class', 'the one-cell glyph class of esc was not found in the page');
  else {
    const cls = new Set(JSON.parse('"' + re[1].replace(/"/g, '\\"').replace(/\\u([0-9a-fA-F]{4})/g, '\\u$1') + '"'));
    const two = /replace\(\/(\S+)\/gu, c => `<span class="g2">/.exec(html);
    const twoSet = new Set(two ? [...two[1]] : []);
    for (const g of glyphs) {
      const cp = g.codePointAt(0);
      if (cls.has(g) || twoSet.has(g) || g === '\u00a0' || (cp >= 0x2500 && cp <= 0x257f)) continue;
      note('glyphs', `the glyph ${g} (U+${cp.toString(16).toUpperCase().padStart(4, '0')}) is drawn but not in esc's one-cell class`);
    }
  }
}

// 7. The top line names a machine that is not answering, as taw's does: before the clock when no header
// names it (the flat list, a filter that leaves it without a row), and not when its header says it (the tree).
{
  const WORDS = 'pi not connected';
  const top = p => p.text()[0];
  for (const kind of ['app', 'popup']) {
    const here = `the top line in the ${kind}`;
    const p = loadPage(here, { embed: true, frame: true }, 160);
    if (!p) continue;
    if (kind === 'popup') step(p, 160, here, () => { p.radio('rmwhere', 'popup'); p.radio('bar', 'taw'); });
    if (top(p).includes(WORDS)) note(here, `the tree, where pi's header says it, repeats ${WORDS}`);
    if (!p.text().some(r => r.includes('not answering, last seen 2m ago'))) note(here, "pi's header does not say not answering in the tree");
    step(p, 160, here, () => p.radio('layout', 'flat'));
    if (!/pi not connected {2}\u2502 {2}\d\d:\d\d $/.test(top(p))) note(here, `the flat list's top line does not end with the away words and the clock: ${top(p)}`);
    step(p, 160, here, () => p.radio('layout', 'tree'));
    if (top(p).includes(WORDS)) note(here, 'the tree names pi on the top line again after the flat list');
    step(p, 160, here, () => p.radio('layout', 'flat'));
    step(p, 160, here, () => p.radio('rmpi', 'live'));
    if (top(p).includes(WORDS)) note(here, 'a connected pi is named on the top line');
    step(p, 160, here, () => p.radio('rmpi', 'dropped'));
    if (!top(p).includes(WORDS)) note(here, 'pi is not named in the flat list once it drops again');
    // A filter in the tree: pi has a header only while a row of it matches.
    step(p, 160, here, () => p.radio('layout', 'tree'));
    step(p, 160, here, () => { p.key('/'); p.type('claude'); });
    const filtered = top(p);
    const header = p.text().some(r => r.includes('not answering, last seen 2m ago'));
    if (header === filtered.includes(WORDS)) note(here, `under a filter pi's header ${header ? 'is' : 'is not'} drawn and the top line ${filtered.includes(WORDS) ? 'names' : 'does not name'} it: ${filtered.trim()}`);
    step(p, 160, here, () => { p.key('Escape'); p.type('/zzzzq'); });
    if (!top(p).includes(WORDS)) note(here, `a filter that matches nothing leaves pi without a header and without a name: ${top(p).trim()}`);
  }
  // A narrow line keeps the away words; the clock goes first, then the machine count.
  const p = loadPage('the narrow top line', { embed: true, frame: true }, 160);
  if (p) {
    step(p, 160, 'the narrow top line', () => p.radio('layout', 'flat'));
    for (const cols of [160, 100, 60]) {
      step(p, cols, `the narrow top line at ${cols}`, () => p.radio('width', String(cols)));
      const t = top(p);
      if (!t.includes(WORDS)) note(`the top line at ${cols}`, `the away words are gone: ${t.trim()}`);
      if (t.length > cols) note(`the top line at ${cols}`, `the line is ${t.length} columns`);
    }
  }
}

// 8. A key does not size the window again: the size is worked out on load and on resize, so a key press
// reads no layout (no getBoundingClientRect) and leaves the font size as it was.
{
  const where = 'keys after the first drawing';
  const p = loadPage(where, { embed: true, frame: true, rectW: 1400, rectH: 600 }, 160);
  if (p) {
    const size = p.fontSize(), before = p.measures();
    step(p, 160, where, () => { for (const k of ['j', 'j', 'k', 'Tab', 'j', 'J', 'K', '/', 'Escape']) p.key(k); });
    if (p.measures() !== before) note(where, `${p.measures() - before} layout reads while keys were pressed`);
    if (p.fontSize() !== size) note(where, `the font size went from ${size} to ${p.fontSize()} on keys`);
    const reads = p.measures();
    p.resize();
    if (p.measures() === reads) note(where, 'a resize does not size the window again');
  }
}

if (problems.size) {
  for (const [what, v] of problems) console.log(`FAIL x${v.n} ${what} (first: ${v.where})`);
  process.exit(1);
}
console.log(`try page smoke ok: ${drawings} drawings, ${glyphs.size} glyphs`);
