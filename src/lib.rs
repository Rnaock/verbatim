//! Verbatim 4 : application Tauri.
//!
//! La page ne touche ni au réseau, ni au disque, ni à la clé API : elle appelle
//! les commandes ci-dessous. Enregistrement, file de transcription et
//! bibliothèque vivent ici, en Rust, et survivent à un rechargement de la page.

pub mod audio;
pub mod calendar;
#[cfg(windows)]
pub mod capture_win;
pub mod core;
pub mod library;
pub mod pipeline;
pub mod recorder;
pub mod transcript;
pub mod update;
pub mod vault;

use crate::core::{ApiError, KeyState, KeyStore, Options, Paths, Res, Settings};
use crate::library::{Item, JobInfo, Library, Summary};
use crate::pipeline::Plan;
use crate::recorder::{LiveStatus, Recorder, Role, Source, SystemClock};
use crate::transcript::{Attribution, Transcript};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use tauri_plugin_opener::OpenerExt;

// ---------------------------------------------------------------------------
// Journal : quelques lignes datées (démarrages, périphériques, erreurs, plantages)
// pour comprendre un problème après coup. Jamais de clé ni de texte transcrit.
// ---------------------------------------------------------------------------
static JOURNAL: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Ajoute une ligne au journal (si un journal est ouvert).
pub fn trace(line: &str) {
    let Some(path) = JOURNAL.get() else { return };
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        use std::io::Write;
        let _ = write!(f, "[{now}] {line}\r\n");
    }
}

/// Ouvre le journal et y consigne les plantages (panic) avant l'arrêt du programme.
pub fn open_journal(path: PathBuf) {
    if std::fs::metadata(&path).map(|m| m.len() > 1_000_000).unwrap_or(false) {
        let _ = std::fs::rename(&path, path.with_extension("ancien.txt"));
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = JOURNAL.set(path);
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        trace(&format!("PLANTAGE : {info}"));
        previous(info);
    }));
}

const RETRY_DELAYS: [u64; 3] = [5, 15, 45];

struct ActiveRec {
    id: String,
    recorder: Recorder,
    ticker_stop: Arc<AtomicBool>,
}

pub struct App {
    pub paths: Paths,
    pub api: String,
    pub http: reqwest::Client,
    pub keys: Mutex<KeyStore>,
    settings_lock: Mutex<()>,
    lib: RwLock<Arc<Library>>,
    clock: Arc<SystemClock>,
    rec: Mutex<Option<ActiveRec>>,
    jobs: Mutex<HashMap<String, tauri::async_runtime::JoinHandle<()>>>,
    items_sem: Arc<tokio::sync::Semaphore>,
    requests_sem: Arc<tokio::sync::Semaphore>,
    /// Finalisations et imports en cours (fermeture et changement de dossier).
    busy: Arc<std::sync::atomic::AtomicUsize>,
    /// Dernière version trouvée sur GitHub.
    update: Mutex<Option<update::UpdateInfo>>,
    /// Adresse iCal secrète (déchiffrée en mémoire seulement).
    cal_url: Mutex<Option<String>>,
    /// Agenda lu : instant de lecture et événements.
    cal_cache: Mutex<Option<(std::time::Instant, Arc<Vec<calendar::VEvent>>)>>,
}

/// Compte une opération longue tant qu'elle vit.
struct BusyGuard(Arc<std::sync::atomic::AtomicUsize>);
impl BusyGuard {
    fn new(c: &Arc<std::sync::atomic::AtomicUsize>) -> Self {
        c.fetch_add(1, Ordering::SeqCst);
        BusyGuard(c.clone())
    }
}
impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

type St<'a> = State<'a, Arc<App>>;

impl App {
    fn lib(&self) -> Arc<Library> {
        self.lib.read().unwrap().clone()
    }
    fn settings(&self) -> Settings {
        core::load_settings(&self.paths)
    }
    fn key(&self) -> Option<String> {
        self.keys.lock().unwrap().key().map(String::from)
    }
}

fn api_base() -> String {
    // Redirection vers une fausse API pour les tests : build de développement seulement.
    #[cfg(debug_assertions)]
    if let Ok(v) = std::env::var("VERBATIM_API_BASE") {
        return v;
    }
    core::DEFAULT_API.to_string()
}

fn err<E: std::fmt::Display>(e: E) -> ApiError {
    ApiError::new(500, e.to_string())
}

fn emit_item(app: &AppHandle, lib: &Library, id: &str) {
    if let Ok(item) = lib.load(id) {
        let _ = app.emit("item", lib.summary(&item));
    } else {
        let _ = app.emit("item-removed", id);
    }
}

fn emit_job(app: &AppHandle, id: &str, job: Option<JobInfo>) {
    let _ = app.emit("job", json!({ "id": id, "job": job }));
}

fn emit_progress(app: &AppHandle, id: &str, stage: &str, p: f32) {
    let _ = app.emit("progress", json!({ "id": id, "stage": stage, "progress": p }));
}

// ---------------------------------------------------------------------------
// État, clé, réglages
// ---------------------------------------------------------------------------
#[derive(Serialize)]
struct AppState {
    version: &'static str,
    key: KeyState,
    settings: Settings,
    library: String,
    recording: Option<String>,
    windows: bool,
    simulated_audio: bool,
    jobs: Vec<String>,
    updates: bool,
}

fn simulated_audio() -> bool {
    !cfg!(windows) || (cfg!(debug_assertions) && std::env::var("VERBATIM_FAKE_AUDIO").is_ok())
}

#[tauri::command]
fn app_state(st: St<'_>) -> AppState {
    AppState {
        version: core::VERSION,
        key: st.keys.lock().unwrap().state(),
        settings: st.settings(),
        library: st.lib().root.to_string_lossy().into_owned(),
        recording: st.rec.lock().unwrap().as_ref().map(|r| r.id.clone()),
        windows: cfg!(windows),
        simulated_audio: simulated_audio(),
        jobs: st.jobs.lock().unwrap().keys().cloned().collect(),
        updates: update::REPO.is_some(),
    }
}

#[tauri::command]
fn key_set(st: St<'_>, key: Option<String>, remember: Option<bool>) -> Res<KeyState> {
    st.keys.lock().unwrap().update(&st.paths, key, remember)
}

#[tauri::command]
fn key_clear(st: St<'_>) -> Res<KeyState> {
    st.keys.lock().unwrap().clear(&st.paths)
}

#[tauri::command]
fn settings_set(app: AppHandle, st: St<'_>, patch: Value) -> Res<Settings> {
    let _g = st.settings_lock.lock().unwrap();
    if patch.get("library_dir").is_some()
        && (st.rec.lock().unwrap().is_some() || !st.jobs.lock().unwrap().is_empty() || st.busy.load(Ordering::SeqCst) > 0)
    {
        return Err(ApiError::new(409, "termine d'abord l'enregistrement, l'import ou la transcription en cours"));
    }
    let before = st.settings();
    let s = core::save_settings(&st.paths, &patch)?;
    if st.paths.library(&s) != st.paths.library(&before) {
        let lib = Arc::new(Library::open(&st.paths.library(&s))?);
        let _ = app.asset_protocol_scope().allow_directory(&lib.root, true);
        let _ = lib.migrate_legacy(&s.my_name);
        *st.lib.write().unwrap() = lib;
        let _ = app.emit("library-reset", ());
    }
    Ok(s)
}

#[tauri::command]
async fn balance(st: St<'_>) -> Res<core::Balance> {
    let key = st.key().ok_or(ApiError::new(400, "aucune clé"))?;
    core::balance(&st.http, &st.api, &key).await
}

// ---------------------------------------------------------------------------
// Périphériques et enregistrement
// ---------------------------------------------------------------------------
#[derive(Serialize, Default)]
struct DevicesOut {
    inputs: Vec<(String, String, bool)>,
    outputs: Vec<(String, String, bool)>,
}

#[tauri::command]
async fn devices() -> DevicesOut {
    if simulated_audio() {
        return DevicesOut {
            inputs: vec![("sim-mic".into(), "Micro simulé".into(), true)],
            outputs: vec![("sim-out".into(), "Haut-parleurs simulés".into(), true)],
        };
    }
    #[cfg(windows)]
    {
        let d = tauri::async_runtime::spawn_blocking(capture_win::list_devices).await.unwrap_or_default();
        return DevicesOut {
            inputs: d.inputs.into_iter().map(|x| (x.id, x.name, x.default)).collect(),
            outputs: d.outputs.into_iter().map(|x| (x.id, x.name, x.default)).collect(),
        };
    }
    #[allow(unreachable_code)]
    DevicesOut::default()
}

fn build_source(st: &App, role: Role, s: &Settings) -> Box<dyn Source> {
    if simulated_audio() {
        return Box::new(recorder::SynthSource {
            clock: st.clock.clone(),
            device: if role == Role::Mic { "Micro simulé".into() } else { "Tout le son du PC (simulé)".into() },
            rate: if role == Role::Mic { 48_000 } else { 44_100 },
            amp: 0.25,
            seed: if role == Role::Mic { 11 } else { 23 },
            script: if role == Role::Mic { |t| (t % 6.0) < 3.0 } else { |t| (t % 6.0) >= 3.0 },
            silent_gaps: role == Role::Pc,
            switch_after: None,
        });
    }
    #[cfg(windows)]
    {
        use capture_win::{Target, WasapiSource};
        let target = match role {
            Role::Mic if s.mic_device == "default" => Target::Default,
            Role::Mic => Target::Device(s.mic_device.clone(), s.mic_device_name.clone()),
            Role::Pc if s.pc_device == "all" => Target::AllAudio,
            Role::Pc if s.pc_device == "default" => Target::Default,
            Role::Pc => Target::Device(s.pc_device.clone(), s.pc_device_name.clone()),
        };
        return Box::new(WasapiSource { role, target, clock: st.clock.clone() });
    }
    #[allow(unreachable_code)]
    {
        let _ = s;
        unreachable!()
    }
}

/// Empêche la mise en veille tant que le fil qui l'appelle vit.
fn keep_awake(on: bool) {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::System::Power::{SetThreadExecutionState, ES_CONTINUOUS, ES_SYSTEM_REQUIRED};
        SetThreadExecutionState(if on { ES_CONTINUOUS | ES_SYSTEM_REQUIRED } else { ES_CONTINUOUS });
    }
    #[cfg(not(windows))]
    let _ = on;
}

#[tauri::command]
fn rec_start(app: AppHandle, st: St<'_>, mic: bool, pc: bool, title: Option<String>, event: Option<String>) -> Res<String> {
    if !mic && !pc {
        return Err(ApiError::new(400, "choisis au moins une source"));
    }
    let mut guard = st.rec.lock().unwrap();
    if guard.is_some() {
        return Err(ApiError::new(409, "un enregistrement est déjà en cours"));
    }
    let _ = core::save_settings(&st.paths, &json!({ "rec_mic": mic, "rec_pc": pc }));
    let s = st.settings();
    let lib = st.lib();
    let mut roles = vec![];
    if mic {
        roles.push(Role::Mic);
    }
    if pc {
        roles.push(Role::Pc);
    }
    let ev = pick_event(&st, event.as_deref());
    let title = title
        .filter(|t| !t.trim().is_empty())
        .or_else(|| ev.as_ref().map(|e| e.title.clone()))
        .unwrap_or_else(|| "Enregistrement".into());
    let item = lib.create(&title, library::Kind::Recording, library::State::Recording)?;
    lib.update(&item.id, |i| {
        i.roles = roles.clone();
        i.event = ev.clone();
    })?;
    let sources: Vec<(Role, Box<dyn Source>)> = roles.iter().map(|&r| (r, build_source(&st, r, &s))).collect();
    let recorder = match Recorder::start(&lib.dir(&item.id)?, st.clock.clone(), sources) {
        Ok(r) => r,
        Err(e) => {
            let _ = lib.update(&item.id, |i| {
                i.state = library::State::Failed;
                i.error = Some(format!("Démarrage impossible : {e}"));
            });
            return Err(err(e));
        }
    };
    let ticker_stop = Arc::new(AtomicBool::new(false));
    trace(&format!("enregistrement {} : micro={mic} pc={pc} (micro {:?}, PC {:?})", item.id, s.mic_device, s.pc_device));
    *guard = Some(ActiveRec { id: item.id.clone(), recorder, ticker_stop: ticker_stop.clone() });
    drop(guard);
    emit_item(&app, &lib, &item.id);

    // Fil de suivi : vumètres, noms des périphériques, arrêt sur erreur disque, anti-veille.
    let st2 = st.inner().clone();
    let id = item.id.clone();
    std::thread::spawn(move || {
        keep_awake(true);
        let mut devices: HashMap<String, String> = HashMap::new();
        let mut last_status: HashMap<String, String> = HashMap::new();
        let mut n = 0u64;
        while !ticker_stop.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(100));
            let live: Option<LiveStatus> = st2.rec.lock().unwrap().as_ref().filter(|r| r.id == id).map(|r| r.recorder.live());
            let Some(live) = live else { break };
            let _ = app.emit("rec-live", json!({ "id": id, "live": live }));
            n += 1;
            let mut changed = false;
            for t in &live.tracks {
                let st_line = format!(
                    "{:?}{}{}",
                    t.status.state,
                    t.status.device.as_ref().map(|d| format!(" [{d}]")).unwrap_or_default(),
                    t.status.message.as_ref().map(|m| format!(" ({m})")).unwrap_or_default()
                );
                if last_status.insert(t.role.stem().to_string(), st_line.clone()).as_deref() != Some(st_line.as_str()) {
                    trace(&format!("  {} à {:.1} s : {st_line}", t.role.label(), live.elapsed));
                }
                if let Some(d) = &t.status.device {
                    if devices.insert(t.role.stem().to_string(), d.clone()).as_deref() != Some(d.as_str()) {
                        changed = true;
                    }
                }
            }
            if changed || n % 50 == 0 {
                let devs = devices.clone();
                let el = live.elapsed;
                let _ = st2.lib().update(&id, |i| {
                    i.devices.extend(devs.into_iter());
                    i.duration = Some(el);
                });
            }
            // Garde-fou : un WAV ne peut dépasser ~24 h ; au-delà de 20 h on
            // arrête proprement (ElevenLabs n'accepte de toute façon que 10 h).
            if live.elapsed > 20.0 * 3600.0 {
                trace("arrêt automatique après 20 h");
                let _ = app.emit("rec-error", json!({ "id": id, "message": "Enregistrement arrêté automatiquement après 20 heures." }));
                let _ = stop_recording(&app, &st2);
                break;
            }
            if live.tracks.iter().any(|t| t.error.is_some()) {
                let msg = live.tracks.iter().find_map(|t| t.error.clone()).unwrap_or_default();
                trace(&format!("  arrêt sur erreur d'écriture : {msg}"));
                let _ = app.emit("rec-error", json!({ "id": id, "message": msg }));
                let _ = stop_recording(&app, &st2);
                break;
            }
        }
        keep_awake(false);
    });
    Ok(item.id)
}

#[tauri::command]
fn rec_pause(st: St<'_>, paused: bool) -> Res<()> {
    let g = st.rec.lock().unwrap();
    let r = g.as_ref().ok_or(ApiError::new(404, "aucun enregistrement en cours"))?;
    if paused {
        r.recorder.pause()
    } else {
        r.recorder.resume()
    }
    Ok(())
}

fn stop_recording(app: &AppHandle, st: &Arc<App>) -> Res<String> {
    let active = st.rec.lock().unwrap().take().ok_or(ApiError::new(404, "aucun enregistrement en cours"))?;
    active.ticker_stop.store(true, Ordering::SeqCst);
    let id = active.id.clone();
    let lib = st.lib();
    let _ = lib.update(&id, |i| i.state = library::State::Finalizing);
    emit_item(app, &lib, &id);
    let (app2, st2, id2) = (app.clone(), st.clone(), id.clone());
    std::thread::spawn(move || {
        let lib = st2.lib();
        let stopped = active.recorder.stop();
        if let Err(e) = &stopped {
            let _ = lib.update(&id2, |i| i.error = Some(format!("Arrêt incomplet : {e}")));
        }
        trace(&format!("arrêt {id2} : {stopped:?}"));
        let _busy = BusyGuard::new(&st2.busy);
        let res = pipeline::finalize_item(&lib, &id2, stopped.is_err(), |p| emit_progress(&app2, &id2, "finalize", p));
        trace(&format!("finalisation {id2} : {}", match &res { Ok(_) => "ok".to_string(), Err(e) => e.to_string() }));
        emit_item(&app2, &lib, &id2);
        if res.is_ok() {
            let s = st2.settings();
            if s.auto_transcribe && st2.key().is_some() {
                enqueue(&app2, &st2, &id2, Options::from_settings(&s), s.mode.clone());
            }
        }
    });
    Ok(id)
}

#[tauri::command]
fn rec_stop(app: AppHandle, st: St<'_>) -> Res<String> {
    stop_recording(&app, st.inner())
}

// ---------------------------------------------------------------------------
// Bibliothèque
// ---------------------------------------------------------------------------
#[tauri::command]
async fn library_list(st: St<'_>) -> Res<Vec<Summary>> {
    let lib = st.lib();
    tauri::async_runtime::spawn_blocking(move || lib.list()).await.map_err(err)
}

#[tauri::command]
async fn library_search(st: St<'_>, query: String) -> Res<Vec<String>> {
    let lib = st.lib();
    tauri::async_runtime::spawn_blocking(move || lib.search(&query)).await.map_err(err)
}

#[derive(Serialize)]
struct Detail {
    item: Item,
    summary: Summary,
    transcript: Option<Transcript>,
    envelopes: Option<library::Envelopes>,
    playback: Option<String>,
    plan: Option<Plan>,
}

#[tauri::command]
async fn item_get(app: AppHandle, st: St<'_>, id: String) -> Res<Detail> {
    let lib = st.lib();
    let mode = st.settings().mode;
    tauri::async_runtime::spawn_blocking(move || {
        let item = lib.load(&id)?;
        let playback = library::playback_path(&lib, &item);
        if let Some(p) = &playback {
            if !p.starts_with(&lib.root) {
                let _ = app.asset_protocol_scope().allow_file(p);
            }
        }
        Ok(Detail {
            summary: lib.summary(&item),
            transcript: lib.current_transcript(&item),
            envelopes: lib.envelopes(&id),
            playback: playback.map(|p| p.to_string_lossy().into_owned()),
            plan: pipeline::plan(&item, &mode).ok(),
            item,
        })
    })
    .await
    .map_err(err)?
}

#[tauri::command]
fn item_rename(app: AppHandle, st: St<'_>, id: String, title: String) -> Res<()> {
    let t: String = title.trim().chars().filter(|c| !c.is_control()).take(120).collect();
    if t.is_empty() {
        return Err(ApiError::new(400, "titre vide"));
    }
    let lib = st.lib();
    let item = lib.update(&id, |i| i.title = t)?;
    if let (Some(rel), Some(tr)) = (item.current.clone(), lib.current_transcript(&item)) {
        let _ = lib.save_transcript(&item, &rel, &tr);
    }
    emit_item(&app, &lib, &id);
    Ok(())
}

#[tauri::command]
fn item_trash(app: AppHandle, st: St<'_>, id: String) -> Res<()> {
    if st.rec.lock().unwrap().as_ref().map(|r| r.id == id).unwrap_or(false) {
        return Err(ApiError::new(409, "enregistrement en cours"));
    }
    if st.jobs.lock().unwrap().contains_key(&id) {
        return Err(ApiError::new(409, "transcription en cours"));
    }
    let lib = st.lib();
    lib.trash(&id)?;
    let _ = app.emit("item-removed", &id);
    Ok(())
}

fn open_path(p: &Path) -> Res<()> {
    #[cfg(windows)]
    std::process::Command::new("explorer").arg(p).spawn().map_err(err)?;
    #[cfg(not(windows))]
    let _ = std::process::Command::new("xdg-open").arg(p).spawn();
    Ok(())
}

#[tauri::command]
fn open_folder(st: St<'_>, id: Option<String>) -> Res<()> {
    let lib = st.lib();
    match id {
        Some(id) => open_path(&lib.dir(&id)?),
        None => open_path(&lib.root),
    }
}

#[tauri::command]
fn open_url(app: AppHandle, url: String) -> Res<()> {
    let allowed = url.starts_with("https://elevenlabs.io/") || calendar::is_meeting_url(&url);
    if !allowed || url.chars().any(|c| c.is_whitespace() || c == '"') {
        return Err(ApiError::new(400, "adresse refusée"));
    }
    app.opener().open_url(url, None::<&str>).map_err(err)
}

// ---------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------
#[tauri::command]
async fn pick_files(app: AppHandle) -> Res<Vec<String>> {
    let mut exts: Vec<&str> = pipeline::AUDIO.to_vec();
    exts.extend_from_slice(pipeline::VIDEO);
    let picked = tauri::async_runtime::spawn_blocking(move || {
        app.dialog()
            .file()
            .set_title("Importer des enregistrements")
            .add_filter("Audio ou vidéo", &exts)
            .set_directory(dirs::download_dir().unwrap_or_else(|| PathBuf::from(".")))
            .blocking_pick_files()
    })
    .await
    .map_err(err)?;
    Ok(picked.unwrap_or_default().into_iter().filter_map(|f| f.into_path().ok()).map(|p| p.to_string_lossy().into_owned()).collect())
}

// ---------------------------------------------------------------------------
// Agenda
// ---------------------------------------------------------------------------
const CAL_REFRESH: Duration = Duration::from_secs(5 * 60);

#[tauri::command]
fn calendar_state(st: St<'_>) -> calendar::CalState {
    calendar::state(st.cal_url.lock().unwrap().as_deref())
}

/// Lit l'agenda si la copie a plus de 5 minutes (ou sur demande).
async fn calendar_events(st: &App, force: bool) -> Res<Arc<Vec<calendar::VEvent>>> {
    let url = st.cal_url.lock().unwrap().clone().ok_or(ApiError::new(404, "aucun agenda connecté"))?;
    if !force {
        if let Some((at, ev)) = st.cal_cache.lock().unwrap().as_ref() {
            if at.elapsed() < CAL_REFRESH {
                return Ok(ev.clone());
            }
        }
    }
    match calendar::fetch(&st.http, &url).await {
        Ok(text) => {
            let ev = Arc::new(calendar::parse(&text));
            *st.cal_cache.lock().unwrap() = Some((std::time::Instant::now(), ev.clone()));
            Ok(ev)
        }
        // Hors ligne : la dernière lecture reste utilisable.
        Err(e) => match st.cal_cache.lock().unwrap().as_ref() {
            Some((_, ev)) if !force => Ok(ev.clone()),
            _ => Err(e),
        },
    }
}

/// Ta propre adresse s'affiche avec ton prénom (réglage « Toi »).
fn with_my_name(mut list: Vec<calendar::CalEvent>, my_name: &str) -> Vec<calendar::CalEvent> {
    for e in &mut list {
        for p in e.people.iter_mut().filter(|p| p.me) {
            p.name = my_name.to_string();
            p.guessed = false;
        }
    }
    list
}

fn me_of(st: &App) -> Option<String> {
    st.cal_url.lock().unwrap().as_deref().and_then(calendar::account_of)
}

#[tauri::command]
async fn calendar_set(st: St<'_>, url: Option<String>) -> Res<calendar::CalState> {
    match url.filter(|u| !u.trim().is_empty()) {
        None => {
            calendar::save_url(&st.paths, None)?;
            *st.cal_url.lock().unwrap() = None;
            *st.cal_cache.lock().unwrap() = None;
        }
        Some(u) => {
            let u = calendar::validate_url(&u, cfg!(debug_assertions)).map_err(|e| ApiError::new(400, e))?;
            // On vérifie que l'adresse répond avant de l'enregistrer.
            let text = calendar::fetch(&st.http, &u).await?;
            calendar::save_url(&st.paths, Some(&u))?;
            *st.cal_url.lock().unwrap() = Some(u);
            *st.cal_cache.lock().unwrap() = Some((std::time::Instant::now(), Arc::new(calendar::parse(&text))));
            trace("agenda connecté");
        }
    }
    Ok(calendar::state(st.cal_url.lock().unwrap().as_deref()))
}

#[derive(Serialize)]
struct CalNow {
    connected: bool,
    /// En cours ou dans les 10 minutes.
    now: Vec<calendar::CalEvent>,
    /// Prochain rendez-vous de la journée, au-delà.
    next: Option<calendar::CalEvent>,
    error: Option<String>,
}

#[tauri::command]
async fn calendar_now(st: St<'_>, force: Option<bool>) -> Res<CalNow> {
    if st.cal_url.lock().unwrap().is_none() {
        return Ok(CalNow { connected: false, now: vec![], next: None, error: None });
    }
    let ev = match calendar_events(&st, force.unwrap_or(false)).await {
        Ok(ev) => ev,
        Err(e) => return Ok(CalNow { connected: true, now: vec![], next: None, error: Some(e.message) }),
    };
    let now = chrono::Utc::now();
    let end_of_day = chrono::Local::now().date_naive().and_hms_opt(23, 59, 59).and_then(|d| d.and_local_timezone(chrono::Local).earliest()).map(|d| d.with_timezone(&chrono::Utc)).unwrap_or(now + chrono::Duration::hours(12));
    let people = st.settings().people;
    let list = with_my_name(calendar::events_between(&ev, now - chrono::Duration::hours(1), end_of_day.max(now + chrono::Duration::hours(1)), me_of(&st).as_deref(), &people), &st.settings().my_name);
    let current = calendar::current(&list, now);
    let next = list.iter().find(|e| e.start > now.timestamp() + calendar::LEAD_MIN * 60).cloned();
    Ok(CalNow { connected: true, now: current, next, error: None })
}

/// Rendez-vous proches d'un instant (pour lier une conversation après coup).
#[tauri::command]
async fn calendar_around(st: St<'_>, ts: i64) -> Res<Vec<calendar::CalEvent>> {
    let ev = calendar_events(&st, false).await?;
    let at = chrono::DateTime::from_timestamp(ts, 0).unwrap_or_else(chrono::Utc::now);
    let people = st.settings().people;
    Ok(with_my_name(calendar::events_between(&ev, at - chrono::Duration::hours(2), at + chrono::Duration::hours(2), me_of(&st).as_deref(), &people), &st.settings().my_name))
}

/// Événement à associer à un nouvel enregistrement : celui demandé, sinon
/// l'unique rendez-vous en cours (s'il y en a plusieurs, on ne devine pas).
/// Utilise la dernière lecture de l'agenda : démarrer ne doit jamais attendre le réseau.
fn pick_event(st: &App, wanted: Option<&str>) -> Option<calendar::CalEvent> {
    if wanted == Some("") || st.cal_url.lock().unwrap().is_none() {
        return None;
    }
    let ev = st.cal_cache.lock().unwrap().as_ref().map(|(_, e)| e.clone())?;
    let now = chrono::Utc::now();
    let people = st.settings().people;
    let list = with_my_name(calendar::events_between(&ev, now - chrono::Duration::hours(3), now + chrono::Duration::hours(3), me_of(st).as_deref(), &people), &st.settings().my_name);
    match wanted {
        Some(uid) => list.into_iter().find(|e| e.uid == uid),
        None => {
            let cur = calendar::current(&list, now);
            (cur.len() == 1).then(|| cur.into_iter().next()).flatten()
        }
    }
}

#[tauri::command]
async fn item_link_event(app: AppHandle, st: St<'_>, id: String, event: Option<calendar::CalEvent>) -> Res<()> {
    let lib = st.lib();
    let item = lib.load(&id)?;
    let default_title = item.event.as_ref().map(|e| e.title == item.title).unwrap_or(false) || item.title.starts_with("Enregistrement");
    let item = lib.update(&id, |i| {
        if let (Some(e), true) = (&event, default_title) {
            i.title = e.title.clone();
        }
        i.event = event.clone();
    })?;
    // Noms des participants proposés tout de suite, sans repayer.
    if let (Some(rel), Some(mut t), Some(ev)) = (item.current.clone(), lib.current_transcript(&item), item.event.clone()) {
        let before = t.speakers.clone();
        pipeline::name_from_event(&mut t, &ev, &st.settings().my_name);
        if t.speakers != before {
            lib.save_transcript(&item, &rel, &t)?;
        }
    }
    emit_item(&app, &lib, &id);
    Ok(())
}

/// Corrige le nom d'une personne : annuaire, rendez-vous de la conversation, et
/// locuteur qui portait l'ancien nom.
#[tauri::command]
fn person_rename(app: AppHandle, st: St<'_>, id: Option<String>, email: String, name: String) -> Res<()> {
    let email = email.trim().to_lowercase();
    let name: String = name.trim().chars().filter(|c| !c.is_control()).take(40).collect();
    if !email.contains('@') {
        return Err(ApiError::new(400, "adresse invalide"));
    }
    {
        let _g = st.settings_lock.lock().unwrap();
        let mut people = st.settings().people;
        if name.is_empty() {
            people.remove(&email);
        } else {
            people.insert(email.clone(), name.clone());
        }
        core::save_settings(&st.paths, &json!({ "people": people }))?;
    }
    let Some(id) = id else { return Ok(()) };
    if name.is_empty() {
        return Ok(());
    }
    let lib = st.lib();
    let mut old = None;
    let item = lib.update(&id, |i| {
        if let Some(ev) = i.event.as_mut() {
            for p in ev.people.iter_mut().filter(|p| p.email == email) {
                old = Some(std::mem::replace(&mut p.name, name.clone()));
                p.guessed = false;
            }
        }
    })?;
    if let (Some(old), Some(rel), Some(mut t)) = (old, item.current.clone(), lib.current_transcript(&item)) {
        let mut changed = false;
        for s in t.speakers.iter_mut().filter(|s| s.name == old) {
            s.name = name.clone();
            changed = true;
        }
        if changed {
            lib.save_transcript(&item, &rel, &t)?;
        }
    }
    emit_item(&app, &lib, &id);
    Ok(())
}

// ---------------------------------------------------------------------------
// Mises à jour
// ---------------------------------------------------------------------------
fn ensure_idle(st: &App) -> Res<()> {
    if st.rec.lock().unwrap().is_some() || !st.jobs.lock().unwrap().is_empty() || st.busy.load(Ordering::SeqCst) > 0 {
        return Err(ApiError::new(409, "termine d'abord l'enregistrement, l'import ou la transcription en cours"));
    }
    Ok(())
}

#[tauri::command]
async fn update_check(st: St<'_>) -> Res<Option<update::UpdateInfo>> {
    let found = update::check(&st.http).await?;
    *st.update.lock().unwrap() = found.clone();
    Ok(found)
}

/// Remplace l'exécutable par `candidate` (déjà vérifié), relance et quitte.
fn install_and_restart(app: &AppHandle, st: &App, candidate: &Path) -> Res<()> {
    let version = update::probe(candidate).map_err(|e| ApiError::new(400, e))?;
    ensure_idle(st)?;
    let exe = update::swap(candidate).map_err(|e| ApiError::new(500, e))?;
    trace(&format!("mise à jour vers {version} installée"));
    update::relaunch(&exe).map_err(|e| ApiError::new(500, format!("{e}. Relance Verbatim à la main : la nouvelle version est en place.")))?;
    app.exit(0);
    Ok(())
}

#[tauri::command]
async fn update_install(app: AppHandle, st: St<'_>) -> Res<()> {
    ensure_idle(&st)?;
    let info = st.update.lock().unwrap().clone().ok_or(ApiError::new(404, "aucune mise à jour à installer"))?;
    let app2 = app.clone();
    let mut last = -1.0f32;
    let path = update::download(&st.http, &st.paths.data_dir, &info, |p| {
        if p - last >= 0.02 || p >= 1.0 {
            last = p;
            let _ = app2.emit("update-progress", p);
        }
    })
    .await?;
    let (app3, st3) = (app.clone(), st.inner().clone());
    tauri::async_runtime::spawn_blocking(move || install_and_restart(&app3, &st3, &path)).await.map_err(err)?
}

#[tauri::command]
async fn update_install_file(app: AppHandle, st: St<'_>, path: Option<String>) -> Res<bool> {
    ensure_idle(&st)?;
    let path = match path {
        Some(p) => PathBuf::from(p),
        None => {
            let app2 = app.clone();
            let picked = tauri::async_runtime::spawn_blocking(move || {
                app2.dialog()
                    .file()
                    .set_title("Installer une version de Verbatim")
                    .add_filter("Verbatim (.exe ou .zip)", &["exe", "zip"])
                    .set_directory(dirs::download_dir().unwrap_or_else(|| PathBuf::from(".")))
                    .blocking_pick_file()
            })
            .await
            .map_err(err)?;
            match picked.and_then(|f| f.into_path().ok()) {
                Some(p) => p,
                None => return Ok(false),
            }
        }
    };
    let (app2, st2) = (app.clone(), st.inner().clone());
    tauri::async_runtime::spawn_blocking(move || {
        let candidate = update::stage_file(&st2.paths.data_dir, &path).map_err(|e| ApiError::new(400, e))?;
        install_and_restart(&app2, &st2, &candidate).map(|_| true)
    })
    .await
    .map_err(err)?
}

#[derive(Serialize)]
struct ImportOut {
    ids: Vec<String>,
    rejected: Vec<String>,
}

#[tauri::command]
fn import_paths(app: AppHandle, st: St<'_>, paths: Vec<String>) -> Res<ImportOut> {
    let lib = st.lib();
    let mut out = ImportOut { ids: vec![], rejected: vec![] };
    for p in paths.iter().take(200) {
        let path = PathBuf::from(p);
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.clone());
        if !path.is_file() || !pipeline::is_media(&path) || path.starts_with(&lib.root) {
            out.rejected.push(name);
            continue;
        }
        match pipeline::import_create(&lib, &path) {
            Ok(item) => {
                emit_item(&app, &lib, &item.id);
                out.ids.push(item.id.clone());
                let (app2, lib2, id) = (app.clone(), lib.clone(), item.id.clone());
                let st2 = st.inner().clone();
                std::thread::spawn(move || {
                    let _busy = BusyGuard::new(&st2.busy);
                    let res = pipeline::import_fill(&lib2, &id, |p| emit_progress(&app2, &id, "import", p));
                    emit_item(&app2, &lib2, &id);
                    let s = st2.settings();
                    if res.is_ok() && s.auto_transcribe && st2.key().is_some() {
                        enqueue(&app2, &st2, &id, Options::from_settings(&s), s.mode.clone());
                    }
                });
            }
            Err(_) => out.rejected.push(name),
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Transcription
// ---------------------------------------------------------------------------
#[derive(Deserialize, Clone, Debug)]
struct TranscribeReq {
    lang: String,
    speakers: String,
    no_verbatim: bool,
    #[serde(default)]
    keyterms: Vec<String>,
    mode: String,
}

#[tauri::command]
fn estimate(st: St<'_>, id: String, mode: String) -> Res<Plan> {
    let item = st.lib().load(&id)?;
    pipeline::plan(&item, &mode).map_err(|e| ApiError::new(400, e.to_string()))
}

#[tauri::command]
fn transcribe(app: AppHandle, st: St<'_>, id: String, req: Option<TranscribeReq>) -> Res<()> {
    if st.key().is_none() {
        return Err(ApiError::new(400, "aucune clé API configurée"));
    }
    let s = st.settings();
    let (opts, mode) = match req {
        Some(r) => (Options { lang: r.lang, speakers: r.speakers, no_verbatim: r.no_verbatim, keyterms: r.keyterms }, r.mode),
        None => (Options::from_settings(&s), s.mode.clone()),
    };
    let item = st.lib().load(&id)?;
    pipeline::plan(&item, &mode).map_err(|e| ApiError::new(400, e.to_string()))?;
    if st.jobs.lock().unwrap().contains_key(&id) {
        return Err(ApiError::new(409, "transcription déjà en cours"));
    }
    enqueue(&app, st.inner(), &id, opts, mode);
    Ok(())
}

#[tauri::command]
fn transcribe_cancel(app: AppHandle, st: St<'_>, id: String) -> Res<()> {
    if let Some(h) = st.jobs.lock().unwrap().remove(&id) {
        h.abort();
    }
    let lib = st.lib();
    let _ = lib.update(&id, |i| i.job = None);
    pipeline::clear_cache(&lib, &id);
    emit_job(&app, &id, None);
    emit_item(&app, &lib, &id);
    Ok(())
}

fn enqueue(app: &AppHandle, st: &Arc<App>, id: &str, opts: Options, mode: String) {
    let lib = st.lib();
    if let Ok(item) = pipeline::set_job(&lib, id, "queued", None, None) {
        emit_job(app, id, item.job);
    }
    let (app2, st2, id2) = (app.clone(), st.clone(), id.to_string());
    let handle = tauri::async_runtime::spawn(async move {
        let res = run_job(&app2, &st2, &id2, &opts, &mode).await;
        st2.jobs.lock().unwrap().remove(&id2);
        let lib = st2.lib();
        trace(&format!("transcription {id2} : {}", match &res { Ok(()) => "ok".to_string(), Err(e) => format!("erreur {} {}", e.status, e.message) }));
        match res {
            Ok(()) => emit_job(&app2, &id2, None),
            Err(e) => {
                let msg = friendly(&e);
                if let Ok(item) = pipeline::set_job(&lib, &id2, "error", Some(msg), None) {
                    emit_job(&app2, &id2, item.job);
                }
            }
        }
        emit_item(&app2, &lib, &id2);
    });
    st.jobs.lock().unwrap().insert(id.to_string(), handle);
}

fn friendly(e: &ApiError) -> String {
    let d = e.message.as_str();
    let low = d.to_lowercase();
    match e.status {
        0 => format!("Connexion à ElevenLabs impossible. Vérifie ta connexion internet. ({d})"),
        401 => format!("Clé refusée par ElevenLabs. ({d})"),
        _ if low.contains("quota") || low.contains("credit") => format!("Crédits insuffisants sur le compte ElevenLabs. ({d})"),
        422 => format!("Paramètres refusés par ElevenLabs : {d}"),
        429 => format!("ElevenLabs est saturé, réessaie dans un moment. ({d})"),
        _ => d.to_string(),
    }
}

async fn run_job(app: &AppHandle, st: &Arc<App>, id: &str, opts: &Options, mode: &str) -> Res<()> {
    let _permit = st.items_sem.clone().acquire_owned().await.map_err(err)?;
    let key = st.key().ok_or(ApiError::new(400, "aucune clé API configurée"))?;
    let lib = st.lib();
    let set = |state: &str, msg: Option<String>, p: Option<f32>| {
        if let Ok(item) = pipeline::set_job(&lib, id, state, msg, p) {
            emit_job(app, id, item.job);
        }
    };
    set("preparing", None, Some(0.0));
    let item = lib.load(id)?;
    let plan = pipeline::plan(&item, mode).map_err(|e| ApiError::new(400, e.to_string()))?;
    let calibrate_from = if opts.keyterms.iter().all(|k| k.trim().is_empty()) && st.jobs.lock().unwrap().len() <= 1 {
        core::balance(&st.http, &st.api, &key).await.ok().and_then(|b| b.count)
    } else {
        None
    };

    // 1. Fichiers à envoyer.
    let (lib2, item2, plan2, app2, id2, st2) = (lib.clone(), item.clone(), plan.clone(), app.clone(), id.to_string(), st.clone());
    let files = tauri::async_runtime::spawn_blocking(move || {
        let mut last = -1.0f32;
        pipeline::prepare(&lib2, &item2, &plan2, &mut |p| {
            if p - last >= 0.02 && st2.jobs.lock().unwrap().contains_key(&id2) {
                last = p;
                if let Ok(it) = pipeline::set_job(&lib2, &id2, "preparing", None, Some(p)) {
                    emit_job(&app2, &id2, it.job);
                }
            }
        })
    })
    .await
    .map_err(err)??;

    // 2. Envois, en parallèle, avec relances.
    let sizes: Vec<u64> = files.iter().map(|f| std::fs::metadata(f).map(|m| m.len()).unwrap_or(1)).collect();
    let total: u64 = sizes.iter().sum::<u64>().max(1);
    let sent = Arc::new(Mutex::new(vec![0u64; files.len()]));
    let last_pct = Arc::new(Mutex::new(-1i32));
    set("uploading", None, Some(0.0));
    // JoinSet : si la tâche est annulée ou qu'un envoi échoue, les autres envois
    // sont interrompus aussitôt (pas de facturation inutile, pas d'état fantôme).
    let mut tasks = tokio::task::JoinSet::new();
    for (k, (path, part)) in files.iter().zip(plan.parts.iter()).enumerate() {
        let (st, key, opts, path, part) = (st.clone(), key.clone(), opts.clone(), path.clone(), *part);
        let (sent, last_pct, lib, app, id) = (sent.clone(), last_pct.clone(), lib.clone(), app.clone(), id.to_string());
        let size = sizes[k];
        tasks.spawn(async move {
            let mut attempt = 0;
            loop {
                let _req = st.requests_sem.clone().acquire_owned().await.map_err(err)?;
                let (sent2, last2, lib2, app2, id2, st2) = (sent.clone(), last_pct.clone(), lib.clone(), app.clone(), id.clone(), st.clone());
                let progress = move |s: u64, _t: u64| {
                    if !st2.jobs.lock().unwrap().contains_key(&id2) {
                        return; // annulée entre-temps
                    }
                    let mut v = sent2.lock().unwrap();
                    v[k] = s;
                    let pct = (v.iter().sum::<u64>() * 100 / total) as i32;
                    let mut l = last2.lock().unwrap();
                    if pct != *l {
                        *l = pct;
                        let state = if pct >= 100 { "processing" } else { "uploading" };
                        if let Ok(it) = pipeline::set_job(&lib2, &id2, state, None, Some(pct as f32 / 100.0)) {
                            emit_job(&app2, &id2, it.job);
                        }
                    }
                };
                match core::transcribe_file(&st.http, &st.api, &key, &path, &opts, progress).await {
                    Ok(v) => return Ok((k, part, v)),
                    // On ne relance que si ElevenLabs n'a pas pu traiter le fichier :
                    // refus explicite (429, 503) ou coupure avant la fin de l'envoi.
                    // Une coupure après l'envoi complet pourrait être facturée deux fois.
                    Err(e)
                        if attempt < RETRY_DELAYS.len()
                            && ([429, 503].contains(&e.status)
                                || ([0, 500, 502, 504].contains(&e.status) && sent.lock().unwrap()[k] < size)) =>
                    {
                        drop(_req);
                        let d = RETRY_DELAYS[attempt];
                        attempt += 1;
                        if let Ok(it) = pipeline::set_job(&lib, &id, "uploading", Some(format!("{} Nouvel essai dans {d} s.", friendly(&e))), None) {
                            emit_job(&app, &id, it.job);
                        }
                        tokio::time::sleep(Duration::from_secs(d)).await;
                        sent.lock().unwrap()[k] = 0;
                    }
                    Err(e) if e.status == 0 && sent.lock().unwrap()[k] >= size => {
                        return Err(ApiError::new(
                            0,
                            format!("connexion coupée pendant le traitement par ElevenLabs ({}). Vérifie ton solde avant de relancer.", e.message),
                        ))
                    }
                    Err(e) => return Err::<(usize, Option<Role>, Value), ApiError>(e),
                }
            }
        });
    }
    let mut got = vec![];
    while let Some(r) = tasks.join_next().await {
        got.push(r.map_err(err)??);
    }
    got.sort_by_key(|x| x.0);
    let responses: Vec<(Option<Role>, Value)> = got.into_iter().map(|(_, p, v)| (p, v)).collect();

    // 3. Assemblage et sauvegarde.
    let s = st.settings();
    let (lib2, id2, plan2, opts2) = (lib.clone(), id.to_string(), plan.clone(), opts.clone());
    tauri::async_runtime::spawn_blocking(move || pipeline::finish(&lib2, &id2, &plan2, &responses, &opts2, &s.my_name))
        .await
        .map_err(err)??;

    // 4. Calibrage du coût réel (sans vocabulaire, une seule tâche à la fois).
    if let Some(before) = calibrate_from {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        if let Ok(b) = core::balance(&st.http, &st.api, &key).await {
            let minutes = plan.billed_seconds() / 60.0;
            if let Some(after) = b.count {
                let rate = (after - before) / minutes;
                if minutes >= 1.0 && (50.0..=3000.0).contains(&rate) && st.jobs.lock().unwrap().len() <= 1 {
                    let prev = st.settings().credits_per_min;
                    let next = if (prev - core::DEFAULT_CREDITS).abs() < 0.5 { rate } else { (prev + rate) / 2.0 };
                    let _ = core::save_settings(&st.paths, &json!({ "credits_per_min": next.round() }));
                    let _ = app.emit("settings-changed", ());
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Corrections et exports
// ---------------------------------------------------------------------------
#[tauri::command]
fn attribution_save(app: AppHandle, st: St<'_>, id: String, attribution: Attribution) -> Res<Transcript> {
    let lib = st.lib();
    let item = lib.load(&id)?;
    let rel = item.current.clone().ok_or(ApiError::new(404, "aucune transcription"))?;
    let mut t = lib.current_transcript(&item).ok_or(ApiError::new(404, "transcription illisible"))?;
    t.apply(&attribution).map_err(|e| ApiError::new(400, e))?;
    lib.save_transcript(&item, &rel, &t)?;
    let names: Vec<Value> = t.speakers.iter().map(|s| json!(s.name)).collect();
    let s = st.settings();
    let mut known: Vec<Value> = names.into_iter().filter(|n| !n.as_str().unwrap_or("").starts_with("Locuteur")).collect();
    known.extend(s.names.iter().map(|n| json!(n)));
    let mut seen = std::collections::HashSet::new();
    known.retain(|n| seen.insert(n.as_str().unwrap_or("").to_lowercase()));
    let _ = core::save_settings(&st.paths, &json!({ "names": known }));
    emit_item(&app, &lib, &id);
    Ok(t)
}

#[tauri::command]
fn transcript_reset(app: AppHandle, st: St<'_>, id: String) -> Res<Transcript> {
    let lib = st.lib();
    let t = pipeline::rebuild(&lib, &id, &st.settings().my_name)?;
    emit_item(&app, &lib, &id);
    Ok(t)
}

#[tauri::command]
async fn export(app: AppHandle, st: St<'_>, id: String, format: String) -> Res<Option<String>> {
    let lib = st.lib();
    let item = lib.load(&id)?;
    let t = lib.current_transcript(&item).ok_or(ApiError::new(404, "aucune transcription"))?;
    let (text, ext, label) = match format.as_str() {
        "md" => (transcript::export_md(&t, &item.title, item.duration), "md", "Markdown"),
        "srt" => (transcript::export_srt(&t), "srt", "Sous-titres"),
        _ => (transcript::export_txt(&t, &item.title, item.duration), "txt", "Texte"),
    };
    let name = core::safe_file_name(&item.title, ext);
    let dir = lib.dir(&id)?;
    let target = tauri::async_runtime::spawn_blocking(move || {
        app.dialog().file().set_title("Exporter").set_directory(dir).set_file_name(name).add_filter(label, &[ext]).blocking_save_file()
    })
    .await
    .map_err(err)?;
    let Some(path) = target.and_then(|f| f.into_path().ok()) else { return Ok(None) };
    core::write_text_file(&path, &text)?;
    Ok(Some(path.to_string_lossy().into_owned()))
}

#[tauri::command]
async fn pick_library_dir(app: AppHandle, st: St<'_>) -> Res<Option<String>> {
    let cur = st.lib().root.clone();
    let p = tauri::async_runtime::spawn_blocking(move || app.dialog().file().set_title("Dossier de la bibliothèque").set_directory(cur).blocking_pick_folder())
        .await
        .map_err(err)?;
    Ok(p.and_then(|f| f.into_path().ok()).map(|p| p.to_string_lossy().into_owned()))
}

// ---------------------------------------------------------------------------
// Lancement
// ---------------------------------------------------------------------------
pub fn run() {
    let paths = Paths::system();
    open_journal(paths.data_dir.join("journal.txt"));
    trace(&format!("démarrage de Verbatim {}", core::VERSION));
    let settings = core::load_settings(&paths);
    let lib = Arc::new(Library::open(&paths.library(&settings)).or_else(|_| Library::open(&paths.out_dir)).expect("bibliothèque inaccessible"));
    let cal_url = calendar::load_url(&paths);
    let state = Arc::new(App {
        keys: Mutex::new(KeyStore::load(&paths)),
        paths,
        api: api_base(),
        http: core::http_client(),
        settings_lock: Mutex::new(()),
        lib: RwLock::new(lib),
        clock: Arc::new(SystemClock::new()),
        rec: Mutex::new(None),
        jobs: Mutex::new(HashMap::new()),
        items_sem: Arc::new(tokio::sync::Semaphore::new(2)),
        requests_sem: Arc::new(tokio::sync::Semaphore::new(2)),
        busy: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        update: Mutex::new(None),
        cal_url: Mutex::new(cal_url),
        cal_cache: Mutex::new(None),
    });

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.unminimize();
                let _ = w.show();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(state.clone())
        .setup(move |app| {
            let st = state.clone();
            let lib = st.lib();
            let _ = app.asset_protocol_scope().allow_directory(&lib.root, true);
            let handle = app.handle().clone();
            update::cleanup(&st.paths.data_dir);
            if update::REPO.is_some() {
                let (st_u, h_u) = (st.clone(), handle.clone());
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(4)).await;
                    if let Ok(Some(info)) = update::check(&st_u.http).await {
                        *st_u.update.lock().unwrap() = Some(info.clone());
                        let _ = h_u.emit("update-available", info);
                    }
                });
            }
            // Reprise : anciennes transcriptions, enregistrements interrompus.
            std::thread::spawn(move || {
                let s = st.settings();
                if lib.migrate_legacy(&s.my_name) > 0 {
                    let _ = handle.emit("library-reset", ());
                }
                for id in lib.startup_scan() {
                    let _busy = BusyGuard::new(&st.busy);
                    trace(&format!("reprise de {id}"));
                    let _ = pipeline::finalize_item(&lib, &id, true, |p| emit_progress(&handle, &id, "finalize", p));
                    emit_item(&handle, &lib, &id);
                }
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let app = window.app_handle().clone();
                let st = app.state::<Arc<App>>().inner().clone();
                let recording = st.rec.lock().unwrap().is_some();
                let jobs = !st.jobs.lock().unwrap().is_empty();
                let busy = st.busy.load(Ordering::SeqCst) > 0;
                if !recording && !jobs && !busy {
                    return;
                }
                api.prevent_close();
                let msg = if recording {
                    "Un enregistrement est en cours. Si tu fermes, il est arrêté et conservé : Verbatim le finalisera au prochain lancement."
                } else if busy && !jobs {
                    "Verbatim termine l'enregistrement d'une conversation ou un import. Si tu fermes, l'audio est conservé et le travail reprendra au prochain lancement (un import devra être relancé)."
                } else {
                    "Une transcription est en cours. Si tu fermes maintenant, elle sera perdue et peut-être facturée quand même."
                };
                let w = window.clone();
                app.dialog()
                    .message(msg)
                    .title("Verbatim")
                    .kind(MessageDialogKind::Warning)
                    .buttons(MessageDialogButtons::OkCancelCustom("Fermer quand même".into(), "Continuer".into()))
                    .show(move |quit| {
                        if quit {
                            if let Some(active) = st.rec.lock().unwrap().take() {
                                active.ticker_stop.store(true, Ordering::SeqCst);
                                let _ = active.recorder.stop();
                            }
                            for (_, h) in st.jobs.lock().unwrap().drain() {
                                h.abort();
                            }
                            let _ = w.destroy();
                        }
                    });
            }
        })
        .invoke_handler(tauri::generate_handler![
            app_state,
            key_set,
            key_clear,
            settings_set,
            balance,
            devices,
            rec_start,
            rec_pause,
            rec_stop,
            library_list,
            library_search,
            item_get,
            item_rename,
            item_trash,
            open_folder,
            open_url,
            pick_files,
            import_paths,
            estimate,
            transcribe,
            transcribe_cancel,
            attribution_save,
            transcript_reset,
            export,
            pick_library_dir,
            calendar_state,
            calendar_set,
            calendar_now,
            calendar_around,
            item_link_event,
            person_rename,
            update_check,
            update_install,
            update_install_file
        ])
        .run(tauri::generate_context!())
        .expect("échec du lancement de Verbatim");
}

/// `Verbatim.exe --install-update fichier rapport.txt` : installe une version
/// (exe ou zip) sans ouvrir la fenêtre. Sert aux tests et en dépannage.
pub fn cli_install(file: &str, report: &str) -> bool {
    let p = Paths::system();
    let res = update::stage_file(&p.data_dir, Path::new(file))
        .and_then(|c| update::probe(&c).map(|v| (c, v)))
        .and_then(|(c, v)| update::swap(&c).map(|_| v));
    let _ = std::fs::write(report, match &res {
        Ok(v) => format!("OK : Verbatim {v} installé. Relance Verbatim."),
        Err(e) => format!("ÉCHEC : {e}"),
    });
    res.is_ok()
}

/// `Verbatim.exe --selftest rapport.txt` : chiffrement, dossiers, audio, sans fenêtre.
/// `Verbatim.exe --audio-test rapport.txt` : capture réelle de quelques secondes.
pub fn selftest(report: &str, audio_test: bool) -> bool {
    let _ = std::fs::remove_file(report);
    open_journal(PathBuf::from(report));
    trace("test en cours (ce fichier sera remplacé par le rapport complet)");
    let mut lines = vec![format!("Verbatim {} {}", core::VERSION, if audio_test { "audio-test" } else { "selftest" })];
    let mut ok = true;
    let mut check = |name: &str, pass: bool, detail: String| {
        ok &= pass;
        let line = format!("{} {name} {detail}", if pass { "OK  " } else { "ÉCHEC" });
        trace(&line);
        lines.push(line);
    };
    let p = Paths::system();
    let s = core::load_settings(&p);
    check("bibliothèque", true, p.library(&s).display().to_string());
    check("DPAPI réel", vault::IS_REAL, String::new());
    let secret = b"sk_selftest_0123456789";
    match vault::protect(secret) {
        Ok(blob) => {
            check("chiffrement", !blob.windows(secret.len()).any(|w| w == secret), format!("{} octets", blob.len()));
            check("déchiffrement", vault::unprotect(&blob).map(|b| b == secret).unwrap_or(false), String::new());
        }
        Err(e) => check("chiffrement", false, e.to_string()),
    }
    // Chaîne audio complète : WAV → FLAC → décodage → 16 kHz.
    let dir = std::env::temp_dir().join(format!("verbatim-selftest-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let chain = (|| -> std::io::Result<f64> {
        let mut w = audio::WavWriter::create(&recorder::wav_path(&dir, Role::Mic), audio::STORE_RATE)?;
        let x: Vec<f32> = (0..audio::STORE_RATE * 2).map(|i| 0.3 * (i as f32 * 0.05).sin()).collect();
        w.write(&x)?;
        w.finish()?;
        let f = recorder::finalize(&dir, &[Role::Mic], false, |_| {})?;
        let n = audio::decode_mono(&dir.join("micro.flac"), audio::UPLOAD_RATE, |_| true)?;
        Ok(f.duration + n as f64 / 1e9)
    })();
    check("chaîne audio", chain.as_ref().map(|d| (d - 2.0).abs() < 0.01).unwrap_or(false), format!("{chain:?}"));
    let _ = std::fs::remove_dir_all(&dir);
    #[cfg(windows)]
    if audio_test {
        let (diag, pass) = capture_win::diagnose(4.0);
        check("capture micro et son du PC", pass, "(détail ci-dessous)".into());
        let (rec, rec_ok) = capture_win::record_check(10.0);
        check("enregistrement réel de 10 s", rec_ok, "(détail ci-dessous)".into());
        lines.push("--- capture (4 s par source) ---".into());
        lines.extend(diag);
        lines.push("--- enregistrement micro + son du PC (10 s) ---".into());
        lines.extend(rec);
    }
    #[cfg(not(windows))]
    let _ = audio_test;
    lines.push(if ok { "RÉSULTAT : tout est bon".into() } else { "RÉSULTAT : au moins un échec".into() });
    let _ = std::fs::write(report, lines.join("\r\n"));
    ok
}
