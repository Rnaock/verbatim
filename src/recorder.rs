//! Enregistrement multi-pistes, indépendant de Windows.
//!
//! Chaque source (micro, son du PC) tourne dans son propre fil et envoie des
//! blocs d'échantillons datés sur une horloge commune. Un fil d'écriture par
//! piste les place sur la ligne de temps :
//!
//! - quand une source se tait (Windows n'envoie rien pendant que le PC est
//!   silencieux), la piste est complétée par du silence au rythme de l'horloge ;
//! - quand un périphérique change en cours de route, avec une autre fréquence,
//!   le rééchantillonneur est remplacé sans couper la piste ;
//! - la dérive entre horloges matérielles est rattrapée dans les silences,
//!   pour que micro et PC restent alignés à quelques dizaines de ms près.
//!
//! Les pistes sont écrites en WAV avec un point de contrôle toutes les deux
//! secondes ; `finalize` les convertit ensuite en FLAC. Après un plantage,
//! `finalize` sait repartir des WAV interrompus.

use crate::audio::{self, Activity, EnvelopeBuilder, FlacWriter, Resampler, WavReader, WavWriter, STORE_RATE};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Écart au-delà duquel on considère qu'il manque de l'audio (coupure).
const GAP: f64 = 0.25;
/// Dérive tolérée avant correction discrète dans les silences.
const DRIFT: f64 = 0.06;
/// Marge laissée aux sources avant de combler un silence (latence des tampons).
const IDLE_MARGIN: f64 = 0.35;
const QUIET_DB: f32 = -45.0;

// ---------------------------------------------------------------------------
// Horloge
// ---------------------------------------------------------------------------
pub trait Clock: Send + Sync {
    /// Secondes écoulées depuis une origine fixe.
    fn now(&self) -> f64;
}

pub struct SystemClock(Instant);

impl SystemClock {
    pub fn new() -> Self {
        SystemClock(Instant::now())
    }
    pub fn since(&self, t: Instant) -> f64 {
        t.saturating_duration_since(self.0).as_secs_f64()
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> f64 {
        self.0.elapsed().as_secs_f64()
    }
}

/// Horloge pilotée à la main, pour les tests.
#[derive(Default)]
pub struct ManualClock(Mutex<f64>);

impl ManualClock {
    pub fn set(&self, t: f64) {
        *self.0.lock().unwrap() = t;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> f64 {
        *self.0.lock().unwrap()
    }
}

// ---------------------------------------------------------------------------
// Sources
// ---------------------------------------------------------------------------
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Mic,
    Pc,
}

impl Role {
    pub fn stem(self) -> &'static str {
        match self {
            Role::Mic => "micro",
            Role::Pc => "pc",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Role::Mic => "Micro",
            Role::Pc => "Son du PC",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceState {
    Starting,
    Active,
    /// Périphérique absent ou en cours de remplacement : la piste continue en silence.
    Waiting,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceStatus {
    pub state: SourceState,
    pub device: Option<String>,
    pub message: Option<String>,
}

impl SourceStatus {
    pub fn new(state: SourceState, device: Option<String>, message: Option<String>) -> Self {
        SourceStatus { state, device, message }
    }
}

pub enum SourceMsg {
    /// Échantillons mono ; `at` = instant (horloge commune) du premier échantillon.
    Audio { samples: Vec<f32>, rate: u32, at: f64 },
    Status(SourceStatus),
}

/// Une source tourne dans son propre fil jusqu'à ce que `stop` passe à vrai.
pub trait Source: Send + 'static {
    fn run(self: Box<Self>, tx: Sender<SourceMsg>, stop: Arc<AtomicBool>);
}

/// Source construite à partir d'une fermeture (tests, sources simulées).
pub struct FnSource<F: FnOnce(Sender<SourceMsg>, Arc<AtomicBool>) + Send + 'static>(pub F);

impl<F: FnOnce(Sender<SourceMsg>, Arc<AtomicBool>) + Send + 'static> Source for FnSource<F> {
    fn run(self: Box<Self>, tx: Sender<SourceMsg>, stop: Arc<AtomicBool>) {
        (self.0)(tx, stop)
    }
}

// ---------------------------------------------------------------------------
// Ligne de temps (avec pauses)
// ---------------------------------------------------------------------------
#[derive(Debug, Default)]
pub struct Timeline {
    start: f64,
    pauses: Vec<(f64, Option<f64>)>,
}

impl Timeline {
    pub fn new(start: f64) -> Self {
        Timeline { start, pauses: vec![] }
    }

    pub fn paused(&self) -> bool {
        matches!(self.pauses.last(), Some((_, None)))
    }

    pub fn pause(&mut self, t: f64) {
        if !self.paused() {
            self.pauses.push((t, None));
        }
    }

    pub fn resume(&mut self, t: f64) {
        if let Some(p) = self.pauses.last_mut() {
            if p.1.is_none() {
                p.1 = Some(t.max(p.0));
            }
        }
    }

    /// Position dans l'enregistrement d'un instant d'horloge ; None pendant une pause.
    pub fn media(&self, t: f64) -> Option<f64> {
        let mut off = 0.0;
        for &(a, b) in &self.pauses {
            match b {
                Some(b) if t >= b => off += b - a,
                _ if t >= a => return None,
                _ => {}
            }
        }
        Some((t - self.start - off).max(0.0))
    }

    /// Position courante : figée au début de la pause en cours.
    pub fn media_now(&self, t: f64) -> f64 {
        match self.pauses.last() {
            Some(&(a, None)) => self.media(a - 1e-9).unwrap_or(0.0),
            _ => self.media(t).unwrap_or(0.0),
        }
    }
}

// ---------------------------------------------------------------------------
// Fil d'écriture d'une piste
// ---------------------------------------------------------------------------
pub struct TrackShared {
    pub role: Role,
    pub peak: AtomicU32,
    pub frames: AtomicU64,
    pub status: Mutex<SourceStatus>,
    pub write_error: Mutex<Option<String>>,
    pub gaps: AtomicU64,
}

impl TrackShared {
    fn new(role: Role) -> Self {
        TrackShared {
            role,
            peak: AtomicU32::new(0),
            frames: AtomicU64::new(0),
            status: Mutex::new(SourceStatus::new(SourceState::Starting, None, None)),
            write_error: Mutex::new(None),
            gaps: AtomicU64::new(0),
        }
    }

    /// Crête depuis la dernière lecture (pour le vumètre).
    pub fn take_peak(&self) -> f32 {
        f32::from_bits(self.peak.swap(0, Ordering::Relaxed))
    }

    fn bump_peak(&self, p: f32) {
        let mut cur = self.peak.load(Ordering::Relaxed);
        while p > f32::from_bits(cur) {
            match self.peak.compare_exchange_weak(cur, p.to_bits(), Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(c) => cur = c,
            }
        }
    }
}

struct TrackWriter {
    shared: Arc<TrackShared>,
    wav: WavWriter,
    rs: Option<Resampler>,
    timeline: Arc<Mutex<Timeline>>,
    clock: Arc<dyn Clock>,
    out: Vec<f32>,
    last_checkpoint: f64,
    last_sync: f64,
    failed: bool,
}

impl TrackWriter {
    /// Position logique en secondes : écrit + ce qui attend dans le rééchantillonneur.
    fn position(&self) -> f64 {
        let pending = self.rs.as_ref().map(|r| r.pending_out()).unwrap_or(0.0);
        (self.wav.frames() as f64 + pending) / STORE_RATE as f64
    }

    fn write_out(&mut self) {
        if self.failed || self.out.is_empty() {
            self.out.clear();
            return;
        }
        if let Err(e) = self.wav.write(&self.out) {
            self.fail(e);
        }
        self.out.clear();
        self.shared.frames.store(self.wav.frames(), Ordering::Relaxed);
    }

    fn fail(&mut self, e: io::Error) {
        self.failed = true;
        let msg = if e.raw_os_error() == Some(112) || e.kind() == io::ErrorKind::StorageFull {
            "Disque plein : l'enregistrement de cette piste est arrêté.".to_string()
        } else {
            format!("Écriture impossible : {e}")
        };
        *self.shared.write_error.lock().unwrap() = Some(msg);
    }

    /// Ajoute du silence, en passant par le rééchantillonneur pour garder l'ordre.
    fn silence(&mut self, secs: f64) {
        if secs <= 0.0 {
            return;
        }
        match self.rs.take() {
            Some(mut rs) => {
                let n = (secs * rs.rates().0 as f64).round() as usize;
                let zeros = vec![0f32; n.min(STORE_RATE as usize * 4)];
                let mut left = n;
                while left > 0 {
                    let k = left.min(zeros.len());
                    rs.push(&zeros[..k], &mut self.out);
                    left -= k;
                    if self.out.len() > STORE_RATE as usize * 4 {
                        self.write_out();
                    }
                }
                self.rs = Some(rs);
            }
            None => {
                let n = (secs * STORE_RATE as f64).round() as u64;
                if !self.failed {
                    if let Err(e) = self.wav.write_silence(n) {
                        self.fail(e);
                    }
                }
                self.shared.frames.store(self.wav.frames(), Ordering::Relaxed);
            }
        }
        self.write_out();
    }

    fn switch_rate(&mut self, rate: u32) {
        if let Some(mut old) = self.rs.take() {
            old.finish(&mut self.out);
            self.write_out();
        }
        self.rs = Some(Resampler::new(rate, STORE_RATE));
    }

    fn on_audio(&mut self, mut samples: Vec<f32>, rate: u32, at: f64) {
        let Some(media) = self.timeline.lock().unwrap().media(at) else { return };
        if self.rs.as_ref().map(|r| r.rates().0) != Some(rate) {
            self.switch_rate(rate);
        }
        let peak = samples.iter().fold(0f32, |m, s| m.max(s.abs()));
        self.shared.bump_peak(peak);
        let quiet = 20.0 * peak.max(1e-9).log10() < QUIET_DB;
        let diff = media - self.position();
        if diff > GAP {
            // Il manque de l'audio : coupure, source silencieuse, changement de périphérique.
            self.shared.gaps.fetch_add(1, Ordering::Relaxed);
            self.silence(diff);
        } else if diff > DRIFT && quiet {
            self.silence((diff - DRIFT / 2.0).min(samples.len() as f64 / rate as f64));
        } else if diff < -DRIFT && quiet {
            let drop = (((-diff - DRIFT / 2.0) * rate as f64) as usize).min(samples.len() / 2);
            samples.drain(..drop);
        }
        if let Some(rs) = self.rs.as_mut() {
            rs.push(&samples, &mut self.out);
        }
        self.write_out();
    }

    fn on_idle(&mut self) {
        let tl = self.timeline.lock().unwrap();
        let now = tl.media_now(self.clock.now());
        let paused = tl.paused();
        drop(tl);
        if paused {
            return;
        }
        let behind = now - IDLE_MARGIN - self.position();
        if behind > GAP {
            self.silence(behind);
        }
    }

    fn maybe_checkpoint(&mut self) {
        let now = self.clock.now();
        if self.failed || now - self.last_checkpoint < 2.0 {
            return;
        }
        let sync = now - self.last_sync >= 10.0;
        if let Err(e) = self.wav.checkpoint(sync) {
            self.fail(e);
        }
        self.last_checkpoint = now;
        if sync {
            self.last_sync = now;
        }
    }

    fn run(mut self, rx: Receiver<SourceMsg>, end: Arc<Mutex<Option<f64>>>) -> io::Result<u64> {
        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(SourceMsg::Audio { samples, rate, at }) => self.on_audio(samples, rate, at),
                Ok(SourceMsg::Status(s)) => {
                    // Nouveau périphérique : on repart d'un rééchantillonneur propre.
                    if s.state != SourceState::Active {
                        if let Some(mut old) = self.rs.take() {
                            old.finish(&mut self.out);
                            self.write_out();
                        }
                    }
                    *self.shared.status.lock().unwrap() = s;
                }
                Err(RecvTimeoutError::Timeout) => self.on_idle(),
                Err(RecvTimeoutError::Disconnected) => break,
            }
            self.maybe_checkpoint();
        }
        if let Some(mut rs) = self.rs.take() {
            rs.finish(&mut self.out);
            self.write_out();
        }
        if let Some(end) = *end.lock().unwrap() {
            let missing = end - self.position();
            if missing > 0.0 {
                self.silence(missing);
            }
        }
        let frames = self.wav.finish()?;
        Ok(frames)
    }
}

// ---------------------------------------------------------------------------
// Enregistreur
// ---------------------------------------------------------------------------
struct Track {
    shared: Arc<TrackShared>,
    stop: Arc<AtomicBool>,
    source: Option<JoinHandle<()>>,
    writer: Option<JoinHandle<io::Result<u64>>>,
}

pub struct Recorder {
    pub dir: PathBuf,
    clock: Arc<dyn Clock>,
    timeline: Arc<Mutex<Timeline>>,
    end: Arc<Mutex<Option<f64>>>,
    tracks: Vec<Track>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrackLive {
    pub role: Role,
    pub peak_db: f32,
    pub status: SourceStatus,
    pub error: Option<String>,
    pub seconds: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LiveStatus {
    pub elapsed: f64,
    pub paused: bool,
    pub tracks: Vec<TrackLive>,
}

pub fn wav_path(dir: &Path, role: Role) -> PathBuf {
    dir.join(format!("{}.wav", role.stem()))
}

impl Recorder {
    pub fn start(dir: &Path, clock: Arc<dyn Clock>, sources: Vec<(Role, Box<dyn Source>)>) -> io::Result<Recorder> {
        std::fs::create_dir_all(dir)?;
        let timeline = Arc::new(Mutex::new(Timeline::new(clock.now())));
        let end = Arc::new(Mutex::new(None));
        let mut tracks: Vec<Track> = vec![];
        // En cas d'échec au milieu, les pistes déjà lancées sont arrêtées : un
        // micro ne doit jamais rester ouvert sans enregistrement visible.
        let abort = |tracks: Vec<Track>, end: &Arc<Mutex<Option<f64>>>, e: io::Error| -> io::Result<Recorder> {
            *end.lock().unwrap() = Some(0.0);
            for t in &tracks {
                t.stop.store(true, Ordering::SeqCst);
            }
            for mut t in tracks {
                if let Some(h) = t.source.take() {
                    let _ = h.join();
                }
                if let Some(h) = t.writer.take() {
                    let _ = h.join();
                }
            }
            Err(e)
        };
        for (role, src) in sources {
            let shared = Arc::new(TrackShared::new(role));
            let wav = match WavWriter::create(&wav_path(dir, role), STORE_RATE) {
                Ok(w) => w,
                Err(e) => return abort(tracks, &end, e),
            };
            let (tx, rx) = crossbeam_channel::bounded::<SourceMsg>(512);
            let stop = Arc::new(AtomicBool::new(false));
            let now = clock.now();
            let writer = TrackWriter {
                shared: shared.clone(),
                wav,
                rs: None,
                timeline: timeline.clone(),
                clock: clock.clone(),
                out: Vec::with_capacity(STORE_RATE as usize),
                last_checkpoint: now,
                last_sync: now,
                failed: false,
            };
            let end2 = end.clone();
            let wh = match std::thread::Builder::new()
                .name(format!("verbatim-ecriture-{}", role.stem()))
                .spawn(move || writer.run(rx, end2))
            {
                Ok(h) => h,
                Err(e) => return abort(tracks, &end, e),
            };
            let stop2 = stop.clone();
            let sh = match std::thread::Builder::new()
                .name(format!("verbatim-capture-{}", role.stem()))
                .spawn(move || src.run(tx, stop2))
            {
                Ok(h) => h,
                Err(e) => {
                    // `tx` est parti avec la fermeture : le fil d'écriture s'arrête seul.
                    tracks.push(Track { shared, stop, source: None, writer: Some(wh) });
                    return abort(tracks, &end, e);
                }
            };
            tracks.push(Track { shared, stop, source: Some(sh), writer: Some(wh) });
        }
        Ok(Recorder { dir: dir.to_path_buf(), clock, timeline, end, tracks })
    }

    pub fn roles(&self) -> Vec<Role> {
        self.tracks.iter().map(|t| t.shared.role).collect()
    }

    pub fn pause(&self) {
        self.timeline.lock().unwrap().pause(self.clock.now());
    }

    pub fn resume(&self) {
        self.timeline.lock().unwrap().resume(self.clock.now());
    }

    pub fn elapsed(&self) -> f64 {
        self.timeline.lock().unwrap().media_now(self.clock.now())
    }

    pub fn live(&self) -> LiveStatus {
        let tl = self.timeline.lock().unwrap();
        let elapsed = tl.media_now(self.clock.now());
        let paused = tl.paused();
        drop(tl);
        LiveStatus {
            elapsed,
            paused,
            tracks: self
                .tracks
                .iter()
                .map(|t| {
                    let p = t.shared.take_peak();
                    TrackLive {
                        role: t.shared.role,
                        peak_db: if p > 0.0 { (20.0 * p.log10()).max(-90.0) } else { -90.0 },
                        status: t.shared.status.lock().unwrap().clone(),
                        error: t.shared.write_error.lock().unwrap().clone(),
                        seconds: t.shared.frames.load(Ordering::Relaxed) as f64 / STORE_RATE as f64,
                    }
                })
                .collect(),
        }
    }

    /// Arrête les sources, vide les tampons, aligne les pistes sur la même durée.
    pub fn stop(mut self) -> io::Result<f64> {
        let end = self.elapsed();
        *self.end.lock().unwrap() = Some(end);
        for t in &self.tracks {
            t.stop.store(true, Ordering::SeqCst);
        }
        let mut first_err = None;
        for t in &mut self.tracks {
            if let Some(h) = t.source.take() {
                let _ = h.join();
            }
            if let Some(h) = t.writer.take() {
                match h.join() {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => first_err = first_err.or(Some(e)),
                    Err(_) => first_err = first_err.or(Some(io::Error::other("fil d'écriture interrompu"))),
                }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(end),
        }
    }
}

// ---------------------------------------------------------------------------
// Finalisation : WAV → FLAC, enveloppes, piste d'écoute
// ---------------------------------------------------------------------------
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TrackInfo {
    pub role: Role,
    pub file: String,
    pub activity: Activity,
    pub gain_db: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finalized {
    pub duration: f64,
    pub tracks: Vec<TrackInfo>,
    /// Corrélation micro/PC : au-delà de 0,5, le micro capte les haut-parleurs.
    pub echo: Option<f32>,
    pub playback: String,
    pub envelopes: Vec<(Role, Vec<u8>)>,
    pub recovered: bool,
}

/// Mélange de deux pistes : gains de parole, puis limiteur doux.
pub fn mix_sample(a: f32, b: f32) -> f32 {
    let s = a + b;
    if s.abs() <= 0.8 {
        s
    } else {
        s.signum() * (0.8 + 0.2 * ((s.abs() - 0.8) / 0.2).tanh())
    }
}

/// Convertit les pistes WAV d'un dossier en FLAC. Sert aussi après un plantage
/// (`recover` = true : les en-têtes sont d'abord réparés).
pub fn finalize<P: FnMut(f32)>(dir: &Path, roles: &[Role], recover: bool, mut progress: P) -> io::Result<Finalized> {
    let mut infos = vec![];
    let mut envelopes = vec![];
    let mut frames_max = 0u64;
    let present: Vec<Role> = roles.iter().copied().filter(|r| wav_path(dir, *r).exists()).collect();
    if present.is_empty() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "aucune piste à finaliser"));
    }
    let total_steps = present.len() as f32 + if present.len() > 1 { 1.0 } else { 0.0 };
    for (i, &role) in present.iter().enumerate() {
        let wav = wav_path(dir, role);
        if recover {
            audio::repair_wav(&wav)?;
        }
        let mut r = WavReader::open(&wav)?;
        let flac_name = format!("{}.flac", role.stem());
        let mut w = FlacWriter::create(&dir.join(&flac_name), r.rate)?;
        let mut env = EnvelopeBuilder::new(r.rate);
        let total = r.frames.max(1);
        let mut buf = Vec::with_capacity(48_000);
        let mut done = 0u64;
        loop {
            buf.clear();
            let n = r.read_mono(48_000, &mut buf)?;
            if n == 0 {
                break;
            }
            env.push(&buf);
            w.write(&buf)?;
            done += n as u64;
            progress((i as f32 + done as f32 / total as f32) / total_steps);
        }
        let written = w.finish()?;
        frames_max = frames_max.max(written);
        let env = env.finish();
        let act = audio::activity(&env);
        infos.push(TrackInfo { role, file: flac_name, activity: act, gain_db: audio::speech_gain_db(&act) });
        envelopes.push((role, env));
    }

    let echo = match (envelopes.iter().find(|e| e.0 == Role::Mic), envelopes.iter().find(|e| e.0 == Role::Pc)) {
        (Some(m), Some(p)) => audio::echo_coupling(&m.1, &p.1),
        _ => None,
    };

    // Piste d'écoute : les deux sources mélangées à niveau égal.
    let playback = if infos.len() == 1 {
        infos[0].file.clone()
    } else {
        let name = "ecoute.flac".to_string();
        let mut readers: Vec<(WavReader, f32)> = infos
            .iter()
            .map(|t| Ok((WavReader::open(&wav_path(dir, t.role))?, audio::db_to_gain(t.gain_db))))
            .collect::<io::Result<_>>()?;
        let mut w = FlacWriter::create(&dir.join(&name), STORE_RATE)?;
        let (mut a, mut b, mut mix) = (vec![], vec![], vec![]);
        let mut done = 0u64;
        loop {
            a.clear();
            b.clear();
            let na = readers[0].0.read_mono(48_000, &mut a)?;
            let nb = readers[1].0.read_mono(48_000, &mut b)?;
            if na == 0 && nb == 0 {
                break;
            }
            let n = na.max(nb);
            let (ga, gb) = (readers[0].1, readers[1].1);
            mix.clear();
            mix.extend((0..n).map(|i| mix_sample(a.get(i).copied().unwrap_or(0.0) * ga, b.get(i).copied().unwrap_or(0.0) * gb)));
            w.write(&mix)?;
            done += n as u64;
            progress((total_steps - 1.0 + done as f32 / frames_max.max(1) as f32) / total_steps);
        }
        w.finish()?;
        name
    };

    // Vérification avant de supprimer quoi que ce soit.
    for t in &infos {
        let info = audio::media_info(&dir.join(&t.file))?;
        let wav_frames = WavReader::open(&wav_path(dir, t.role))?.frames;
        let flac_frames = (info.duration.unwrap_or(0.0) * STORE_RATE as f64).round() as u64;
        if flac_frames.abs_diff(wav_frames) > 1 {
            return Err(io::Error::other(format!("vérification du FLAC {} échouée", t.file)));
        }
    }
    // Les WAV ne sont supprimés qu'une fois la conversation enregistrée comme
    // prête (voir `remove_wavs`) : un plantage entre-temps ne perd rien.
    progress(1.0);
    Ok(Finalized {
        duration: frames_max as f64 / STORE_RATE as f64,
        tracks: infos,
        echo,
        playback,
        envelopes,
        recovered: recover,
    })
}

/// Supprime les WAV de travail, une fois les FLAC vérifiés et la conversation
/// enregistrée comme prête.
pub fn remove_wavs(dir: &Path, roles: &[Role]) {
    for r in roles {
        let _ = std::fs::remove_file(wav_path(dir, *r));
    }
}

// ---------------------------------------------------------------------------
// Source simulée (développement sous Linux, tests de bout en bout)
// ---------------------------------------------------------------------------
/// Parole simulée : bruit modulé par phrases, à une fréquence donnée,
/// envoyée en temps réel par paquets de 10 ms. `script(t)` dit si la source
/// émet à l'instant t (secondes depuis le début).
pub struct SynthSource {
    pub clock: Arc<SystemClock>,
    pub device: String,
    pub rate: u32,
    pub amp: f32,
    pub seed: u32,
    pub script: fn(f64) -> bool,
    /// Simule une source muette (Windows n'envoie rien pendant les silences du PC).
    pub silent_gaps: bool,
    /// Change de périphérique (et de fréquence) après ce délai.
    pub switch_after: Option<(f64, u32, String)>,
}

impl Source for SynthSource {
    fn run(self: Box<Self>, tx: Sender<SourceMsg>, stop: Arc<AtomicBool>) {
        let s = *self;
        let mut rate = s.rate;
        let mut device = s.device.clone();
        let _ = tx.send(SourceMsg::Status(SourceStatus::new(SourceState::Active, Some(device.clone()), None)));
        let t0 = s.clock.now();
        let mut sent = 0u64;
        let mut rng = s.seed.max(1);
        let mut switched = false;
        while !stop.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(10));
            let now = s.clock.now();
            if let Some((after, r, ref name)) = s.switch_after {
                if !switched && now - t0 >= after {
                    switched = true;
                    let _ = tx.send(SourceMsg::Status(SourceStatus::new(SourceState::Waiting, None, Some("changement de périphérique".into()))));
                    std::thread::sleep(Duration::from_millis(400));
                    rate = r;
                    device = name.clone();
                    let _ = tx.send(SourceMsg::Status(SourceStatus::new(SourceState::Active, Some(device.clone()), None)));
                    sent = ((s.clock.now() - t0) * rate as f64) as u64;
                    continue;
                }
            }
            let due = ((now - t0) * rate as f64) as u64;
            if due <= sent {
                continue;
            }
            let n = (due - sent) as usize;
            let at = t0 + sent as f64 / rate as f64;
            sent = due;
            let talking = (s.script)(at - t0);
            if !talking && s.silent_gaps {
                continue;
            }
            let mut v = Vec::with_capacity(n);
            for i in 0..n {
                rng ^= rng << 13;
                rng ^= rng >> 17;
                rng ^= rng << 5;
                let noise = (rng as f32 / u32::MAX as f32) * 2.0 - 1.0;
                let t = at + i as f64 / rate as f64;
                let syll = (0.5 + 0.5 * (t * 2.0 * std::f64::consts::PI * 4.0).sin()) as f32;
                v.push(if talking { s.amp * syll * noise } else { 0.0005 * noise });
            }
            if tx.send(SourceMsg::Audio { samples: v, rate, at }).is_err() {
                break;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::tests::{sine, tmpdir};

    /// Rejoue un scénario sur une horloge manuelle : chaque étape avance
    /// l'horloge puis envoie (ou non) un paquet.
    fn drive(clock: &ManualClock, tx: &Sender<SourceMsg>, from: f64, to: f64, rate: u32, step: f64, f: impl Fn(f64) -> Option<f32>) {
        let mut t = from;
        while t < to - 1e-9 {
            clock.set(t + step);
            if let Some(freq) = f(t) {
                let n = (step * rate as f64).round() as usize;
                let samples: Vec<f32> = (0..n)
                    .map(|i| 0.3 * (2.0 * std::f32::consts::PI * freq * (t as f32 + i as f32 / rate as f32)).sin())
                    .collect();
                tx.send(SourceMsg::Audio { samples, rate, at: t }).unwrap();
            }
            std::thread::sleep(Duration::from_millis(2));
            t += step;
        }
    }

    fn read_all(p: &Path) -> Vec<f32> {
        let mut r = WavReader::open(p).unwrap();
        let mut v = vec![];
        while r.read_mono(48_000, &mut v).unwrap() > 0 {}
        v
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|s| s * s).sum::<f32>() / x.len().max(1) as f32).sqrt()
    }

    #[test]
    fn ligne_de_temps_et_pauses() {
        let mut tl = Timeline::new(10.0);
        assert_eq!(tl.media(12.0), Some(2.0));
        tl.pause(15.0);
        assert_eq!(tl.media(16.0), None);
        assert_eq!(tl.media_now(18.0), 5.0 - 1e-9);
        tl.resume(20.0);
        assert_eq!(tl.media(21.0), Some(6.0));
        assert_eq!(tl.media(14.0), Some(4.0));
    }

    /// Le PC se tait (aucun paquet), le micro change de périphérique et de
    /// fréquence, puis pause : les deux pistes restent alignées sur l'horloge.
    #[test]
    fn deux_pistes_alignees_malgre_silences_changement_et_pause() {
        let dir = tmpdir("rec");
        let clock = Arc::new(ManualClock::default());
        let (mic_tx, mic_rx) = crossbeam_channel::unbounded();
        let (pc_tx, pc_rx) = crossbeam_channel::unbounded();
        let mic_rx = Arc::new(Mutex::new(Some(mic_rx)));
        let pc_rx = Arc::new(Mutex::new(Some(pc_rx)));
        let relay = |rx: Arc<Mutex<Option<Receiver<SourceMsg>>>>| {
            Box::new(FnSource(move |tx: Sender<SourceMsg>, stop: Arc<AtomicBool>| {
                let rx = rx.lock().unwrap().take().unwrap();
                while !stop.load(Ordering::SeqCst) {
                    if let Ok(m) = rx.recv_timeout(Duration::from_millis(5)) {
                        let _ = tx.send(m);
                    }
                }
                while let Ok(m) = rx.try_recv() {
                    let _ = tx.send(m);
                }
            })) as Box<dyn Source>
        };
        let rec = Recorder::start(&dir, clock.clone(), vec![(Role::Mic, relay(mic_rx)), (Role::Pc, relay(pc_rx))]).unwrap();

        // 0-3 s : micro à 48 kHz (440 Hz), PC à 44,1 kHz parle de 1 à 2 s seulement.
        let step = 0.02;
        let mut t = 0.0;
        while t < 3.0 {
            clock.set(t + step);
            let mic: Vec<f32> = sine(440.0, 48_000, step as f32, 0.3);
            mic_tx.send(SourceMsg::Audio { samples: mic, rate: 48_000, at: t }).unwrap();
            if (1.0..2.0).contains(&t) {
                pc_tx.send(SourceMsg::Audio { samples: sine(1000.0, 44_100, step as f32, 0.3), rate: 44_100, at: t }).unwrap();
            }
            std::thread::sleep(Duration::from_millis(2));
            t += step;
        }
        // 3-4 s : le micro disparaît (changement de casque).
        mic_tx.send(SourceMsg::Status(SourceStatus::new(SourceState::Waiting, None, None))).unwrap();
        drive(&clock, &pc_tx, 3.0, 4.0, 44_100, step, |_| None);
        // 4-6 s : nouveau micro à 16 kHz.
        mic_tx.send(SourceMsg::Status(SourceStatus::new(SourceState::Active, Some("Webcam".into()), None))).unwrap();
        drive(&clock, &mic_tx, 4.0, 6.0, 16_000, step, |_| Some(300.0));
        // Pause de 6 à 9 s : ce qui arrive est ignoré.
        rec.pause();
        drive(&clock, &mic_tx, 6.0, 9.0, 16_000, step, |_| Some(300.0));
        rec.resume();
        // 9-10 s : le PC reparle.
        drive(&clock, &pc_tx, 9.0, 10.0, 44_100, step, |_| Some(1000.0));
        clock.set(10.0);
        std::thread::sleep(Duration::from_millis(400));
        let live = rec.live();
        assert!(live.tracks.iter().all(|t| t.error.is_none()));
        let end = rec.stop().unwrap();
        assert!((end - 7.0).abs() < 0.05, "durée hors pause : {end}");

        let mic = read_all(&wav_path(&dir, Role::Mic));
        let pc = read_all(&wav_path(&dir, Role::Pc));
        let r = STORE_RATE as usize;
        assert!((mic.len() as f64 / r as f64 - 7.0).abs() < 0.08, "micro : {}", mic.len() as f64 / r as f64);
        assert!((pc.len() as f64 / r as f64 - 7.0).abs() < 0.08, "pc : {}", pc.len() as f64 / r as f64);
        let at = |x: &[f32], a: f64, b: f64| rms(&x[(a * r as f64) as usize..(b * r as f64) as usize]);
        // Le PC parle exactement de 1 à 2 s puis de 6 à 7 s (après la pause).
        assert!(at(&pc, 0.2, 0.9) < 0.01 && at(&pc, 1.1, 1.9) > 0.15 && at(&pc, 2.2, 5.8) < 0.01, "pc mal placé");
        assert!(at(&pc, 6.15, 6.85) > 0.15, "pc après pause mal placé");
        // Micro : parle 0-3, silence 3-4 (débranché), reparle 4-6, silence ensuite.
        assert!(at(&mic, 0.1, 2.9) > 0.15 && at(&mic, 3.2, 3.8) < 0.01 && at(&mic, 4.2, 5.8) > 0.15, "micro mal placé");
        assert!(at(&mic, 6.2, 6.9) < 0.01, "la pause ne doit rien enregistrer");
        // Le 300 Hz du nouveau micro a bien été rééchantillonné (pas accéléré).
        let seg = &mic[(4.5 * r as f64) as usize..(5.5 * r as f64) as usize];
        let zc = seg.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count();
        assert!((zc as i32 - 300).abs() <= 3, "{zc}");
    }

    #[test]
    fn finalisation_flac_enveloppes_echo_et_reprise() {
        let dir = tmpdir("fin");
        let r = STORE_RATE;
        let mut mic = WavWriter::create(&wav_path(&dir, Role::Mic), r).unwrap();
        let mut pc = WavWriter::create(&wav_path(&dir, Role::Pc), r).unwrap();
        for i in 0..20 {
            let me = i % 2 == 0;
            mic.write(&sine(220.0, r, 1.0, if me { 0.05 } else { 0.0005 })).unwrap();
            pc.write(&sine(330.0, r, 1.0, if me { 0.0005 } else { 0.3 })).unwrap();
        }
        mic.checkpoint(true).unwrap();
        pc.checkpoint(true).unwrap();
        // Plantage simulé : on ne termine pas proprement les fichiers.
        std::mem::forget(mic);
        std::mem::forget(pc);
        let mut last = 0.0;
        let f = finalize(&dir, &[Role::Mic, Role::Pc], true, |p| {
            assert!(p >= last - 1e-6, "progression croissante");
            last = p;
        })
        .unwrap();
        assert!((last - 1.0).abs() < 1e-6);
        assert!((f.duration - 20.0).abs() < 0.01, "{}", f.duration);
        assert_eq!(f.playback, "ecoute.flac");
        assert!(wav_path(&dir, Role::Mic).exists(), "WAV gardés tant que la conversation n'est pas enregistrée");
        assert!(dir.join("micro.flac").exists() && dir.join("ecoute.flac").exists());
        remove_wavs(&dir, &[Role::Mic, Role::Pc]);
        assert!(!wav_path(&dir, Role::Mic).exists() && !wav_path(&dir, Role::Pc).exists());
        let m = f.tracks.iter().find(|t| t.role == Role::Mic).unwrap();
        let p = f.tracks.iter().find(|t| t.role == Role::Pc).unwrap();
        assert!((m.activity.speech_ratio - 0.5).abs() < 0.05, "{:?}", m.activity);
        assert!((m.gain_db - 9.0).abs() < 0.6, "micro faible remonté (−29 dB → −20 dB) : {}", m.gain_db);
        assert!(p.gain_db < 0.0 && p.gain_db > -6.1, "PC fort atténué : {}", p.gain_db);
        assert_eq!(f.envelopes[0].1.len(), 1000);
        // Casque : le micro ne suit pas le PC.
        assert!(f.echo.is_none() || f.echo.unwrap() < 0.4, "{:?}", f.echo);
        let info = audio::media_info(&dir.join("ecoute.flac")).unwrap();
        assert!((info.duration.unwrap() - 20.0).abs() < 0.01);
    }

    #[test]
    fn enregistrement_reel_avec_sources_simulees() {
        let dir = tmpdir("synth");
        let clock = Arc::new(SystemClock::new());
        let mic = SynthSource {
            clock: clock.clone(),
            device: "Micro simulé".into(),
            rate: 48_000,
            amp: 0.3,
            seed: 7,
            script: |t| (t % 2.0) < 1.0,
            silent_gaps: false,
            switch_after: Some((1.2, 44_100, "Autre micro".into())),
        };
        let pc = SynthSource {
            clock: clock.clone(),
            device: "Tout le son du PC".into(),
            rate: 48_000,
            amp: 0.3,
            seed: 9,
            script: |t| (t % 2.0) >= 1.0,
            silent_gaps: true,
            switch_after: None,
        };
        let rec = Recorder::start(&dir, clock.clone(), vec![(Role::Mic, Box::new(mic)), (Role::Pc, Box::new(pc))]).unwrap();
        std::thread::sleep(Duration::from_millis(3000));
        let live = rec.live();
        assert_eq!(live.tracks.len(), 2);
        assert!(live.tracks.iter().all(|t| t.status.state == SourceState::Active), "{live:?}");
        assert!(live.tracks.iter().any(|t| t.status.device.as_deref() == Some("Autre micro")));
        let end = rec.stop().unwrap();
        let f = finalize(&dir, &[Role::Mic, Role::Pc], false, |_| {}).unwrap();
        assert!((f.duration - end).abs() < 0.1, "{} vs {}", f.duration, end);
        for t in &f.tracks {
            assert!(t.activity.speech_ratio > 0.3 && t.activity.speech_ratio < 0.7, "{t:?}");
        }
    }
}
