// Verbatim 4 : interface. Tout passe par les commandes Rust ; la page n'a
// accès ni au réseau, ni au disque, ni à la clé API.
import { h, render } from './vendor/preact.js';
import { useState, useEffect, useRef, useMemo, useReducer, useCallback } from './vendor/hooks.js';
import htm from './vendor/htm.js';

const html = htm.bind(h);
const T = window.__TAURI__;
const invoke = (cmd, args) => T.core.invoke(cmd, args);
const listen = (ev, fn) => T.event.listen(ev, e => fn(e.payload));

/* ------------------------------------------------------------------ */
/* Constantes et utilitaires                                           */
/* ------------------------------------------------------------------ */
// Vocabulaire spécifique : +20 % sur toute la transcription (tarif ElevenLabs).
const USD_PER_HOUR = 0.22, KEYTERM_FACTOR = 1.2, USD_TO_EUR = 0.92;
const PALETTE = ['#2743E3', '#0E8A6D', '#B5651D', '#8A2E7A', '#2B8FA8', '#6A5ACD', '#A0522D', '#3F7D20'];
const MODE_LABEL = { separate: 'pistes séparées', mixed: 'piste mixée', single: 'piste unique' };

const pad = n => String(n).padStart(2, '0');
const clock = s => { s = Math.max(0, Math.floor(s || 0)); const h = Math.floor(s / 3600), m = Math.floor(s % 3600 / 60), x = s % 60; return h ? `${h}:${pad(m)}:${pad(x)}` : `${pad(m)}:${pad(x)}`; };
const dur = s => { if (!s) return ''; const m = Math.round(s / 60); return m < 60 ? `${Math.max(1, m)} min` : `${Math.floor(m / 60)} h ${pad(m % 60)}`; };
const eur = v => v.toFixed(2).replace('.', ',') + ' €';
const num = v => Math.round(v).toLocaleString('fr-FR');
const errMsg = e => (e && e.message) || String(e);
const parseDate = s => new Date((s || '').replace(' ', 'T'));
function dayLabel(d) {
  const today = new Date(); today.setHours(0, 0, 0, 0);
  const that = new Date(d); that.setHours(0, 0, 0, 0);
  const diff = Math.round((today - that) / 864e5);
  if (diff === 0) return 'Aujourd’hui';
  if (diff === 1) return 'Hier';
  return d.toLocaleDateString('fr-FR', { weekday: 'long', day: 'numeric', month: 'long', year: today.getFullYear() === d.getFullYear() ? undefined : 'numeric' });
}
const hhmm = d => d.toLocaleTimeString('fr-FR', { hour: '2-digit', minute: '2-digit' });
function parseKeyterms(s) {
  const seen = new Set(), out = []; let rejected = 0;
  for (let t of String(s || '').split(/[\n,;]+/)) {
    t = t.trim().replace(/\s+/g, ' ');
    if (!t) continue;
    if (t.length >= 50 || t.split(' ').length > 5 || /[<>{}\[\]\\]/.test(t)) { rejected++; continue; }
    if (seen.has(t.toLowerCase())) continue;
    seen.add(t.toLowerCase());
    if (out.length >= 100) { rejected++; continue; }
    out.push(t);
  }
  return { terms: out, rejected };
}
const b64 = s => { const bin = atob(s || ''); const a = new Uint8Array(bin.length); for (let i = 0; i < bin.length; i++) a[i] = bin.charCodeAt(i); return a; };

/* ------------------------------------------------------------------ */
/* Magasin d'état minimal                                              */
/* ------------------------------------------------------------------ */
let S = {
  ready: false, version: '', key: null, settings: null, library: '', windows: true, simulated: false,
  items: {}, selected: null, query: '', hits: null, recording: null, live: null,
  progress: {}, balance: null, balanceError: null, devices: null, drawer: false, toast: null, dragging: false,
  updates: false, update: null, updating: null, updateFile: null,
  cal: { connected: false, now: [], next: null, error: null }, calState: null,
};
const subs = new Set();
function set(p) { S = { ...S, ...(typeof p === 'function' ? p(S) : p) }; subs.forEach(f => f()); }
function useStore(sel) {
  const [, force] = useReducer(x => x + 1, 0);
  const selRef = useRef(sel); selRef.current = sel;
  const val = useRef(); val.current = sel(S);
  useEffect(() => {
    const f = () => { const v = selRef.current(S); if (!Object.is(v, val.current)) { val.current = v; force(); } };
    subs.add(f); return () => subs.delete(f);
  }, []);
  return val.current;
}
let toastTimer;
function toast(msg) { set({ toast: msg }); clearTimeout(toastTimer); toastTimer = setTimeout(() => set({ toast: null }), 3600); }
function upsert(sum) { set(s => ({ items: { ...s.items, [sum.id]: sum } })); }

const rates = settings => {
  const kt = parseKeyterms(settings.keyterms).terms.length > 0;
  const f = kt ? KEYTERM_FACTOR : 1;
  return { eurPerMin: USD_PER_HOUR * f * USD_TO_EUR / 60, creditsPerMin: settings.credits_per_min * f };
};

/* ------------------------------------------------------------------ */
/* Actions                                                             */
/* ------------------------------------------------------------------ */
async function refreshState() {
  const st = await invoke('app_state');
  applyTheme(st.settings.theme);
  set({ ready: true, version: st.version, key: st.key, settings: st.settings, library: st.library, windows: st.windows, simulated: st.simulated_audio, updates: st.updates, repo: st.repo, recording: st.recording ? { id: st.recording } : null });
  return st;
}
async function loadLibrary() {
  const list = await invoke('library_list');
  const items = {}; list.forEach(x => items[x.id] = x);
  set({ items });
}
async function fetchBalance() {
  if (!S.key || !S.key.has_key) { set({ balance: null, balanceError: null }); return; }
  try {
    const d = await invoke('balance');
    const limit = Number(d.limit) || 0, used = Number(d.count) || 0;
    set({ balance: { left: Math.max(0, limit - used), used, limit, tier: d.tier || '—', reset: d.reset ? new Date(d.reset * 1000) : null }, balanceError: null });
  } catch (e) {
    set({ balance: null, balanceError: e && e.status === 401 ? 'Clé refusée, ou sans la permission de lire le compte (User → Read). La transcription peut fonctionner quand même.' : errMsg(e) });
  }
}
async function saveSettings(patch) {
  try { const s = await invoke('settings_set', { patch }); applyTheme(s.theme); set({ settings: s }); return s; }
  catch (e) { toast('Réglage non enregistré : ' + errMsg(e)); }
}
async function startRecording(title, event) {
  const st = S.settings;
  if (!st.rec_mic && !st.rec_pc) { toast('Choisis au moins une source à enregistrer.'); return; }
  try {
    const id = await invoke('rec_start', { mic: st.rec_mic, pc: st.rec_pc, title: title || null, event: event === undefined ? null : event });
    set({ recording: { id }, selected: id, live: null });
  } catch (e) { toast('Enregistrement impossible : ' + errMsg(e)); }
}
async function stopRecording() {
  try { await invoke('rec_stop'); } catch (e) { toast(errMsg(e)); }
  set({ recording: null, live: null });
}
async function importPaths(paths) {
  if (!paths || !paths.length) return;
  // Une nouvelle version de Verbatim glissée dans la fenêtre : on propose de l'installer.
  if (paths.length === 1 && /\.(exe|zip)$/i.test(paths[0])) { set({ updateFile: paths[0] }); return; }
  try {
    const r = await invoke('import_paths', { paths });
    if (r.rejected.length) toast('Non importé (format non reconnu) : ' + r.rejected.join(', '));
    if (r.ids.length) set({ selected: r.ids[r.ids.length - 1] });
  } catch (e) { toast(errMsg(e)); }
}
function applyTheme(t) {
  if (t === 'light' || t === 'dark') document.documentElement.dataset.theme = t;
  else delete document.documentElement.dataset.theme;
}
async function refreshCalendar(force) {
  try { set({ cal: await invoke('calendar_now', { force: !!force }) }); } catch (_) {}
}
const fmtRange = e => `${hhmm(new Date(e.start * 1000))}–${hhmm(new Date(e.end * 1000))}`;
const whenLabel = e => { const m = Math.round((e.start * 1000 - Date.now()) / 60000); return m > 0 ? `dans ${m} min` : 'en cours'; };
const joinAndRecord = async e => { if (e.meet) openUrl(e.meet); await startRecording(null, e.uid); };
const pickAndImport = () => invoke('pick_files').then(importPaths).catch(e => toast(errMsg(e)));
const openUrl = url => invoke('open_url', { url }).catch(e => toast(errMsg(e)));

/* ------------------------------------------------------------------ */
/* Icônes                                                              */
/* ------------------------------------------------------------------ */
const I = {
  mic: html`<svg class="icon" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><rect x="5.5" y="1.5" width="5" height="8" rx="2.5"/><path d="M3 7.5a5 5 0 0 0 10 0M8 12.5V15"/></svg>`,
  pc: html`<svg class="icon" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><path d="M2 6h2.5L8 3v10L4.5 10H2z"/><path d="M10.5 5.5a3.5 3.5 0 0 1 0 5M12.5 3.5a6.5 6.5 0 0 1 0 9"/></svg>`,
  file: html`<svg class="icon" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><path d="M4 1.5h5l3.5 3.5v9.5H4z"/><path d="M9 1.5V5h3.5"/></svg>`,
  search: html`<svg class="icon" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.6"><circle cx="7" cy="7" r="4.5"/><path d="M10.5 10.5 14 14"/></svg>`,
  gear: html`<svg class="icon" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round"><path d="M14.91 6.85 L14.91 9.15 L13.14 9.28 L12.54 10.73 L13.70 12.07 L12.07 13.70 L10.73 12.54 L9.28 13.14 L9.15 14.91 L6.85 14.91 L6.72 13.14 L5.27 12.54 L3.93 13.70 L2.30 12.07 L3.46 10.73 L2.86 9.28 L1.09 9.15 L1.09 6.85 L2.86 6.72 L3.46 5.27 L2.30 3.93 L3.93 2.30 L5.27 3.46 L6.72 2.86 L6.85 1.09 L9.15 1.09 L9.28 2.86 L10.73 3.46 L12.07 2.30 L13.70 3.93 L12.54 5.27 L13.14 6.72Z"/><circle cx="8" cy="8" r="2.3"/></svg>`,
  info: html`<svg viewBox="0 0 16 16" width="13" height="13" fill="none" stroke="currentColor" stroke-width="1.4"><circle cx="8" cy="8" r="6.3"/><path d="M8 7.2v4M8 4.9v.1" stroke-linecap="round"/></svg>`,
  chev: html`<svg class="icon" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.6"><path d="m4 6 4 4 4-4"/></svg>`,
  play: html`<svg viewBox="0 0 16 16" width="14" height="14" fill="currentColor"><path d="M4 2.5v11l9-5.5z"/></svg>`,
  pause: html`<svg viewBox="0 0 16 16" width="14" height="14" fill="currentColor"><rect x="3.5" y="2.5" width="3" height="11"/><rect x="9.5" y="2.5" width="3" height="11"/></svg>`,
  imp: html`<svg class="icon" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><path d="M8 2v8M4.5 6.5 8 10l3.5-3.5M2.5 11v2.5h11V11"/></svg>`,
  folder: html`<svg class="icon" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><path d="M1.5 3.5h4.5l1.5 1.5h7v8.5h-13z"/></svg>`,
  cal: html`<svg class="icon" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4"><rect x="2" y="3" width="12" height="11" rx="1.5"/><path d="M2 6.5h12M5.5 1.5v3M10.5 1.5v3"/></svg>`,
  swap: html`<svg viewBox="0 0 16 16" width="13" height="13" fill="none" stroke="currentColor" stroke-width="1.5"><path d="M3 5h9l-2.5-2.5M13 11H4l2.5 2.5"/></svg>`,
};
const srcIcon = src => src === 'mic' ? I.mic : src === 'pc' ? I.pc : null;

/* ------------------------------------------------------------------ */
/* Petits composants                                                   */
/* ------------------------------------------------------------------ */
function Popover({ open, onClose, children, style }) {
  if (!open) return null;
  return html`<div class="backdrop" onClick=${onClose}></div><div class="pop" style=${style} onClick=${e => e.stopPropagation()}>${children}</div>`;
}
function Note({ kind, children }) { return html`<div class=${'note ' + (kind || '')}>${children}</div>`; }
// Explication au survol (et au focus clavier), pour garder les panneaux légers.
function Tip({ text }) { return html`<span class="tip" tabindex="0" data-tip=${text} aria-label=${text}>${I.info}</span>`; }

// Titre modifiable : la saisie n'est jamais écrasée par les mises à jour qui
// arrivent pendant qu'on tape (finalisation, transcription…).
function TitleField({ id, title, autoFocus }) {
  const [v, setV] = useState(title);
  const focused = useRef(false);
  const ref = useRef();
  useEffect(() => { if (!focused.current) setV(title); }, [title]);
  useEffect(() => { if (autoFocus && ref.current && document.activeElement !== ref.current) { ref.current.focus(); ref.current.select(); } }, [id, autoFocus]);
  const save = () => {
    focused.current = false;
    const t = v.trim();
    if (!t) { setV(title); return; }
    if (t !== title) invoke('item_rename', { id, title: t }).catch(err => { toast(errMsg(err)); setV(title); });
  };
  return html`<input ref=${ref} class="title" value=${v} placeholder="Nom de la conversation" aria-label="Titre"
    onFocus=${() => { focused.current = true; }} onInput=${e => setV(e.target.value)} onBlur=${save}
    onKeyDown=${e => { if (e.key === 'Enter') e.target.blur(); if (e.key === 'Escape') { setV(title); focused.current = false; e.target.blur(); } }} />`;
}

function statusChip(it) {
  if (it.state === 'recording') return html`<span class="chip rec">● en cours</span>`;
  if (it.state === 'finalizing') return html`<span class="chip run">finalisation</span>`;
  if (it.state === 'importing') return html`<span class="chip run">import</span>`;
  if (it.state === 'failed') return html`<span class="chip err">erreur</span>`;
  const j = it.job;
  if (j && j.state === 'error') return html`<span class="chip err">à relancer</span>`;
  if (j) return html`<span class="chip run">${j.state === 'queued' ? 'en attente' : j.progress != null && j.state !== 'processing' ? `${Math.round(j.progress * 100)} %` : 'transcription'}</span>`;
  if (!it.has_transcript) return html`<span class="chip todo">non transcrit</span>`;
  return null;
}

/* ------------------------------------------------------------------ */
/* Barre du haut                                                       */
/* ------------------------------------------------------------------ */
function RecordButton() {
  const settings = useStore(s => s.settings);
  const recording = useStore(s => s.recording);
  const [open, setOpen] = useState(false);
  if (recording) return html`<button class="btn rec" onClick=${stopRecording}><span class="dot" style="border-radius:2px"></span> Arrêter</button>`;
  const toggle = k => saveSettings({ [k]: !settings[k] });
  const srcs = [settings.rec_mic && 'micro', settings.rec_pc && 'son du PC'].filter(Boolean).join(' + ') || 'aucune source';
  return html`<div class="split" style="position:relative">
    <button class="btn rec" onClick=${() => startRecording()} title=${'Enregistrer : ' + srcs}><span class="dot"></span> Enregistrer</button>
    <button class="btn rec" aria-label="Sources" onClick=${() => setOpen(true)}>${I.chev}</button>
    <${Popover} open=${open} onClose=${() => setOpen(false)} style="top:40px; left:0; width:300px">
      <div class="head">Sources</div>
      <label class="item"><input type="checkbox" checked=${settings.rec_mic} onChange=${() => toggle('rec_mic')} /> ${I.mic} <span class="grow">Micro</span></label>
      <label class="item"><input type="checkbox" checked=${settings.rec_pc} onChange=${() => toggle('rec_pc')} /> ${I.pc} <span class="grow">Son du PC</span></label>
      <div class="sep"></div>
      <label class="item"><input type="checkbox" checked=${settings.auto_transcribe} onChange=${() => toggle('auto_transcribe')} /> <span class="grow">Transcrire automatiquement à l’arrêt</span></label>
      <div class="sep"></div>
      <button class="item" onClick=${() => { setOpen(false); set({ drawer: true }); }}>${I.gear} Choisir les périphériques…</button>
    <//>
  </div>`;
}

function Top() {
  const [q, setQ] = useState('');
  useEffect(() => {
    const t = setTimeout(async () => {
      if (!q.trim()) { set({ query: '', hits: null }); return; }
      try { const ids = await invoke('library_search', { query: q }); set({ query: q, hits: new Set(ids) }); } catch (_) {}
    }, 220);
    return () => clearTimeout(t);
  }, [q]);
  return html`<div class="top">
    <button class="brand" title="Accueil" onClick=${() => set({ selected: null })}>Verbatim<b>.</b></button>
    <${RecordButton} />
    <button class="btn" onClick=${pickAndImport}>${I.imp} Importer</button>
    <${NowPill} />
    <span class="grow"></span>
    <label class="search">${I.search}<input type="search" placeholder="Rechercher dans les conversations" value=${q} onInput=${e => setQ(e.target.value)} /></label>
    <button class="btn ghost" aria-label="Réglages" title="Réglages" onClick=${() => set({ drawer: true })}>${I.gear}</button>
  </div>`;
}

/* ------------------------------------------------------------------ */
/* Mises à jour                                                        */
/* ------------------------------------------------------------------ */
async function installUpdate(file) {
  set({ updating: 0 });
  try {
    if (file !== undefined) {
      const done = await invoke('update_install_file', { path: file });
      if (!done) set({ updating: null });
    } else await invoke('update_install');
    // En cas de succès, Verbatim se ferme et la nouvelle version s'ouvre.
  } catch (e) { set({ updating: null }); toast('Mise à jour impossible : ' + errMsg(e)); }
}
async function checkUpdate(manual) {
  try {
    const u = await invoke('update_check');
    set({ update: u });
    if (manual && !u) toast(`Verbatim ${S.version} est à jour.`);
  } catch (e) { if (manual) toast('Vérification impossible : ' + errMsg(e)); }
}

function UpdateBar() {
  const u = useStore(s => s.update);
  const file = useStore(s => s.updateFile);
  const updating = useStore(s => s.updating);
  const recording = useStore(s => s.recording);
  const [later, setLater] = useState(false);
  if (updating != null) return html`<div class="updbar"><b>Installation de la nouvelle version…</b>
    <div class="progress" style="width:160px;margin:0"><i style=${`width:${Math.round(updating * 100)}%`}></i></div>
    <small class="muted">Verbatim va redémarrer. Tes conversations, ta clé et tes réglages sont conservés.</small></div>`;
  if (file) return html`<div class="updbar"><span>Installer <b>${file.split(/[\\/]/).pop()}</b> comme nouvelle version de Verbatim ?</span>
    <span class="grow"></span>
    <button class="btn small primary" disabled=${!!recording} onClick=${() => { set({ updateFile: null }); installUpdate(file); }}>Installer et redémarrer</button>
    <button class="btn small ghost" onClick=${() => set({ updateFile: null })}>Annuler</button></div>`;
  if (!u || later) return null;
  return html`<div class="updbar"><span><b>Verbatim ${u.version}</b> est disponible.</span>
    ${u.notes ? html`<${Tip} text=${u.notes.slice(0, 600)} />` : null}
    <span class="grow"></span>
    ${recording ? html`<small class="muted">Après l’enregistrement.</small>` : null}
    <button class="btn small primary" disabled=${!!recording} onClick=${() => installUpdate()}>Mettre à jour</button>
    <button class="btn small ghost" onClick=${() => setLater(true)}>Plus tard</button></div>`;
}

/* ------------------------------------------------------------------ */
/* Bande d'enregistrement                                              */
/* ------------------------------------------------------------------ */
// Dernier instant (secondes d'enregistrement) où chaque piste a capté autre chose
// qu'un silence numérique. Un micro coupé (touche mute, casque) donne ~-100 dBFS.
const lastSound = {};
const MUTE_DB = -85, MUTE_AFTER = 6;

function Meter({ t, elapsed, recId }) {
  const pct = Math.max(0, Math.min(100, (t.peak_db + 60) / 60 * 100));
  const color = t.role === 'mic' ? 'var(--mic)' : 'var(--pc)';
  const key = recId + ':' + t.role;
  if (lastSound[key] === undefined || t.peak_db > MUTE_DB || t.status.state !== 'active') lastSound[key] = elapsed;
  const muted = t.role === 'mic' && t.status.state === 'active' && elapsed - lastSound[key] > MUTE_AFTER;
  const warn = t.status.state !== 'active' || t.error || muted;
  const dev = t.error
    || (muted ? 'Aucun son capté : le micro est-il coupé ?'
    : t.status.state === 'active' ? (t.status.device || '') : (t.status.message || 'en attente…'));
  return html`<div class=${'meter' + (warn ? ' warn' : '')}>
    <span class="lbl row" style=${'color:' + color}>${t.role === 'mic' ? I.mic : I.pc} ${t.role === 'mic' ? 'Micro' : 'Son du PC'}</span>
    <div class="bar"><i style=${`width:${pct}%; background:${pct > 92 ? 'var(--rec)' : color}`}></i></div>
    <span class="dev" title=${dev}>${dev}</span>
  </div>`;
}

function RecBar() {
  const recording = useStore(s => s.recording);
  const live = useStore(s => s.live);
  const item = useStore(s => s.recording && s.items[s.recording.id]);
  const [title, setTitle] = useState('');
  useEffect(() => setTitle(''), [recording && recording.id]);
  if (!recording) return null;
  const paused = live && live.paused;
  const saveTitle = () => { if (title.trim() && item) invoke('item_rename', { id: item.id, title }).catch(() => {}); };
  return html`<div class=${'recbar' + (paused ? ' paused' : '')}>
    <span class="recdot"></span>
    <span class="rectime">${clock(live ? live.elapsed : 0)}</span>
    ${item && item.event ? html`<span class="linked" title="Rendez-vous lié : titre et participants servent à nommer les voix">${I.cal}${item.event}<button aria-label="Délier" title="Ne pas lier à ce rendez-vous" onClick=${() => invoke('item_link_event', { id: item.id, event: null }).catch(e => toast(errMsg(e)))}>×</button></span>` : null}
    ${(live ? live.tracks : []).map(t => html`<${Meter} t=${t} elapsed=${live.elapsed} recId=${recording.id} />`)}
    <span class="grow"></span>
    <input class="rectitle" placeholder="Nom de la conversation (facultatif)" value=${title} onInput=${e => setTitle(e.target.value)} onBlur=${saveTitle} onKeyDown=${e => e.key === 'Enter' && e.target.blur()} />
    <button class="btn" onClick=${() => invoke('rec_pause', { paused: !paused }).catch(e => toast(errMsg(e)))}>${paused ? 'Reprendre' : 'Pause'}</button>
    <button class="btn rec" onClick=${async () => { saveTitle(); await stopRecording(); }}><span class="dot" style="border-radius:2px"></span> Arrêter</button>
  </div>`;
}

/* ------------------------------------------------------------------ */
/* Bibliothèque                                                        */
/* ------------------------------------------------------------------ */
function Side() {
  const items = useStore(s => s.items);
  const hits = useStore(s => s.hits);
  const selected = useStore(s => s.selected);
  const groups = useMemo(() => {
    const list = Object.values(items).filter(x => !hits || hits.has(x.id)).sort((a, b) => (b.created || '').localeCompare(a.created || ''));
    const out = [];
    for (const it of list) {
      const d = parseDate(it.created); const label = dayLabel(d);
      if (!out.length || out[out.length - 1].label !== label) out.push({ label, items: [] });
      out[out.length - 1].items.push({ it, d });
    }
    return out;
  }, [items, hits]);
  if (!groups.length) return html`<aside class="side"><div class="empty-list">${hits ? 'Aucune conversation ne correspond.' : 'Tes conversations apparaîtront ici.'}</div></aside>`;
  return html`<aside class="side">${groups.map(g => html`
    <div class="group">${g.label}</div>
    ${g.items.map(({ it, d }) => html`<button class=${'entry' + (it.id === selected ? ' sel' : '')} onClick=${() => set({ selected: it.id })}>
      <div class="t">${it.title}</div>
      <div class="m">
        ${it.kind === 'recording' ? I.mic : I.file}
        <span>${hhmm(d)}${it.duration ? ' · ' + dur(it.duration) : ''}</span>
        ${it.speakers.length ? html`<span class="who">· ${it.speakers.slice(0, 3).join(', ')}${it.speakers.length > 3 ? '…' : ''}</span>` : null}
        <span class="grow"></span>${statusChip(it)}
      </div>
    </button>`)}`)}</aside>`;
}

/* ------------------------------------------------------------------ */
/* Accueil                                                             */
/* ------------------------------------------------------------------ */
function KeySetup({ compact }) {
  const key = useStore(s => s.key);
  const [val, setVal] = useState('');
  const [remember, setRemember] = useState(true);
  const [editing, setEditing] = useState(false);
  const save = async () => {
    if (!val.trim()) return;
    try {
      const k = await invoke('key_set', { key: val.trim(), remember });
      setVal(''); setEditing(false); set({ key: k }); fetchBalance();
      toast(k.remember ? 'Clé enregistrée, protégée par ta session Windows.' : 'Clé gardée jusqu’à la fermeture.');
    } catch (e) { toast('Clé refusée : ' + errMsg(e)); }
  };
  if (key && key.has_key && !editing) {
    return html`<div class="stack">
      <div class="row"><code class="mono">${key.masked}</code><span class="grow"></span>
        <button class="linkbtn" onClick=${() => setEditing(true)}>changer</button>
        <button class="linkbtn quiet" onClick=${async () => { set({ key: await invoke('key_clear'), balance: null }); }}>effacer</button></div>
      <label class="check"><input type="checkbox" checked=${key.remember} onChange=${async e => set({ key: await invoke('key_set', { key: null, remember: e.target.checked }) })} />
        <span><b>Retenir la clé</b><small>Chiffrée par ta session Windows.</small></span></label>
    </div>`;
  }
  return html`<div class="stack">
    ${!compact ? html`<p class="muted">Verbatim transcrit avec ElevenLabs. Colle ta clé API : elle reste sur ce PC, chiffrée par ta session Windows.</p>` : null}
    <div class="row"><input class="inp mono" type="password" placeholder="sk_…" value=${val} onInput=${e => setVal(e.target.value)} onKeyDown=${e => e.key === 'Enter' && save()} />
      <button class="btn primary" onClick=${save}>Enregistrer</button></div>
    <label class="check"><input type="checkbox" checked=${remember} onChange=${e => setRemember(e.target.checked)} /><span><b>Retenir la clé</b></span></label>
    <span class="muted" style="font-size:12.5px">À créer sur <button class="linkbtn" onClick=${() => openUrl('https://elevenlabs.io/app/developers/api-keys')}>elevenlabs.io → Developers → API Keys</button>, avec la permission Speech to Text (et User → Read pour voir le solde).</span>
  </div>`;
}

/* ------------------------------------------------------------------ */
/* Agenda                                                              */
/* ------------------------------------------------------------------ */
const others = e => e.people.filter(p => !p.me);

function EventCard({ e }) {
  const ppl = others(e);
  return html`<div class="event">
    <div class="ev-head"><span class="ev-when">${whenLabel(e)}</span><span class="muted">${fmtRange(e)}</span></div>
    <div class="ev-title">${e.title}</div>
    ${ppl.length ? html`<div class="ev-people">avec ${ppl.slice(0, 6).map(p => p.name).join(', ')}${ppl.length > 6 ? '…' : ''}</div>` : null}
    <div class="row" style="margin-top:12px; gap:8px; flex-wrap:wrap">
      ${e.meet ? html`<button class="btn rec" onClick=${() => joinAndRecord(e)}><span class="dot"></span> Rejoindre et enregistrer</button>
        <button class="btn" onClick=${() => startRecording(null, e.uid)}>Enregistrer seulement</button>
        <button class="linkbtn" onClick=${() => openUrl(e.meet)}>Ouvrir la visio</button>`
      : html`<button class="btn rec" onClick=${() => startRecording(null, e.uid)}><span class="dot"></span> Enregistrer ce rendez-vous</button>`}
    </div>
  </div>`;
}

// Petit rappel dans la barre du haut quand on n'est pas sur l'accueil.
function NowPill() {
  const cal = useStore(s => s.cal);
  const recording = useStore(s => s.recording);
  const selected = useStore(s => s.selected);
  const [open, setOpen] = useState(false);
  if (recording || !selected || !cal.now.length) return null;
  const e = cal.now[0];
  return html`<span style="position:relative">
    <button class="pill" onClick=${() => setOpen(true)} title="Rendez-vous de ton agenda">${I.cal}<span class="pill-t">${e.title}</span><span class="muted">· ${whenLabel(e)}</span></button>
    <${Popover} open=${open} onClose=${() => setOpen(false)} style="top:38px; left:0; width:340px; padding:10px">
      ${cal.now.map(x => html`<${EventCard} e=${x} />`)}
    <//>
  </span>`;
}

function Welcome() {
  const key = useStore(s => s.key);
  const settings = useStore(s => s.settings);
  const [title, setTitle] = useState('');
  const toggle = k => saveSettings({ [k]: !settings[k] });
  const cal = useStore(s => s.cal);
  return html`<div class="page home">
    <div class="home-main">
    ${cal.now.length ? html`<div class="stack" style="margin-bottom:22px">${cal.now.map(e => html`<${EventCard} e=${e} />`)}</div>` : null}
    <div class="hero">Enregistre ou importe une conversation</div>
    <p class="muted">Elle rejoint ta bibliothèque, est transcrite par ElevenLabs, et chaque voix est séparée.</p>
    ${!(key && key.has_key) ? html`<div class="card" style="margin-top:16px"><div class="label">Clé API ElevenLabs</div><${KeySetup} /></div>` : null}
    <div class="welcome">
      <div class="big">
        <h2>Enregistrer</h2>
        <p>Ta voix au micro et le son du PC (appel, visio, vidéo), sans réglage audio.</p>
        <div class="row" style="gap:16px">
          <button class="bigrec" aria-label="Enregistrer" onClick=${() => startRecording(title)}><i></i></button>
          <div class="stack" style="gap:6px">
            <label class="check"><input type="checkbox" checked=${settings.rec_mic} onChange=${() => toggle('rec_mic')} /><span class="row">${I.mic} Micro</span></label>
            <label class="check"><input type="checkbox" checked=${settings.rec_pc} onChange=${() => toggle('rec_pc')} /><span class="row">${I.pc} Son du PC</span></label>
          </div>
        </div>
        <input class="inp" placeholder="Nom de la conversation (facultatif)" value=${title} onInput=${e => setTitle(e.target.value)} />
        ${cal.next && !cal.now.length ? html`<small class="muted">Prochain rendez-vous : ${cal.next.title} à ${hhmm(new Date(cal.next.start * 1000))}</small>` : null}
        <label class="check"><input type="checkbox" checked=${settings.auto_transcribe} onChange=${() => toggle('auto_transcribe')} /><span>Transcrire automatiquement à l’arrêt</span></label>
      </div>
      <div class="big">
        <h2>Transcrire un fichier</h2>
        <p>Mémo du téléphone, enregistrement d’un autre logiciel, vidéo : M4A, MP3, WAV, MP4, MOV…</p>
        <div class="drop" onClick=${pickAndImport}>Glisse tes fichiers dans la fenêtre<br/>ou clique pour parcourir</div>
      </div>
    </div>
    </div>
    <${TodayAgenda} />
  </div>`;
}

// Agenda du jour : passé estompé, repère « maintenant », rendez-vous déjà
// enregistrés reliés à leur conversation.
function TodayAgenda() {
  const cal = useStore(s => s.cal);
  const items = useStore(s => s.items);
  const recording = useStore(s => s.recording);
  const [, tick] = useState(0);
  useEffect(() => { const t = setInterval(() => tick(x => x + 1), 30_000); return () => clearInterval(t); }, []);
  const day = new Date().toLocaleDateString('fr-FR', { weekday: 'long', day: 'numeric', month: 'long' });
  if (!cal.connected) return html`<aside class="today"><div class="today-h"><b>Aujourd’hui</b><span class="muted">${day}</span></div>
    <p class="muted" style="margin:8px 0 12px">Connecte ton agenda Google pour voir ici tes rendez-vous du jour, et lancer l’enregistrement d’un appel en un clic.</p>
    <button class="btn small" onClick=${() => set({ drawer: true })}>${I.cal} Connecter l’agenda</button></aside>`;
  const now = Date.now() / 1000;
  const byUid = {};
  Object.values(items).forEach(it => { if (it.event_uid) byUid[it.event_uid] = it; });
  const rows = [];
  let marked = false;
  const marker = html`<div class="nowline"><span>${hhmm(new Date())}</span></div>`;
  for (const e of cal.today) {
    if (!marked && e.start > now) { rows.push(marker); marked = true; }
    const rec = byUid[e.uid];
    const past = e.end <= now, live = e.start - 600 <= now && now < e.end;
    const ppl = others(e).map(p => p.name);
    rows.push(html`<div class=${'tev' + (past ? ' past' : '') + (live ? ' live' : '')}>
      <div class="tev-time mono">${hhmm(new Date(e.start * 1000))}<br/><span class="faint">${hhmm(new Date(e.end * 1000))}</span></div>
      <div class="tev-body">
        <div class="tev-title">${e.title}</div>
        ${ppl.length ? html`<div class="tev-ppl">${ppl.slice(0, 4).join(', ')}${ppl.length > 4 ? ` +${ppl.length - 4}` : ''}</div>` : null}
        <div class="row" style="gap:10px; margin-top:4px">
          ${rec ? html`<button class="linkbtn" onClick=${() => set({ selected: rec.id })}>${rec.has_transcript ? 'Voir la transcription' : 'Voir l’enregistrement'}</button>`
            : past ? html`<span class="faint">Non enregistré</span>`
            : !recording ? html`${e.meet ? html`<button class="linkbtn" onClick=${() => joinAndRecord(e)}>Rejoindre et enregistrer</button>` : null}
                <button class="linkbtn quiet" onClick=${() => startRecording(null, e.uid)}>Enregistrer</button>` : null}
        </div>
      </div>
    </div>`);
  }
  if (!marked) rows.push(marker);
  return html`<aside class="today"><div class="today-h"><b>Aujourd’hui</b><span class="muted">${day}</span></div>
    ${cal.error ? html`<${Note} kind="warn">Agenda non mis à jour : ${cal.error}<//>` : null}
    ${cal.today.length ? html`<div class="tlist">${rows}</div>` : html`<p class="muted" style="margin-top:8px">Aucun rendez-vous aujourd’hui.</p>`}
  </aside>`;
}

/* ------------------------------------------------------------------ */
/* Frise : niveaux des pistes et locuteurs                             */
/* ------------------------------------------------------------------ */
function Frise({ envelopes, duration, segments, colorOf, time, onSeek, live }) {
  const ref = useRef();
  const lanes = useMemo(() => {
    if (!envelopes || !envelopes.tracks) return [];
    const order = ['micro', 'pc', 'audio'].filter(k => envelopes.tracks[k]);
    return order.map(k => ({ k, data: b64(envelopes.tracks[k]), color: k === 'pc' ? 'var(--pc)' : 'var(--mic)' }));
  }, [envelopes]);
  useEffect(() => {
    const c = ref.current; if (!c) return;
    const dpr = window.devicePixelRatio || 1;
    const W = c.clientWidth, H = c.clientHeight;
    c.width = W * dpr; c.height = H * dpr;
    const g = c.getContext('2d'); g.scale(dpr, dpr); g.clearRect(0, 0, W, H);
    const css = getComputedStyle(document.documentElement);
    const col = v => v.startsWith('var(') ? css.getPropertyValue(v.slice(4, -1)).trim() : v;
    const segH = segments && segments.length ? 8 : 0;
    const waveH = H - segH - (segH ? 4 : 0);
    const total = duration || (lanes[0] ? lanes[0].data.length / 50 : 1);
    // Ondes : une voie par piste ; micro au-dessus de l'axe, PC en dessous.
    const two = lanes.length === 2;
    lanes.forEach((ln, li) => {
      g.fillStyle = col(ln.color); g.globalAlpha = 0.85;
      const n = ln.data.length, mid = two ? waveH / 2 : waveH / 2, half = two ? waveH / 2 - 1 : waveH / 2 - 1;
      for (let x = 0; x < W; x++) {
        const a = Math.floor(x / W * total * 50), b = Math.max(a + 1, Math.floor((x + 1) / W * total * 50));
        let m = 0; for (let i = a; i < b && i < n; i++) m = Math.max(m, ln.data[i]);
        const db = m / 255 * 90 - 90;
        const v = Math.max(0, (db + 60) / 60);
        const hgt = Math.max(v > 0 ? 1 : 0, v * half);
        if (!two) g.fillRect(x, mid - hgt, 1, hgt * 2);
        else if (li === 0) g.fillRect(x, mid - hgt, 1, hgt);
        else g.fillRect(x, mid, 1, hgt);
      }
    });
    g.globalAlpha = 1;
    if (two) { g.fillStyle = col('var(--line)'); g.fillRect(0, waveH / 2, W, 1); }
    // Bande des locuteurs.
    if (segH) {
      for (const s of segments) {
        g.fillStyle = colorOf(s.speaker);
        const x0 = s.start / total * W, x1 = Math.max(x0 + 1.5, s.end / total * W);
        g.fillRect(x0, H - segH, x1 - x0, segH);
      }
    }
    if (time != null && !live) { g.fillStyle = col('var(--ink)'); g.fillRect(Math.round(time / total * W), 0, 1.5, H); }
  }, [lanes, duration, segments, colorOf, time, live]);
  const click = e => {
    if (!onSeek) return;
    const r = ref.current.getBoundingClientRect();
    const total = duration || 1;
    onSeek(Math.max(0, Math.min(total, (e.clientX - r.left) / r.width * total)));
  };
  return html`<div class="frise" onClick=${click}><canvas ref=${ref}></canvas></div>`;
}

/* ------------------------------------------------------------------ */
/* Panneau « Transcrire »                                              */
/* ------------------------------------------------------------------ */
function TranscribePanel({ detail, onCancel, retranscribe }) {
  const settings = useStore(s => s.settings);
  const key = useStore(s => s.key);
  const [o, setO] = useState(() => ({ lang: settings.lang, speakers: settings.speakers, no_verbatim: settings.no_verbatim, keyterms: settings.keyterms, mode: settings.mode }));
  const [plan, setPlan] = useState(detail.plan);
  const [busy, setBusy] = useState(false);
  const two = detail.item.tracks.length === 2;
  useEffect(() => { invoke('estimate', { id: detail.item.id, mode: o.mode }).then(setPlan).catch(() => setPlan(null)); }, [o.mode, detail.item.id]);
  const kt = parseKeyterms(o.keyterms);
  const r = rates({ ...settings, keyterms: o.keyterms });
  const billedMin = plan ? plan.seconds * plan.parts.length / 60 : 0;
  const go = async () => {
    setBusy(true);
    try {
      await invoke('transcribe', { id: detail.item.id, req: { ...o, keyterms: kt.terms } });
      saveSettings({ lang: o.lang, speakers: o.speakers, no_verbatim: o.no_verbatim, keyterms: o.keyterms, mode: o.mode });
      onCancel && onCancel();
    } catch (e) { toast(errMsg(e)); }
    setBusy(false);
  };
  const up = (k, v) => setO(x => ({ ...x, [k]: v }));
  const mode2 = o.mode === 'separate' ? 'separate' : 'auto';
  // Une ligne seulement, et seulement quand c'est utile à savoir.
  const explain = plan && plan.mode === 'mixed' && two && o.mode !== 'separate' ? null
    : plan && plan.mode === 'separate' && plan.parts.length === 1 && two ? 'Une seule source a parlé : elle seule est envoyée.' : null;
  return html`<div class="card">
    <div class="label">${retranscribe ? 'Retranscrire' : 'Transcrire'}</div>
    ${!(key && key.has_key) ? html`<${Note}>Ajoute d’abord ta clé API dans les réglages.<//>` : null}
    ${plan && plan.notes.map(n => html`<${Note} kind="info">${n}<//>`)}
    <div class="stack">
      <div class="grid2">
        <label class="field"><span>Langue</span><select class="inp" value=${o.lang} onChange=${e => up('lang', e.target.value)}>
          <option value="fr">Français</option><option value="en">Anglais</option><option value="es">Espagnol</option><option value="de">Allemand</option><option value="it">Italien</option><option value="auto">Détection automatique</option></select></label>
        <label class="field"><span>Nombre de personnes (si tu le connais)</span><select class="inp" value=${o.speakers} onChange=${e => up('speakers', e.target.value)}>
          <option value="auto">Automatique</option>${[1, 2, 3, 4, 5, 6, 7, 8].map(n => html`<option value=${String(n)}>${n} au plus${plan && plan.parts.length === 2 ? ' par source' : ''}</option>`)}</select></label>
      </div>
      <div class="opts">
        <label class="check inline"><input type="checkbox" checked=${o.no_verbatim} onChange=${e => up('no_verbatim', e.target.checked)} /> Texte nettoyé</label>
        <${Tip} text="Retire les « euh », hésitations et faux départs. Le sens et les mots restent ceux prononcés." />
        ${two ? html`<span class="seg" role="radiogroup" aria-label="Envoi des deux sources">
          ${[['auto', 'Automatique'], ['separate', 'Pistes séparées ×2']].map(([v, t]) =>
            html`<button role="radio" aria-checked=${mode2 === v} class=${mode2 === v ? 'on' : ''} onClick=${() => up('mode', v)}>${t}</button>`)}
        </span>
        <${Tip} text="Automatique : une seule piste envoyée (coût ×1). Tes deux pistes, gardées sur le disque, disent ensuite qui parle. Pistes séparées : chaque source transcrite à part, un peu meilleur quand vous parlez en même temps, coût ×2." />` : null}
      </div>
      <details><summary class="muted" style="cursor:pointer">Vocabulaire spécifique ${kt.terms.length ? `(${kt.terms.length} terme${kt.terms.length > 1 ? 's' : ''}, +20 %)` : ''}</summary>
        <textarea class="inp" style="margin-top:8px" placeholder="Un terme par ligne : noms propres, sigles, jargon" value=${o.keyterms} onInput=${e => up('keyterms', e.target.value)}></textarea>
        <small class="muted">Mémorisé pour les prochaines fois. Ajoute 20 % au prix de toute la transcription (pas par terme).${kt.rejected ? ` ${kt.rejected} terme(s) ignoré(s) : 5 mots et 50 caractères au plus.` : ''}</small></details>
      ${explain ? html`<small class="muted">${explain}</small>` : null}
      <div class="row">
        <button class="btn primary" disabled=${busy || !plan || !(key && key.has_key)} onClick=${go}>${retranscribe ? 'Retranscrire' : 'Transcrire'}</button>
        ${onCancel ? html`<button class="btn ghost" onClick=${onCancel}>Annuler</button>` : null}
        <span class="grow"></span>
        ${plan ? html`<span class="cost">${MODE_LABEL[plan.mode] || ''} · ${dur(plan.seconds)}${plan.parts.length > 1 ? ' × ' + plan.parts.length : ''} · ≈ ${num(billedMin * r.creditsPerMin)} crédits · ≈ ${eur(billedMin * r.eurPerMin)}</span>` : null}
      </div>
    </div>
  </div>`;
}

/* ------------------------------------------------------------------ */
/* Locuteurs et transcription                                          */
/* ------------------------------------------------------------------ */
function useColors(transcript) {
  return useMemo(() => {
    const m = {};
    (transcript ? transcript.speakers : []).forEach((s, i) => m[s.id] = PALETTE[i % PALETTE.length]);
    return id => m[id] || '#888';
  }, [transcript && transcript.speakers.map(s => s.id).join('|')]);
}

function SpeakerChip({ sp, color, pct, transcript, onEdit, people }) {
  const known = useStore(s => s.settings.names);
  const [open, setOpen] = useState(false);
  const [name, setName] = useState(sp.name);
  useEffect(() => setName(sp.name), [sp.name]);
  const commit = () => { if (name.trim() && name.trim() !== sp.name) onEdit({ type: 'rename', id: sp.id, name: name.trim() }); };
  const others = transcript.speakers.filter(x => x.id !== sp.id);
  return html`<span style="position:relative">
    <button class="spk" onClick=${() => setOpen(true)}><span class="sw" style=${'background:' + color}></span>${sp.name}${sp.source ? html`<span class="srcicon" title=${sp.source === 'mic' ? 'entendu au micro' : 'entendu via le PC'}>${srcIcon(sp.source)}</span>` : null}<span class="pct">${pct} %</span></button>
    <${Popover} open=${open} onClose=${() => { commit(); setOpen(false); }} style="top:36px; left:0">
      <div class="head">Nom</div>
      <div style="padding:2px 6px 6px"><input type="text" list="known-names" value=${name} autofocus onInput=${e => setName(e.target.value)} onKeyDown=${e => { if (e.key === 'Enter') { commit(); setOpen(false); } }} /></div>
      <datalist id="known-names">${[...new Set([...(people || []).map(p => p.name), ...(known || [])])].map(n => html`<option value=${n}></option>`)}</datalist>
      ${people && people.length ? html`<div class="sep"></div><div class="head">Participants du rendez-vous</div>
        <div class="quick">${people.filter(p => p.name !== sp.name).map(p => html`<button class="pchip" onClick=${() => { setOpen(false); onEdit({ type: 'rename', id: sp.id, name: p.name }); }}>${p.name}</button>`)}</div>` : null}
      ${others.length ? html`<div class="sep"></div><div class="head">C’est la même personne que…</div>
        ${others.map(o => html`<button class="item" onClick=${() => { setOpen(false); onEdit({ type: 'merge', from: sp.id, into: o.id }); }}>Fusionner avec ${o.name}</button>`)}` : null}
    <//>
  </span>`;
}

function Paragraph({ seg, transcript, colorOf, onEdit, onSeek }) {
  const [open, setOpen] = useState(false);
  return html`<p class="para">
    <button class="reassign" title="Changer de locuteur" onClick=${() => setOpen(true)}>${I.swap}</button>
    ${seg.text}
    <${Popover} open=${open} onClose=${() => setOpen(false)} style="top:22px; left:-30px">
      <div class="head">Ce passage est de…</div>
      ${transcript.speakers.map(s => html`<button class="item" disabled=${s.id === seg.speaker} onClick=${() => { setOpen(false); onEdit({ type: 'assign', seg: seg.id, to: s.id }); }}><span class="sw" style=${`width:9px;height:9px;border-radius:50%;background:${colorOf(s.id)}`}></span>${s.name}</button>`)}
      <div class="sep"></div>
      <button class="item" onClick=${() => { setOpen(false); onEdit({ type: 'assign', seg: seg.id, to: null }); }}>Une nouvelle personne</button>
      <button class="item" onClick=${() => { setOpen(false); onEdit({ type: 'assign-rest', seg: seg.id }); }}>Réattribuer ce paragraphe et la suite du tour</button>
      <div class="sep"></div>
      <button class="item" onClick=${() => { setOpen(false); onSeek(seg.start); }}>Écouter ce passage</button>
    <//>
  </p>`;
}

function applyEdit(t, ed) {
  const a = { speakers: t.speakers.map(s => ({ ...s })), assign: {} };
  t.segments.forEach(s => a.assign[s.id] = s.speaker);
  const segOf = id => t.segments.find(s => s.id === id);
  if (ed.type === 'rename') a.speakers.find(s => s.id === ed.id).name = ed.name;
  if (ed.type === 'merge') for (const k in a.assign) if (a.assign[k] === ed.from) a.assign[k] = ed.into;
  if (ed.type === 'assign' || ed.type === 'assign-rest') {
    const seg = segOf(ed.seg);
    let to = ed.to;
    if (!to) {
      let n = 1; while (a.speakers.some(s => s.id === 'p' + n)) n++;
      to = 'p' + n;
      a.speakers.push({ id: to, name: 'Personne ' + (a.speakers.length + 1), source: seg.source || null });
    }
    if (ed.type === 'assign') a.assign[seg.id] = to;
    else {
      const from = seg.speaker; let on = false;
      for (const s of t.segments) { if (s.id === seg.id) on = true; if (!on) continue; if (s.speaker !== from) break; a.assign[s.id] = to; }
    }
  }
  return a;
}

function TranscriptView({ detail, time, onSeek, reload }) {
  const [t, setT] = useState(detail.transcript);
  const [undo, setUndo] = useState([]);
  useEffect(() => { setT(detail.transcript); }, [detail.transcript]);
  // L'historique ne repart de zéro qu'avec une nouvelle version de transcription.
  useEffect(() => { setUndo([]); }, [detail.item.current]);
  const colorOf = useColors(t);
  const id = detail.item.id;
  const save = async (attribution, prev) => {
    try { const nt = await invoke('attribution_save', { id, attribution }); if (prev) setUndo(u => [...u.slice(-29), prev]); setT(nt); }
    catch (e) { toast('Correction refusée : ' + errMsg(e)); }
  };
  const edit = ed => { const prev = { speakers: t.speakers, assign: Object.fromEntries(t.segments.map(s => [s.id, s.speaker])) }; save(applyEdit(t, ed), prev); };
  const back = () => { const prev = undo[undo.length - 1]; if (!prev) return; setUndo(u => u.slice(0, -1)); save(prev, null); };
  const talk = useMemo(() => {
    const m = {}; let tot = 0;
    t.segments.forEach(s => { const d = Math.max(0.1, s.end - s.start); m[s.speaker] = (m[s.speaker] || 0) + d; tot += d; });
    return id => Math.round((m[id] || 0) / (tot || 1) * 100);
  }, [t]);
  const turns = useMemo(() => {
    const out = [];
    for (const s of t.segments) {
      const last = out[out.length - 1];
      if (last && last.speaker === s.speaker) last.segs.push(s); else out.push({ speaker: s.speaker, start: s.start, segs: [s] });
    }
    return out;
  }, [t]);
  const nameOf = id => (t.speakers.find(s => s.id === id) || {}).name || '?';
  const now = time != null ? t.segments.find(s => time >= s.start && time < s.end + 0.6) : null;
  const exportAs = f => invoke('export', { id, format: f }).then(p => p && toast('Exporté : ' + p)).catch(e => toast(errMsg(e)));
  const [exp, setExp] = useState(false);
  return html`<div>
    <div class="card">
      <div class="label">Participants</div>
      <div class="speakers">
        ${t.speakers.map(sp => html`<${SpeakerChip} sp=${sp} color=${colorOf(sp.id)} pct=${talk(sp.id)} transcript=${t} onEdit=${edit} people=${detail.item.event ? detail.item.event.people : null} />`)}
        <span class="grow"></span>
        ${undo.length ? html`<button class="btn small" onClick=${back}>Annuler la correction</button>` : null}
      </div>
      <small class="muted" style="display:block; margin-top:10px">Clique sur un nom pour le changer ou fusionner deux personnes, et sur ⇄ à gauche d’un paragraphe pour le réattribuer.</small>
    </div>
    <div class="toolbar">
      <span class="label" style="margin:0">Transcription</span><span class="faint mono" style="font-size:11.5px">${MODE_LABEL[t.mode] || ''}${t.language ? ' · ' + t.language : ''}</span>
      <span class="grow"></span>
      <span style="position:relative"><button class="btn small" onClick=${() => setExp(true)}>Exporter ${I.chev}</button>
        <${Popover} open=${exp} onClose=${() => setExp(false)} style="top:32px; right:0">
          <button class="item" onClick=${() => { setExp(false); exportAs('txt'); }}>Texte (.txt)</button>
          <button class="item" onClick=${() => { setExp(false); exportAs('md'); }}>Markdown (.md)</button>
          <button class="item" onClick=${() => { setExp(false); exportAs('srt'); }}>Sous-titres (.srt)</button>
        <//></span>
      <button class="btn small" onClick=${() => navigator.clipboard.writeText(turns.map(x => `[${clock(x.start)}] ${nameOf(x.speaker)}\n${x.segs.map(s => s.text).join('\n\n')}`).join('\n\n')).then(() => toast('Copié.'))}>Copier</button>
      <button class="btn small" onClick=${reload.retranscribe}>Retranscrire…</button>
      <button class="btn small ghost" title="Revenir à l’attribution d’origine" onClick=${async () => { try { setUndo([]); setT(await invoke('transcript_reset', { id })); } catch (e) { toast(errMsg(e)); } }}>Réinitialiser</button>
    </div>
    ${t.notes.map(n => html`<${Note} kind="info">${n}<//>`)}
    <${CostLow} item=${detail.item} />
    <div>${turns.map(tr => html`<div class=${'turn' + (now && tr.segs.includes(now) ? ' now' : '')}>
      <button class="ts" onClick=${() => onSeek(tr.start)}>${clock(tr.start)}</button>
      <div>
        <span class="who" style=${'color:' + colorOf(tr.speaker)}>${nameOf(tr.speaker)}</span>
        ${tr.segs.map(seg => html`<${Paragraph} seg=${seg} transcript=${t} colorOf=${colorOf} onEdit=${edit} onSeek=${onSeek} />`)}
      </div>
    </div>`)}</div>
    <${CostLine} item=${detail.item} />
  </div>`;
}

const fmtN = n => Math.round(n).toLocaleString('fr-FR');
const fmtMin = m => m >= 90 ? `${Math.floor(m / 60)} h ${String(Math.round(m % 60)).padStart(2, '0')}` : `${Math.round(m)} min`;
const currentRef = item => (item.transcripts || []).find(r => r.dir === item.current);

// Fin de transcription : ce que ça a coûté, ce qui reste.
function CostLine({ item }) {
  const r = currentRef(item);
  if (!r || !r.cost) return null;
  const c = r.cost;
  return html`<div class="costline">
    <span>Coût : <b>${fmtN(c.actual != null ? c.actual : c.estimated)} crédits</b>${c.actual != null ? html` <span class="faint">(estimé ${fmtN(c.estimated)})</span>` : html` <span class="faint">(estimé)</span>`}</span>
    ${c.remaining != null ? html`<span>Reste : <b>${fmtN(c.remaining)} crédits</b>${c.remaining_minutes != null ? ` ≈ ${fmtMin(c.remaining_minutes)} de transcription` : ''}</span>` : null}
    ${c.note ? html`<span class=${/écart/.test(c.note) ? 'warn' : 'faint'}>${c.note}</span>` : null}
  </div>`;
}

// Alerte visible en haut de la transcription quand le solde devient juste.
function CostLow({ item }) {
  const r = currentRef(item);
  if (!r || !r.cost || !r.cost.low) return null;
  const c = r.cost;
  return html`<${Note} kind="warn">Il te reste ${c.remaining_minutes != null ? '≈ ' + fmtMin(c.remaining_minutes) + ' de transcription' : 'peu de crédits'} : sans doute pas assez pour une conversation de cette durée.
    <button class="linkbtn" onClick=${() => openUrl('https://elevenlabs.io/app/subscription')}>Recharger</button><//>`;
}

/* ------------------------------------------------------------------ */
/* Détail d'une conversation                                           */
/* ------------------------------------------------------------------ */
function JobCard({ item }) {
  const j = item.job;
  if (!j) return null;
  const quota = j.state === 'error' && /crédits/i.test(j.message || '');
  if (j.state === 'error') return html`<div class="card"><${Note} kind="err">${j.message || 'Transcription interrompue.'}<//>
    <div class="row">${quota ? html`<button class="btn" onClick=${() => openUrl('https://elevenlabs.io/app/subscription')}>Recharger sur ElevenLabs</button>` : null}
    <button class="btn primary" onClick=${() => { invoke('transcribe', { id: item.id, req: null }).catch(e => toast(errMsg(e))); fetchBalance(); }}>Relancer</button>
    <button class="btn ghost" onClick=${() => invoke('transcribe_cancel', { id: item.id })}>Ignorer</button></div></div>`;
  const label = { queued: 'En attente', preparing: 'Préparation de l’audio', uploading: 'Envoi à ElevenLabs', processing: 'Transcription par ElevenLabs' }[j.state] || j.state;
  const busy = j.state === 'processing' || j.state === 'queued' || j.progress == null;
  return html`<div class="card">
    <div class="row"><b>${label}</b>${!busy ? html`<span class="mono muted">${Math.round(j.progress * 100)} %</span>` : null}<span class="grow"></span>
      <button class="btn small ghost" onClick=${() => invoke('transcribe_cancel', { id: item.id })}>Annuler</button></div>
    ${j.message ? html`<small class="muted">${j.message}</small>` : null}
    <div class=${'progress' + (busy ? ' busy' : '')}><i style=${`width:${Math.round((j.progress || 0) * 100)}%`}></i></div>
    ${j.state === 'processing' ? html`<small class="muted" style="display:block;margin-top:8px">Une heure d’audio prend en général quelques minutes. Tu peux continuer à utiliser Verbatim.</small>` : null}
  </div>`;
}

function Player({ detail, audioRef, time, setTime, colorOf }) {
  const [playing, setPlaying] = useState(false);
  const [rate, setRate] = useState(1);
  const total = detail.item.duration || 0;
  const src = useMemo(() => detail.playback ? T.core.convertFileSrc(detail.playback) : null, [detail.playback]);
  useEffect(() => {
    const a = audioRef.current; if (!a) return;
    let raf = 0;
    const tick = () => { setTime(a.currentTime); raf = requestAnimationFrame(tick); };
    const on = () => { setPlaying(true); raf = requestAnimationFrame(tick); };
    const off = () => { setPlaying(false); cancelAnimationFrame(raf); setTime(a.currentTime); };
    a.addEventListener('play', on); a.addEventListener('pause', off); a.addEventListener('ended', off); a.addEventListener('seeked', () => setTime(a.currentTime));
    return () => { cancelAnimationFrame(raf); a.removeEventListener('play', on); a.removeEventListener('pause', off); a.removeEventListener('ended', off); };
  }, [src]);
  const seek = t => { const a = audioRef.current; if (a) { a.currentTime = t; setTime(t); } };
  const lanes = detail.envelopes && detail.envelopes.tracks ? Object.keys(detail.envelopes.tracks) : [];
  return html`<div class="player"><div class="card">
    <${Frise} envelopes=${detail.envelopes} duration=${total} segments=${detail.transcript ? detail.transcript.segments : null} colorOf=${colorOf} time=${time} onSeek=${src ? seek : null} />
    <div class="controls">
      ${src ? html`<button class="play" aria-label=${playing ? 'Pause' : 'Lecture'} onClick=${() => { const a = audioRef.current; playing ? a.pause() : a.play().catch(e => toast('Lecture impossible : ' + errMsg(e))); }}>${playing ? I.pause : I.play}</button>
        <button class="btn small ghost" onClick=${() => seek(Math.max(0, (time || 0) - 10))}>−10 s</button>
        <button class="btn small ghost" onClick=${() => seek(Math.min(total, (time || 0) + 10))}>+10 s</button>
        <span class="time">${clock(time || 0)} / ${clock(total)}</span>
        <button class="btn small ghost mono" onClick=${() => { const r = { 1: 1.25, 1.25: 1.5, 1.5: 2, 2: 1 }[rate]; setRate(r); audioRef.current.playbackRate = r; }}>${String(rate).replace('.', ',')}×</button>` : html`<span class="muted">Audio non conservé pour cette conversation.</span>`}
      <span class="grow"></span>
      <span class="lanes">${lanes.includes('micro') ? html`<span><i style="background:var(--mic)"></i>micro</span>` : null}${lanes.includes('pc') ? html`<span><i style="background:var(--pc)"></i>son du PC</span>` : null}</span>
    </div>
    ${src ? html`<audio ref=${audioRef} src=${src} preload="metadata"></audio>` : null}
  </div></div>`;
}

// Nom d'un participant, modifiable : la correction vaut pour toutes les
// conversations futures (annuaire) et pour la voix qui portait l'ancien nom.
function PersonChip({ p, itemId }) {
  const [edit, setEdit] = useState(false);
  const [v, setV] = useState(p.name);
  // Nom affiché tout de suite après validation, sans attendre le rechargement.
  const [shown, setShown] = useState(null);
  useEffect(() => { setV(p.name); setShown(null); }, [p.name]);
  const save = () => {
    setEdit(false);
    const n = v.trim();
    if (!n || n === p.name) { setV(p.name); return; }
    setShown(n);
    invoke('person_rename', { id: itemId, email: p.email, name: n })
      .then(() => { refreshState(); refreshCalendar(); })
      .catch(e => { setShown(null); setV(p.name); toast(errMsg(e)); });
  };
  if (edit) return html`<input class="pchip-inp" value=${v} autofocus onInput=${e => setV(e.target.value)} onBlur=${save} onKeyDown=${e => { if (e.key === 'Enter') e.target.blur(); if (e.key === 'Escape') { setV(p.name); setEdit(false); } }} />`;
  return html`<button class=${'pchip' + (p.guessed && !shown ? ' guessed' : '')} title=${p.email + (p.guessed ? ' · nom deviné, clique pour le corriger' : ' · clique pour corriger')} onClick=${() => setEdit(true)}>${shown || p.name}${p.me ? ' (toi)' : ''}</button>`;
}

function EventRow({ item }) {
  const connected = useStore(s => s.cal.connected);
  const [open, setOpen] = useState(false);
  const [around, setAround] = useState(null);
  const e = item.event;
  if (!e && !(connected && item.kind === 'recording')) return null;
  const choose = async () => {
    setOpen(true); setAround(null);
    try { setAround(await invoke('calendar_around', { ts: Math.round(parseDate(item.created).getTime() / 1000) })); } catch (err) { setAround([]); toast(errMsg(err)); }
  };
  const link = ev => { setOpen(false); invoke('item_link_event', { id: item.id, event: ev }).catch(err => toast(errMsg(err))); };
  return html`<div class="evrow">
    ${I.cal}
    ${e ? html`<span class="muted">${fmtRange(e)}</span>
        <span class="pchips">${e.people.map(p => html`<${PersonChip} p=${p} itemId=${item.id} />`)}</span>
        ${e.meet ? html`<button class="linkbtn" onClick=${() => openUrl(e.meet)}>Visio</button>` : null}`
      : html`<span class="muted">Aucun rendez-vous lié.</span>`}
    <span style="position:relative"><button class="linkbtn quiet" onClick=${choose}>${e ? 'Changer' : 'Lier à un rendez-vous…'}</button>
      <${Popover} open=${open} onClose=${() => setOpen(false)} style="top:26px; right:0; width:320px">
        <div class="head">Rendez-vous autour de ${hhmm(parseDate(item.created))}</div>
        ${around === null ? html`<div class="item muted">Lecture de l’agenda…</div>`
          : around.length ? around.map(ev => html`<button class="item" onClick=${() => link(ev)}><span class="grow">${ev.title}</span><span class="muted mono" style="font-size:11.5px">${fmtRange(ev)}</span></button>`)
          : html`<div class="item muted">Aucun rendez-vous à cette heure-là.</div>`}
        ${e ? html`<div class="sep"></div><button class="item" onClick=${() => link(null)}>Délier</button>` : null}
      <//></span>
  </div>`;
}

function Detail({ id }) {
  const sum = useStore(s => s.items[id]);
  const prog = useStore(s => s.progress[id]);
  const recording = useStore(s => s.recording && s.recording.id === id);
  const live = useStore(s => s.recording && s.recording.id === id ? s.live : null);
  const [detail, setDetail] = useState(null);
  const [loadErr, setLoadErr] = useState(null);
  const [time, setTime] = useState(null);
  const [retranscribe, setRetranscribe] = useState(false);
  const [confirmDel, setConfirmDel] = useState(false);
  const audioRef = useRef();
  const sig = sum ? [sum.state, sum.has_transcript, sum.title, Math.round(sum.duration || 0), sum.speakers.join('|'), sum.job && sum.job.state === 'error', sum.event_uid, (sum.people || []).join('|'), sum.cost].join('#') : '';
  const load = useCallback(() => invoke('item_get', { id }).then(d => { setDetail(d); setLoadErr(null); }).catch(e => setLoadErr(errMsg(e))), [id]);
  useEffect(() => { setDetail(null); setTime(null); setRetranscribe(false); load(); }, [id]);
  // Pendant l'enregistrement, seul le rendez-vous lié (titre, participants) est rechargé.
  useEffect(() => { if (detail) load(); }, [recording ? (sum && [sum.event_uid, (sum.people || []).join('|')].join('#')) : sig]);
  const colorOf = useColors(detail && detail.transcript);
  const seek = t => { const a = audioRef.current; if (a) { a.currentTime = t; setTime(t); a.play().catch(() => {}); } };
  useEffect(() => {
    const k = e => { if (e.code === 'Space' && !/INPUT|TEXTAREA|SELECT|BUTTON/.test(document.activeElement.tagName) && audioRef.current) { e.preventDefault(); const a = audioRef.current; a.paused ? a.play() : a.pause(); } };
    window.addEventListener('keydown', k); return () => window.removeEventListener('keydown', k);
  }, []);
  if (loadErr) return html`<div class="page"><${Note} kind="err">${loadErr}<//></div>`;
  if (!detail || !sum) return html`<div class="page"><span class="muted">Chargement…</span></div>`;
  const it = detail.item;
  const created = parseDate(it.created);
  const devs = Object.entries(it.devices || {});
  // Juste après l'arrêt, le nom par défaut est sélectionné : on peut le remplacer en tapant.
  const nameNow = it.state === 'finalizing' && /^Enregistrement( \(\d+\))?$/.test(sum.title);
  return html`<div class="page">
    <button class="back" onClick=${() => set({ selected: null })}>‹ Accueil</button>
    <div class="head"><${TitleField} id=${id} title=${sum.title} autoFocus=${nameNow} /></div>
    ${nameNow ? html`<small class="muted" style="display:block;margin:-4px 0 8px">Donne-lui un nom pendant la finalisation : tape simplement, puis Entrée.</small>` : null}
    <div class="sub">
      <span>${created.toLocaleDateString('fr-FR', { weekday: 'long', day: 'numeric', month: 'long' })} · ${hhmm(created)}</span>
      ${sum.duration ? html`<span>${dur(sum.duration)}</span>` : null}
      ${it.kind === 'recording' ? (it.tracks.length ? it.tracks : (it.roles || []).map(r => ({ role: r }))).map(t => html`<span class="src">${t.role === 'mic' ? I.mic : I.pc}${devs.find(d => d[0] === (t.role === 'mic' ? 'micro' : 'pc'))?.[1] || (t.role === 'mic' ? 'Micro' : 'Son du PC')}</span>`) : null}
      ${it.source ? html`<span class="src">${I.file}${it.source.name}</span>` : null}
      <span class="grow"></span>
      <button class="linkbtn" onClick=${() => invoke('open_folder', { id })}>Ouvrir le dossier</button>
      ${!recording ? (confirmDel
        ? html`<span class="row" style="gap:6px"><button class="btn small danger" onClick=${async () => { try { await invoke('item_trash', { id }); } catch (e) { toast(errMsg(e)); setConfirmDel(false); } }}>Mettre à la corbeille</button><button class="btn small ghost" onClick=${() => setConfirmDel(false)}>Garder</button></span>`
        : html`<button class="linkbtn quiet" onClick=${() => setConfirmDel(true)}>Supprimer</button>`) : null}
    </div>

    <${EventRow} item=${it} />
    ${recording ? html`<div class="card"><div class="label">Enregistrement en cours</div>
      <p class="muted">Les deux pistes sont écrites sur le disque en continu. Tu peux naviguer dans la bibliothèque pendant ce temps.</p>
      ${live ? html`<div class="row" style="margin-top:12px; gap:24px">${live.tracks.map(t => html`<${Meter} t=${t} />`)}</div>` : null}</div>` : null}
    ${it.state === 'finalizing' || it.state === 'importing' ? html`<div class="card"><b>${it.state === 'finalizing' ? 'Finalisation de l’enregistrement' : 'Import du fichier'}</b>
      <div class="progress"><i style=${`width:${Math.round(((prog && prog.progress) || 0) * 100)}%`}></i></div></div>` : null}
    ${it.state === 'failed' ? html`<${Note} kind="err">${it.error || 'Erreur.'}<//>` : null}
    ${it.notes.map(n => html`<${Note} kind="info">${n}<//>`)}

    ${it.state === 'ready' && (detail.playback || detail.envelopes) ? html`<${Player} detail=${detail} audioRef=${audioRef} time=${time} setTime=${setTime} colorOf=${colorOf} />` : null}
    <${JobCard} item=${sum} />
    ${it.state === 'ready' && !sum.job && (!detail.transcript || retranscribe) && detail.plan ? html`<${TranscribePanel} detail=${detail} retranscribe=${!!detail.transcript} onCancel=${detail.transcript ? () => setRetranscribe(false) : null} />` : null}
    ${it.state === 'ready' && !detail.transcript && !detail.plan && !sum.job ? html`<${Note} kind="info">Cette conversation ne peut pas être transcrite : l’audio n’a pas été conservé.<//>` : null}
    ${detail.transcript ? html`<${TranscriptView} detail=${detail} time=${time} onSeek=${seek} reload=${{ retranscribe: () => setRetranscribe(true) }} />` : null}
  </div>`;
}

/* ------------------------------------------------------------------ */
/* Réglages                                                            */
/* ------------------------------------------------------------------ */
function CalendarSettings() {
  const st = useStore(s => s.calState);
  const people = useStore(s => s.settings.people);
  const [url, setUrl] = useState('');
  const [busy, setBusy] = useState(false);
  const [showPeople, setShowPeople] = useState(false);
  useEffect(() => { invoke('calendar_state').then(c => set({ calState: c })).catch(() => {}); }, []);
  const connect = async () => {
    setBusy(true);
    try { const c = await invoke('calendar_set', { url }); set({ calState: c }); setUrl(''); refreshCalendar(true); toast('Agenda connecté.'); }
    catch (e) { toast('Agenda non connecté : ' + errMsg(e)); }
    setBusy(false);
  };
  const disconnect = async () => { const c = await invoke('calendar_set', { url: null }); set({ calState: c, cal: { connected: false, now: [], next: null, error: null } }); };
  const entries = Object.entries(people);
  return html`<section><div class="label">Agenda</div>
    ${st && st.connected ? html`<div class="stack">
        <div class="row"><span class="grow">Connecté${st.account ? html` : <b>${st.account}</b>` : ''}</span><button class="btn small ghost" onClick=${disconnect}>Déconnecter</button></div>
        <small class="muted">Au lancement, Verbatim repère le rendez-vous en cours ou qui commence dans les 10 minutes : titre, participants, lien de visio. L’enregistrement y est lié automatiquement.</small>
      </div>`
    : html`<div class="stack">
        <small class="muted">Google Agenda → ⚙ Paramètres → ton agenda (à gauche) → « Intégrer l’agenda » → copie l’<b>adresse secrète au format iCal</b>. Lecture seule, chiffrée sur ce PC comme ta clé.</small>
        <div class="row"><input class="inp grow" type="password" placeholder="https://calendar.google.com/calendar/ical/…/basic.ics" value=${url} onInput=${e => setUrl(e.target.value)} />
          <button class="btn small primary" disabled=${busy || !url.trim()} onClick=${connect}>${busy ? 'Vérification…' : 'Connecter'}</button></div>
      </div>`}
    ${entries.length ? html`<details open=${showPeople} onToggle=${e => setShowPeople(e.target.open)} style="margin-top:10px"><summary class="muted" style="cursor:pointer">Noms des personnes (${entries.length})</summary>
      <div class="people">${entries.map(([email, name]) => html`<div class="row"><span class="grow"><b>${name}</b> <small class="muted">${email}</small></span>
        <button class="linkbtn quiet" onClick=${() => invoke('person_rename', { id: null, email, name: '' }).then(() => { refreshState(); refreshCalendar(); })}>Oublier</button></div>`)}</div>
      <small class="muted">Corrige un nom en cliquant dessus dans une conversation liée : il sera réutilisé ensuite.</small></details>` : null}
  </section>`;
}

// Boîte à idées : ouvre une « issue » GitHub pré-remplie, publiée avec ton
// propre compte. Sur GitHub, tes idées sont marquées « Owner » et celles des
// autres non : c'est ce qui permet de ne suivre que les tiennes sans vérification.
function IdeaBox({ onClose }) {
  const repo = useStore(s => s.repo);
  const version = useStore(s => s.version);
  const [title, setTitle] = useState('');
  const [body, setBody] = useState('');
  const text = () => `${body.trim()}\n\n---\nVerbatim ${version} · proposé depuis l’application`;
  const send = () => {
    const url = `https://github.com/${repo}/issues/new?` + new URLSearchParams({ title: 'Idée : ' + title.trim(), body: text(), labels: 'idée' }).toString();
    openUrl(url); onClose();
  };
  return html`<div class="stack">
    <input class="inp" placeholder="En une phrase" value=${title} onInput=${e => setTitle(e.target.value)} />
    <textarea class="inp" placeholder="Ce qui te gêne ou ce que tu voudrais, avec un exemple si possible" value=${body} onInput=${e => setBody(e.target.value)}></textarea>
    <div class="row"><button class="btn small primary" disabled=${!title.trim() || !repo} onClick=${send}>Ouvrir sur GitHub</button>
      <button class="btn small" disabled=${!title.trim()} onClick=${() => navigator.clipboard.writeText(title.trim() + '\n\n' + text()).then(() => toast('Idée copiée.'))}>Copier</button>
      <button class="btn small ghost" onClick=${onClose}>Annuler</button></div>
    <small class="muted">La page GitHub s’ouvre pré-remplie : il ne reste qu’à valider avec ton compte. Aucun enregistrement ni texte de conversation n’y est joint.</small>
  </div>`;
}

function Drawer() {
  const open = useStore(s => s.drawer);
  const settings = useStore(s => s.settings);
  const balance = useStore(s => s.balance);
  const balanceError = useStore(s => s.balanceError);
  const devices = useStore(s => s.devices);
  const library = useStore(s => s.library);
  const simulated = useStore(s => s.simulated);
  const version = useStore(s => s.version);
  const updates = useStore(s => s.updates);
  const [idea, setIdea] = useState(false);
  const [name, setName] = useState(settings.my_name);
  useEffect(() => { if (open) { invoke('devices').then(d => set({ devices: d })).catch(() => {}); fetchBalance(); setName(settings.my_name); } }, [open]);
  if (!open) return null;
  const r = rates(settings);
  const minutes = balance ? balance.left / r.creditsPerMin : 0;
  const pickDev = (role, v, list) => {
    const name = (list.find(d => d[0] === v) || [])[1] || '';
    saveSettings(role === 'mic' ? { mic_device: v, mic_device_name: name } : { pc_device: v, pc_device_name: name });
  };
  const missing = (v, list) => v && !['default', 'all'].includes(v) && list && !list.some(d => d[0] === v);
  return html`<div class="backdrop" style="background:rgba(0,0,0,.18); z-index:45" onClick=${() => set({ drawer: false })}></div>
  <aside class="drawer" role="dialog" aria-label="Réglages">
    <div class="row"><h2 class="grow">Réglages</h2><button class="btn ghost" onClick=${() => set({ drawer: false })}>Fermer</button></div>
    <section><div class="label">Clé ElevenLabs</div><${KeySetup} compact /></section>
    <section><div class="label">Solde</div>
      ${balance ? html`<div class="figs">
          <div class="fig"><div class="k">Crédits</div><div class="v">${num(balance.left)}</div></div>
          <div class="fig"><div class="k">Soit</div><div class="v">${dur(minutes * 60)}</div></div>
          <div class="fig"><div class="k">Valeur</div><div class="v" style="color:var(--accent)">${eur(minutes * r.eurPerMin)}</div></div></div>
        <div class="gauge"><i class=${balance.limit && balance.left / balance.limit < 0.15 ? 'low' : ''} style=${`width:${balance.limit ? Math.min(100, balance.left / balance.limit * 100) : 0}%`}></i></div>
        <small class="muted" style="display:block;margin-top:8px">Formule ${balance.tier}${balance.reset ? ' · renouvellement le ' + balance.reset.toLocaleDateString('fr-FR', { day: 'numeric', month: 'long' }) : ''} · ${num(settings.credits_per_min)} crédits/min ${settings.credits_per_min !== 330 ? '(mesuré)' : '(estimé)'}</small>
        <div class="row" style="margin-top:10px"><button class="btn small" onClick=${fetchBalance}>Actualiser</button><button class="btn small primary" onClick=${() => openUrl('https://elevenlabs.io/app/subscription')}>Recharger</button></div>`
        : html`<span class="muted">${balanceError || 'Ajoute ta clé pour voir le solde.'}</span>`}
    </section>
    <section><div class="label">Toi</div>
      <label class="field"><span>Ton prénom (donné à la voix du micro)</span><input class="inp" value=${name} onInput=${e => setName(e.target.value)} onBlur=${() => saveSettings({ my_name: name })} /></label>
      <div class="field"><span>Thème</span><span class="seg" role="radiogroup" aria-label="Thème" style="margin-left:0">
        ${[['auto', 'Comme Windows'], ['light', 'Clair'], ['dark', 'Sombre']].map(([v, t]) =>
          html`<button role="radio" aria-checked=${(settings.theme || 'auto') === v} class=${(settings.theme || 'auto') === v ? 'on' : ''} onClick=${() => saveSettings({ theme: v })}>${t}</button>`)}
      </span></div>
    </section>
    <${CalendarSettings} />
    <section><div class="label">Audio</div>
      ${simulated ? html`<${Note} kind="info">Sources simulées (environnement de test).<//>` : null}
      <div class="stack">
        <label class="field"><span class="row">${I.mic} Micro</span>
          <select class="inp" value=${settings.mic_device} onChange=${e => pickDev('mic', e.target.value, devices ? devices.inputs : [])}>
            <option value="default">Par défaut de Windows (suit tes changements)</option>
            ${missing(settings.mic_device, devices && devices.inputs) ? html`<option value=${settings.mic_device}>${settings.mic_device_name || 'Périphérique absent'} (absent)</option>` : null}
            ${(devices ? devices.inputs : []).map(d => html`<option value=${d[0]}>${d[1]}${d[2] ? ' · par défaut' : ''}</option>`)}
          </select></label>
        <label class="field"><span class="row">${I.pc} Son du PC</span>
          <select class="inp" value=${settings.pc_device} onChange=${e => pickDev('pc', e.target.value, devices ? devices.outputs : [])}>
            <option value="all">Tout le son du PC (recommandé)</option>
            <option value="default">Sortie par défaut de Windows</option>
            ${missing(settings.pc_device, devices && devices.outputs) ? html`<option value=${settings.pc_device}>${settings.pc_device_name || 'Périphérique absent'} (absent)</option>` : null}
            ${(devices ? devices.outputs : []).map(d => html`<option value=${d[0]}>${d[1]}</option>`)}
          </select></label>
        <small class="muted">« Tout le son du PC » capte l’appel quel que soit le casque branché, sans enregistrer Verbatim lui-même. Un périphérique choisi et absent est remplacé par celui de Windows.</small>
      </div>
    </section>
    <section><div class="label">Transcription</div>
      <div class="stack">
        <label class="check"><input type="checkbox" checked=${settings.auto_transcribe} onChange=${e => saveSettings({ auto_transcribe: e.target.checked })} /><span><b>Transcrire automatiquement</b><small>À l’arrêt d’un enregistrement et après un import, avec les réglages ci-dessous.</small></span></label>
        <div class="grid2">
          <label class="field"><span>Langue</span><select class="inp" value=${settings.lang} onChange=${e => saveSettings({ lang: e.target.value })}>
            <option value="fr">Français</option><option value="en">Anglais</option><option value="es">Espagnol</option><option value="de">Allemand</option><option value="it">Italien</option><option value="auto">Détection automatique</option></select></label>
          <label class="field"><span class="row">Deux sources <${Tip} text="Automatique : une piste envoyée, coût ×1, qui parle est retrouvé grâce à tes deux pistes locales. Pistes séparées : coût ×2, un peu meilleur quand vous parlez en même temps." /></span><select class="inp" value=${settings.mode === 'separate' ? 'separate' : 'auto'} onChange=${e => saveSettings({ mode: e.target.value })}>
            <option value="auto">Automatique (coût ×1)</option><option value="separate">Pistes séparées (coût ×2)</option></select></label>
        </div>
        <div class="row"><label class="check inline"><input type="checkbox" checked=${settings.no_verbatim} onChange=${e => saveSettings({ no_verbatim: e.target.checked })} /> Texte nettoyé</label><${Tip} text="Sans « euh », hésitations ni faux départs." /></div>
        <label class="field"><span class="row">Vocabulaire spécifique, toujours appliqué <${Tip} text="Noms propres, sigles, jargon : un terme par ligne. Utilisé à chaque transcription. Ajoute 20 % au prix de toute la transcription, quel que soit le nombre de termes. Laisse vide pour ne pas payer ce supplément." /></span><textarea class="inp" placeholder="Un terme par ligne" value=${settings.keyterms} onBlur=${e => saveSettings({ keyterms: e.target.value })}></textarea></label>
      </div>
    </section>
    <section><div class="label">Bibliothèque</div>
      <div class="stack"><code class="mono" style="font-size:12px; word-break:break-all">${library}</code>
        <div class="row"><button class="btn small" onClick=${() => invoke('open_folder', { id: null })}>${I.folder} Ouvrir</button>
          <button class="btn small" onClick=${async () => { const p = await invoke('pick_library_dir'); if (p) { await saveSettings({ library_dir: p }); await refreshState(); await loadLibrary(); set({ selected: null }); } }}>Changer de dossier…</button></div>
        <small class="muted">Chaque conversation est un dossier lisible sans Verbatim : audio FLAC, texte, réponses d’ElevenLabs. Une conversation supprimée va dans le sous-dossier .corbeille.</small></div>
    </section>
    <section><div class="label">Version</div>
      <div class="stack"><span>Verbatim ${version}</span>
        <div class="row">${updates ? html`<button class="btn small" onClick=${() => checkUpdate(true)}>Rechercher une mise à jour</button>` : null}
          <button class="btn small" onClick=${() => installUpdate(null)}>Installer depuis un fichier…</button></div>
        <div class="row"><button class="btn small" onClick=${() => setIdea(x => !x)}>Proposer une idée</button>
          <button class="btn small ghost" title="Démarrages, périphériques, erreurs : sans titres, noms ni texte des conversations" onClick=${() => invoke('journal_show').catch(e => toast(errMsg(e)))}>Ouvrir le journal</button></div>
        ${idea ? html`<${IdeaBox} onClose=${() => setIdea(false)} />` : null}
        <small class="muted">Une mise à jour remplace seulement le programme : conversations, clé et réglages restent en place. Tu peux aussi glisser un Verbatim.exe ou son zip dans la fenêtre.</small></div>
    </section>
  </aside>`;
}

/* ------------------------------------------------------------------ */
/* Application                                                         */
/* ------------------------------------------------------------------ */
function App() {
  const ready = useStore(s => s.ready);
  const selected = useStore(s => s.selected);
  const dragging = useStore(s => s.dragging);
  const toastMsg = useStore(s => s.toast);
  const failed = useStore(s => s.failed);
  if (failed) return html`<div class="page"><${Note} kind="err">Verbatim n’a pas pu démarrer : ${failed}<//></div>`;
  if (!ready) return null;
  return html`<div class=${'shell' + (dragging ? ' dragging' : '')}>
    <${Top} />
    <${UpdateBar} />
    <${RecBar} />
    <div class="body">
      <${Side} />
      <main class="main">${selected ? html`<${Detail} id=${selected} key=${selected} />` : html`<${Welcome} />`}</main>
    </div>
    <${Drawer} />
    <div class="dropveil">Dépose pour importer</div>
    ${toastMsg ? html`<div class="toast" role="status">${toastMsg}</div>` : null}
  </div>`;
}

// Erreurs de l'interface → journal (20 au plus par session).
let uiErrors = 0;
const logUi = m => { if (uiErrors++ < 20) invoke('log_ui', { message: String(m).slice(0, 300) }).catch(() => {}); };
window.addEventListener('error', e => logUi(`${e.message} (${(e.filename || '').split('/').pop()}:${e.lineno})`));
window.addEventListener('unhandledrejection', e => logUi('promesse : ' + (e.reason && e.reason.message || e.reason)));

async function boot() {
  // Écoute d'abord : la reprise au démarrage peut finir avant le premier chargement.
  await Promise.all([
    listen('item', upsert),
    listen('item-removed', id => set(s => { const items = { ...s.items }; delete items[id]; return { items, selected: s.selected === id ? null : s.selected }; })),
    listen('job', ({ id, job }) => { set(s => s.items[id] ? { items: { ...s.items, [id]: { ...s.items[id], job } } } : {}); if (!job || job.state === 'error') setTimeout(fetchBalance, 2500); }),
    listen('progress', p => set(s => ({ progress: { ...s.progress, [p.id]: p } }))),
    listen('rec-live', ({ id, live }) => set(s => ({ live, recording: s.recording && s.recording.id === id ? s.recording : { id } }))),
    listen('rec-error', ({ message }) => { toast('Enregistrement arrêté : ' + message); set({ recording: null, live: null }); }),
    listen('library-reset', () => loadLibrary()),
    listen('settings-changed', () => refreshState()),
    listen('update-available', u => set({ update: u })),
    listen('update-progress', p => set({ updating: p })),
  ]);
  try {
    await refreshState();
    await loadLibrary();
    fetchBalance();
  } catch (e) { set({ failed: errMsg(e) }); return; }
  // Agenda : au lancement, chaque minute, et quand on revient sur la fenêtre.
  refreshCalendar();
  setInterval(refreshCalendar, 60_000);
  window.addEventListener('focus', () => refreshCalendar());
  try {
    T.webview.getCurrentWebview().onDragDropEvent(ev => {
      const p = ev.payload || {};
      if (p.type === 'enter' || p.type === 'over') set({ dragging: true });
      else if (p.type === 'drop') { set({ dragging: false }); importPaths(p.paths); }
      else set({ dragging: false });
    });
  } catch (_) {}
}

window.__errs = []; window.addEventListener('error', e => window.__errs.push(String(e.message))); window.addEventListener('unhandledrejection', e => window.__errs.push(String(e.reason && e.reason.message || e.reason)));
// Accès pour les tests automatisés.
window.Verbatim = Object.freeze({ importPaths, state: () => S, select: id => set({ selected: id }) });
render(html`<${App} />`, document.getElementById('app'));
boot();
