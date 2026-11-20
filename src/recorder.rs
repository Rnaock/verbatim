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
//! secondes (filet de sécurité). En parallèle, pendant l'enregistrement, chaque
//! piste est compressée en FLAC, son enveloppe est calculée, et un mélange
//! d'écoute en 16 kHz est écrit au fil de l'eau : à l'arrêt il ne reste qu'à
//! vérifier. Après un plantage, `finalize` repart des WAV interrompus.

use crate::audio::{self, Activity, EnvelopeBuilder, FlacWriter, Resampler, WavReader, WavWriter, ENV_FPS, STORE_RATE, UPLOAD_RATE};
use std::collections::VecDeque;
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
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
    /// Gain visé pour le mélange d'écoute (millièmes de dB), mis à jour au fil
    /// de l'enregistrement d'après le niveau de parole mesuré.
    pub gain_target: Arc<AtomicI32>,
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
            gain_target: Arc::new(AtomicI32::new(0)),
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

// ---------------------------------------------------------------------------
// Mélange en direct : la piste d'écoute (16 kHz) s'écrit pendant l'appel
// ---------------------------------------------------------------------------
/// Bloc de mélange : 10 ms à 24 kHz.
const MIX_BLOCK: usize = 240;
/// Variation maximale du gain par bloc (6 dB par seconde) : pas de saut audible.
const MIX_SLEW_DB: f32 = 0.06;
/// Retard toléré d'une piste avant de la compter comme silencieuse.
const MIX_MAX_LAG: usize = STORE_RATE as usize * 20;

/// Reçoit les échantillons de chaque piste (déjà alignés sur la ligne de temps,
/// 24 kHz), les mélange à niveaux de parole égalisés et écrit `ecoute.flac`.
pub struct Mixer {
    queues: Vec<VecDeque<f32>>,
    /// Échantillons comptés comme silence faute d'être arrivés : à sauter ensuite.
    debt: Vec<usize>,
    targets: Vec<Arc<AtomicI32>>,
    gains_db: Vec<f32>,
    rs: Resampler,
    flac: Option<FlacWriter>,
    block: Vec<f32>,
    out: Vec<f32>,
}

pub fn playback_path(dir: &Path) -> PathBuf {
    dir.join("ecoute.flac")
}

impl Mixer {
    fn new(dir: &Path, targets: Vec<Arc<AtomicI32>>) -> io::Result<Self> {
        let n = targets.len();
        Ok(Mixer {
            queues: (0..n).map(|_| VecDeque::with_capacity(STORE_RATE as usize)).collect(),
            debt: vec![0; n],
            gains_db: vec![0.0; n],
            targets,
            rs: Resampler::new(STORE_RATE, UPLOAD_RATE),
            flac: Some(FlacWriter::create(&playback_path(dir), UPLOAD_RATE)?),
            block: Vec::with_capacity(MIX_BLOCK),
            out: Vec::with_capacity(MIX_BLOCK),
        })
    }

    fn push(&mut self, idx: usize, mut samples: &[f32]) {
        if self.flac.is_none() {
            return;
        }
        let skip = self.debt[idx].min(samples.len());
        self.debt[idx] -= skip;
        samples = &samples[skip..];
        self.queues[idx].extend(samples.iter().copied());
        let ready = self.queues.iter().map(|q| q.len()).min().unwrap_or(0);
        let longest = self.queues.iter().map(|q| q.len()).max().unwrap_or(0);
        // Une piste en retard de plus de 20 s (écriture en panne) ne bloque pas le mélange.
        let n = ready.max(longest.saturating_sub(MIX_MAX_LAG));
        self.mix(n - n % MIX_BLOCK);
    }

    fn mix(&mut self, mut n: usize) {
        while n > 0 {
            let len = n.min(MIX_BLOCK);
            n -= len;
            for (g, t) in self.gains_db.iter_mut().zip(&self.targets) {
                let target = t.load(Ordering::Relaxed) as f32 / 1000.0;
                *g += (target - *g).clamp(-MIX_SLEW_DB, MIX_SLEW_DB);
            }
            self.block.clear();
            self.block.resize(len, 0.0);
            for (k, q) in self.queues.iter_mut().enumerate() {
                let g = audio::db_to_gain(self.gains_db[k]);
                let have = q.len().min(len);
                for (o, s) in self.block.iter_mut().zip(q.drain(..have)) {
                    *o += s * g;
                }
                self.debt[k] += len - have;
            }
            for o in self.block.iter_mut() {
                *o = mix_sample(*o, 0.0);
            }
            self.out.clear();
            self.rs.push(&self.block, &mut self.out);
            self.write();
        }
    }

    fn write(&mut self) {
        if let Some(f) = self.flac.as_mut() {
            if !self.out.is_empty() && f.write(&self.out).is_err() {
                self.flac = None; // la piste d'écoute sera refaite à la finalisation
            }
        }
        self.out.clear();
    }

    /// Mélange ce qui reste et ferme le fichier. Renvoie sa durée en secondes.
    fn finish(mut self) -> Option<f64> {
        let longest = self.queues.iter().map(|q| q.len()).max().unwrap_or(0);
        self.mix(longest);
        self.out.clear();
        self.rs.finish(&mut self.out);
        self.write();
        let frames = self.flac.take()?.finish().ok()?;
        Some(frames as f64 / UPLOAD_RATE as f64)
    }
}

struct TrackWriter {
    shared: Arc<TrackShared>,
    wav: WavWriter,
    /// Compression en direct de la piste (le WAV reste le filet de sécurité).
    flac: Option<FlacWriter>,
    env: EnvelopeBuilder,
    mixer: Option<(Arc<Mutex<Mixer>>, usize)>,
    next_gain_at: usize,
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
            self.out.clear();
            return;
        }
        self.env.push(&self.out);
        if let Some(f) = self.flac.as_mut() {
            if f.write(&self.out).is_err() {
                self.flac = None; // le FLAC sera refait depuis le WAV à la finalisation
            }
        }
        if let Some((m, idx)) = &self.mixer {
            m.lock().unwrap().push(*idx, &self.out);
        }
        self.out.clear();
        self.shared.frames.store(self.wav.frames(), Ordering::Relaxed);
        self.update_gain();
    }

    /// Toutes les 5 s, recalcule le gain qui amène la parole de cette piste à
    /// niveau égal dans le mélange d'écoute (dès 2 s de parole entendue).
    fn update_gain(&mut self) {
        let n = self.env.frames.len();
        if n < self.next_gain_at {
            return;
        }
        self.next_gain_at = n + 5 * ENV_FPS as usize;
        let a = audio::activity(&self.env.frames);
        if a.speech_ratio * n as f32 >= 2.0 * ENV_FPS as f32 {
            self.shared.gain_target.store((audio::speech_gain_db(&a) * 1000.0) as i32, Ordering::Relaxed);
        }
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
                self.write_out();
                let mut left = (secs * STORE_RATE as f64).round() as u64;
                while left > 0 && !self.failed {
                    let k = left.min(STORE_RATE as u64) as usize;
                    self.out.resize(k, 0.0);
                    self.write_out();
                    left -= k as u64;
                }
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

    fn run(mut self, rx: Receiver<SourceMsg>, end: Arc<Mutex<Option<f64>>>) -> io::Result<TrackResult> {
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
        let flac_frames = self.flac.take().and_then(|f| f.finish().ok());
        Ok(TrackResult { frames, env: self.env.finish(), flac_ok: flac_frames == Some(frames) })
    }
}

struct TrackResult {
    frames: u64,
    env: Vec<u8>,
    flac_ok: bool,
}

/// Ce qui a été produit pendant l'enregistrement, pour une finalisation immédiate.
#[derive(Debug, Clone)]
pub struct Streamed {
    pub tracks: Vec<StreamedTrack>,
    /// Durée de la piste d'écoute écrite en direct, si elle est complète.
    pub mix_seconds: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct StreamedTrack {
    pub role: Role,
    pub env: Vec<u8>,
    pub frames: u64,
    pub flac_ok: bool,
}

#[derive(Debug)]
pub struct Stopped {
    pub duration: f64,
    pub streamed: Streamed,
}

// ---------------------------------------------------------------------------
// Enregistreur
// ---------------------------------------------------------------------------
struct Track {
    shared: Arc<TrackShared>,
    stop: Arc<AtomicBool>,
    source: Option<JoinHandle<()>>,
    writer: Option<JoinHandle<io::Result<TrackResult>>>,
}

pub struct Recorder {
    pub dir: PathBuf,
    clock: Arc<dyn Clock>,
    timeline: Arc<Mutex<Timeline>>,
    end: Arc<Mutex<Option<f64>>>,
    tracks: Vec<Track>,
    mixer: Option<Arc<Mutex<Mixer>>>,
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

pub fn flac_name(role: Role) -> String {
    format!("{}.flac", role.stem())
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
        let shareds: Vec<Arc<TrackShared>> = sources.iter().map(|(role, _)| Arc::new(TrackShared::new(*role))).collect();
        // Sans mélange en direct (création impossible), la finalisation le refera.
        let mixer = Mixer::new(dir, shareds.iter().map(|s| s.gain_target.clone()).collect()).ok().map(|m| Arc::new(Mutex::new(m)));
        for (idx, (role, src)) in sources.into_iter().enumerate() {
            let shared = shareds[idx].clone();
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
                flac: FlacWriter::create(&dir.join(flac_name(role)), STORE_RATE).ok(),
                env: EnvelopeBuilder::new(STORE_RATE),
                mixer: mixer.clone().map(|m| (m, idx)),
                next_gain_at: 3 * ENV_FPS as usize,
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
        Ok(Recorder { dir: dir.to_path_buf(), clock, timeline, end, tracks, mixer })
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
    pub fn stop(mut self) -> io::Result<Stopped> {
        let end = self.elapsed();
        *self.end.lock().unwrap() = Some(end);
        for t in &self.tracks {
            t.stop.store(true, Ordering::SeqCst);
        }
        let mut first_err = None;
        let mut done = vec![];
        for t in &mut self.tracks {
            if let Some(h) = t.source.take() {
                let _ = h.join();
            }
            if let Some(h) = t.writer.take() {
                match h.join() {
                    Ok(Ok(r)) => done.push(StreamedTrack { role: t.shared.role, env: r.env, frames: r.frames, flac_ok: r.flac_ok }),
                    Ok(Err(e)) => first_err = first_err.or(Some(e)),
                    Err(_) => first_err = first_err.or(Some(io::Error::other("fil d'écriture interrompu"))),
                }
            }
        }
        // Les fils d'écriture sont terminés : le mélange peut être fermé.
        let mix_seconds = self
            .mixer
            .take()
            .and_then(|m| Arc::try_unwrap(m).ok())
            .and_then(|m| m.into_inner().ok())
            .and_then(|m| m.finish());
        match first_err {
            Some(e) => Err(e),
            None => Ok(Stopped { duration: end, streamed: Streamed { tracks: done, mix_seconds } }),
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
    /// Fin du dernier son utile (secondes) : au-delà, toutes les pistes se taisent.
    pub active_end: f64,
}

/// Dernier instant où une piste au moins porte du son (parole, vidéo…), d'après
/// les enveloppes : il faut au moins 0,3 s de son dans la seconde qui précède.
pub fn active_end(envelopes: &[(Role, Vec<u8>)]) -> f64 {
    let n = envelopes.iter().map(|e| e.1.len()).max().unwrap_or(0);
    let mut act = vec![false; n];
    for (_, env) in envelopes {
        let thr = audio::speech_threshold(audio::activity(env).floor_db);
        for (a, &q) in act.iter_mut().zip(env) {
            *a |= audio::env_db(q) >= thr;
        }
    }
    let win = ENV_FPS as usize;
    let mut count = act.iter().rev().take(win).filter(|a| **a).count();
    let mut i = n;
    while i > 0 {
        if count >= 15 {
            // Dernier instant sonore de cette seconde.
            let last = (0..i).rev().find(|&k| act[k]).unwrap_or(0);
            return (last + 1) as f64 / ENV_FPS as f64;
        }
        i -= 1;
        if act[i] {
            count -= 1;
        }
        if i >= win && act[i - win] {
            count += 1;
        }
    }
    0.0
}

/// Finalisation immédiate : tout a été produit pendant l'enregistrement, il
/// ne reste qu'à vérifier les fichiers. `None` si une vérification échoue.
fn finalize_streamed(dir: &Path, s: &Streamed) -> Option<Finalized> {
    let mut infos = vec![];
    let mut envelopes = vec![];
    let mut frames_max = 0u64;
    if s.tracks.is_empty() {
        return None;
    }
    for t in &s.tracks {
        let name = flac_name(t.role);
        let info = audio::media_info(&dir.join(&name)).ok()?;
        let flac_frames = (info.duration? * STORE_RATE as f64).round() as u64;
        let wav_frames = WavReader::open(&wav_path(dir, t.role)).ok()?.frames;
        if !t.flac_ok || flac_frames.abs_diff(t.frames) > 1 || wav_frames != t.frames {
            return None;
        }
        frames_max = frames_max.max(t.frames);
        let act = audio::activity(&t.env);
        infos.push(TrackInfo { role: t.role, file: name, activity: act, gain_db: audio::speech_gain_db(&act) });
        envelopes.push((t.role, t.env.clone()));
    }
    let duration = frames_max as f64 / STORE_RATE as f64;
    let mix = audio::media_info(&playback_path(dir)).ok()?;
    if (s.mix_seconds? - duration).abs() > 0.1 || (mix.duration? - duration).abs() > 0.1 || mix.rate != Some(UPLOAD_RATE) {
        return None;
    }
    let echo = match (envelopes.iter().find(|e| e.0 == Role::Mic), envelopes.iter().find(|e| e.0 == Role::Pc)) {
        (Some(m), Some(p)) => audio::echo_coupling(&m.1, &p.1),
        _ => None,
    };
    let active_end = active_end(&envelopes);
    Some(Finalized { duration, tracks: infos, echo, playback: "ecoute.flac".into(), envelopes, recovered: false, active_end })
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
pub fn finalize<P: FnMut(f32)>(dir: &Path, roles: &[Role], recover: bool, progress: P) -> io::Result<Finalized> {
    finalize_with(dir, roles, recover, None, progress)
}

/// Comme `finalize`, en profitant de ce que l'enregistreur a déjà produit.
pub fn finalize_with<P: FnMut(f32)>(dir: &Path, roles: &[Role], recover: bool, streamed: Option<&Streamed>, mut progress: P) -> io::Result<Finalized> {
    if let Some(f) = streamed.filter(|_| !recover).and_then(|s| finalize_streamed(dir, s)) {
        progress(1.0);
        return Ok(f);
    }
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
        let flac_name = flac_name(role);
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

    // Piste d'écoute : les deux sources mélangées à niveau égal, en 16 kHz
    // (suffisant pour réécouter, et directement envoyable à la transcription).
    let playback = if infos.len() == 1 {
        let _ = std::fs::remove_file(playback_path(dir)); // mélange en direct incomplet
        infos[0].file.clone()
    } else {
        let name = "ecoute.flac".to_string();
        let mut readers: Vec<(WavReader, f32)> = infos
            .iter()
            .map(|t| Ok((WavReader::open(&wav_path(dir, t.role))?, audio::db_to_gain(t.gain_db))))
            .collect::<io::Result<_>>()?;
        let mut w = FlacWriter::create(&dir.join(&name), UPLOAD_RATE)?;
        let mut rs = Resampler::new(STORE_RATE, UPLOAD_RATE);
        let (mut a, mut b, mut mix, mut out) = (vec![], vec![], vec![], vec![]);
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
            out.clear();
            rs.push(&mix, &mut out);
            w.write(&out)?;
            done += n as u64;
            progress((total_steps - 1.0 + done as f32 / frames_max.max(1) as f32) / total_steps);
        }
        out.clear();
        rs.finish(&mut out);
        w.write(&out)?;
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
    let active_end = active_end(&envelopes);
    Ok(Finalized {
        duration: frames_max as f64 / STORE_RATE as f64,
        tracks: infos,
        echo,
        playback,
        envelopes,
        recovered: recover,
        active_end,
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
        let stopped = rec.stop().unwrap();
        let end = stopped.duration;
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
        assert_eq!(info.rate, Some(UPLOAD_RATE), "piste d'écoute en 16 kHz");
        assert!((f.active_end - 20.0).abs() < 0.1, "{}", f.active_end);
    }

    #[test]
    fn fin_du_dernier_son() {
        let q = |db: f32, n: usize| vec![audio::quantize_db(db); n];
        // Micro : bruit de fond partout, parole de 10 à 70 s. PC : parole de 40 à 100 s, puis silence numérique.
        let mic = [q(-60.0, 500), q(-25.0, 3000), q(-60.0, 26_500)].concat();
        let pc = [q(-90.0, 2000), q(-20.0, 3000), q(-90.0, 25_000)].concat();
        let end = active_end(&[(Role::Mic, mic), (Role::Pc, pc)]);
        assert!((end - 100.0).abs() < 0.05, "{end}");
        assert_eq!(active_end(&[(Role::Pc, q(-90.0, 500))]), 0.0);
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
        let stopped = rec.stop().unwrap();
        let end = stopped.duration;
        // Tout a été produit pendant l'enregistrement : FLAC, enveloppes, écoute.
        assert!(stopped.streamed.tracks.iter().all(|t| t.flac_ok), "FLAC en direct complets");
        assert!((stopped.streamed.mix_seconds.unwrap() - end).abs() < 0.1);
        let t0 = Instant::now();
        let fast = finalize_streamed(&dir, &stopped.streamed).expect("finalisation immédiate");
        assert!(t0.elapsed() < Duration::from_millis(300), "finalisation immédiate : {:?}", t0.elapsed());
        assert_eq!(fast.playback, "ecoute.flac");
        // Le mélange en direct contient bien les deux voix, en 16 kHz.
        let mut mix = vec![];
        audio::decode_mono(&dir.join("ecoute.flac"), UPLOAD_RATE, |b| {
            mix.extend_from_slice(b);
            true
        })
        .unwrap();
        assert!((mix.len() as f64 / UPLOAD_RATE as f64 - end).abs() < 0.1);
        let at = |a: f64, b: f64| rms(&mix[(a * UPLOAD_RATE as f64) as usize..(b * UPLOAD_RATE as f64) as usize]);
        assert!(at(0.2, 0.8) > 0.02 && at(1.2, 1.8) > 0.02, "micro puis PC audibles : {} {}", at(0.2, 0.8), at(1.2, 1.8));
        // Même résultat que la finalisation complète depuis les WAV.
        let f = finalize(&dir, &[Role::Mic, Role::Pc], false, |_| {}).unwrap();
        assert!((f.duration - end).abs() < 0.1, "{} vs {}", f.duration, end);
        assert!((fast.duration - f.duration).abs() < 1e-6);
        for (a, b) in fast.tracks.iter().zip(&f.tracks) {
            assert!(a.role == b.role && a.file == b.file && (a.gain_db - b.gain_db).abs() < 0.5 && (a.activity.speech_ratio - b.activity.speech_ratio).abs() < 0.02, "{a:?} / {b:?}");
        }
        // Enveloppes identiques, à l'arrondi du WAV 16 bits près.
        for (a, b) in fast.envelopes.iter().zip(&f.envelopes) {
            assert!(a.0 == b.0 && a.1.len() == b.1.len() && a.1.iter().zip(&b.1).all(|(x, y)| x.abs_diff(*y) <= 1));
        }
        for t in &f.tracks {
            assert!(t.activity.speech_ratio > 0.3 && t.activity.speech_ratio < 0.7, "{t:?}");
        }
    }

    /// Une seule source : la piste d'écoute est quand même écrite en direct.
    #[test]
    fn une_seule_source_en_direct() {
        let dir = tmpdir("solo");
        let clock = Arc::new(SystemClock::new());
        let mic = SynthSource { clock: clock.clone(), device: "Micro".into(), rate: 48_000, amp: 0.3, seed: 3, script: |_| true, silent_gaps: false, switch_after: None };
        let rec = Recorder::start(&dir, clock.clone(), vec![(Role::Mic, Box::new(mic))]).unwrap();
        std::thread::sleep(Duration::from_millis(1200));
        let stopped = rec.stop().unwrap();
        let f = finalize_with(&dir, &[Role::Mic], false, Some(&stopped.streamed), |_| {}).unwrap();
        assert_eq!((f.playback.as_str(), f.tracks.len()), ("ecoute.flac", 1));
        assert!((audio::media_info(&dir.join("ecoute.flac")).unwrap().duration.unwrap() - stopped.duration).abs() < 0.1);
    }
}
