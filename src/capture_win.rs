//! Capture audio Windows (WASAPI, mode partagé).
//!
//! - Micro : périphérique d'entrée par défaut (suivi en direct) ou choisi.
//! - Son du PC : « capture par processus » en mode exclusion (tout le son du
//!   PC sauf Verbatim), indépendante de la sortie utilisée ; si Windows ne la
//!   propose pas (avant Windows 11), capture en boucle de la sortie par défaut,
//!   suivie en direct.
//!
//! Aucune lecture du micro n'est renvoyée vers les écouteurs : tu ne t'entends
//! pas. Un périphérique qui disparaît met la source en attente, la piste
//! continue en silence, et la capture reprend dès qu'un périphérique revient.

#![allow(non_snake_case)]

use crate::recorder::{Clock, Role, Source, SourceMsg, SourceState, SourceStatus, SystemClock};
use crossbeam_channel::Sender;
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use windows::core::{implement, Interface, GUID, HRESULT, PWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PropVariantToStringAlloc, PROPVARIANT};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, BLOB, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;

const TAG_PCM: u16 = 1;
const TAG_FLOAT: u16 = 3;
const TAG_EXTENSIBLE: u16 = 0xFFFE;
const SUB_PCM: GUID = GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71);
const SUB_FLOAT: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);
/// Tampon WASAPI : 200 ms, large marge contre les à-coups du système.
const BUFFER_HNS: i64 = 2_000_000;

// ---------------------------------------------------------------------------
// COM
// ---------------------------------------------------------------------------
struct Com(bool);

impl Com {
    fn init() -> Self {
        Com(unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok())
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

fn enumerator() -> windows::core::Result<IMMDeviceEnumerator> {
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
}

fn take_pwstr(p: PWSTR) -> String {
    let s = unsafe { p.to_string() }.unwrap_or_default();
    unsafe { CoTaskMemFree(Some(p.0 as _)) };
    s
}

fn device_id(d: &IMMDevice) -> String {
    unsafe { d.GetId() }.map(take_pwstr).unwrap_or_default()
}

fn device_name(d: &IMMDevice) -> String {
    unsafe {
        let Ok(store) = d.OpenPropertyStore(STGM_READ) else { return "Périphérique audio".into() };
        let Ok(mut pv) = store.GetValue(&PKEY_Device_FriendlyName) else { return "Périphérique audio".into() };
        let name = PropVariantToStringAlloc(&pv).map(take_pwstr).unwrap_or_default();
        let _ = PropVariantClear(&mut pv);
        if name.is_empty() {
            "Périphérique audio".into()
        } else {
            name
        }
    }
}

// ---------------------------------------------------------------------------
// Liste des périphériques
// ---------------------------------------------------------------------------
#[derive(Debug, Clone, Serialize)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub default: bool,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Devices {
    pub inputs: Vec<DeviceInfo>,
    pub outputs: Vec<DeviceInfo>,
}

pub fn list_devices() -> Devices {
    let _com = Com::init();
    let Ok(en) = enumerator() else { return Devices::default() };
    let list = |flow: EDataFlow| -> Vec<DeviceInfo> {
        let def = unsafe { en.GetDefaultAudioEndpoint(flow, eConsole) }.ok().map(|d| device_id(&d)).unwrap_or_default();
        let Ok(col) = (unsafe { en.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE) }) else { return vec![] };
        let n = unsafe { col.GetCount() }.unwrap_or(0);
        let mut v: Vec<DeviceInfo> = (0..n)
            .filter_map(|i| unsafe { col.Item(i) }.ok())
            .map(|d| {
                let id = device_id(&d);
                DeviceInfo { default: id == def, name: device_name(&d), id }
            })
            .collect();
        v.sort_by(|a, b| b.default.cmp(&a.default).then(a.name.cmp(&b.name)));
        v
    };
    Devices { inputs: list(eCapture), outputs: list(eRender) }
}

// ---------------------------------------------------------------------------
// Format des échantillons
// ---------------------------------------------------------------------------
#[derive(Debug, Clone, Copy, PartialEq)]
enum Sample {
    F32,
    I16,
    I24,
    I32,
}

#[derive(Debug, Clone, Copy)]
struct Fmt {
    rate: u32,
    channels: usize,
    sample: Sample,
    block: usize,
}

unsafe fn parse_format(p: *const WAVEFORMATEX) -> Option<Fmt> {
    let f = unsafe { std::ptr::read_unaligned(p) };
    let tag = f.wFormatTag;
    let bits = f.wBitsPerSample;
    let float = match tag {
        TAG_FLOAT => true,
        TAG_PCM => false,
        TAG_EXTENSIBLE => {
            let x = unsafe { std::ptr::read_unaligned(p as *const WAVEFORMATEXTENSIBLE) };
            let sub = x.SubFormat;
            if sub == SUB_FLOAT {
                true
            } else if sub == SUB_PCM {
                false
            } else {
                return None;
            }
        }
        _ => return None,
    };
    let sample = match (float, bits) {
        (true, 32) => Sample::F32,
        (false, 16) => Sample::I16,
        (false, 24) => Sample::I24,
        (false, 32) => Sample::I32,
        _ => return None,
    };
    Some(Fmt { rate: f.nSamplesPerSec, channels: f.nChannels.max(1) as usize, sample, block: f.nBlockAlign as usize })
}

/// Convertit un paquet entrelacé en mono f32.
fn to_mono(data: &[u8], frames: usize, fmt: &Fmt, out: &mut Vec<f32>) {
    let ch = fmt.channels;
    let bps = fmt.block / ch;
    let inv = 1.0 / ch as f32;
    for f in 0..frames {
        let mut acc = 0f32;
        for c in 0..ch {
            let o = f * fmt.block + c * bps;
            let b = &data[o..o + bps];
            acc += match fmt.sample {
                Sample::F32 => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
                Sample::I16 => i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0,
                Sample::I24 => (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8_388_608.0,
                Sample::I32 => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0,
            };
        }
        out.push(acc * inv);
    }
}

// ---------------------------------------------------------------------------
// Flux de capture
// ---------------------------------------------------------------------------
struct Stream {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    fmt: Fmt,
    event: Option<HANDLE>,
    name: String,
    /// Identifiant du périphérique ouvert (None pour la capture par processus).
    id: Option<String>,
}

impl Drop for Stream {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
            if let Some(e) = self.event {
                let _ = CloseHandle(e);
            }
        }
    }
}

fn open_endpoint(dev: &IMMDevice, loopback: bool) -> windows::core::Result<Stream> {
    unsafe {
        let client: IAudioClient = dev.Activate(CLSCTX_ALL, None)?;
        let mix = client.GetMixFormat()?;
        let fmt = parse_format(mix);
        let flags = if loopback { AUDCLNT_STREAMFLAGS_LOOPBACK } else { 0 };
        let res = client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, BUFFER_HNS, 0, mix, None);
        CoTaskMemFree(Some(mix as _));
        res?;
        let fmt = fmt.ok_or_else(|| windows::core::Error::new(HRESULT(0x88890008u32 as i32), "format audio non pris en charge"))?;
        let capture: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        Ok(Stream { client, capture, fmt, event: None, name: device_name(dev), id: Some(device_id(dev)) })
    }
}

#[implement(IActivateAudioInterfaceCompletionHandler, windows::Win32::System::Com::IAgileObject)]
struct Activation(Arc<(Mutex<bool>, Condvar)>);

impl windows::Win32::System::Com::IAgileObject_Impl for Activation_Impl {}

impl IActivateAudioInterfaceCompletionHandler_Impl for Activation_Impl {
    fn ActivateCompleted(&self, _op: windows::core::Ref<IActivateAudioInterfaceAsyncOperation>) -> windows::core::Result<()> {
        let (m, c) = &*self.0;
        *m.lock().unwrap() = true;
        c.notify_all();
        Ok(())
    }
}

/// Tout le son du PC sauf celui de Verbatim (Windows 10 build 20348 et plus).
fn open_process_loopback() -> windows::core::Result<Stream> {
    unsafe {
        let params = AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: std::process::id(),
                    ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
                },
            },
        };
        // Le blob pointe sur `params` (pile) : PropVariantClear ne doit jamais
        // le libérer, sinon corruption du tas. Le garde remet VT_EMPTY en sortie.
        struct BlobGuard(PROPVARIANT);
        impl Drop for BlobGuard {
            fn drop(&mut self) {
                unsafe {
                    let inner = &mut *self.0.Anonymous.Anonymous;
                    inner.vt = windows::Win32::System::Variant::VT_EMPTY;
                    inner.Anonymous.blob = BLOB::default();
                }
            }
        }
        let mut guard = BlobGuard(PROPVARIANT::default());
        let pv = &mut guard.0;
        {
            let inner = &mut *pv.Anonymous.Anonymous;
            inner.vt = VT_BLOB;
            inner.Anonymous.blob = BLOB {
                cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                pBlobData: &params as *const _ as *mut u8,
            };
        }
        let done = Arc::new((Mutex::new(false), Condvar::new()));
        let handler: IActivateAudioInterfaceCompletionHandler = Activation(done.clone()).into();
        let op = ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, &IAudioClient::IID, Some(&*pv), &handler)?;
        {
            let (m, c) = &*done;
            let g = m.lock().unwrap();
            let (g, _) = c.wait_timeout_while(g, Duration::from_secs(5), |d| !*d).unwrap();
            if !*g {
                return Err(windows::core::Error::new(HRESULT(0x800705B4u32 as i32), "délai d'activation dépassé"));
            }
        }
        let mut hr = HRESULT(0);
        let mut unk = None;
        op.GetActivateResult(&mut hr, &mut unk)?;
        hr.ok()?;
        let client: IAudioClient = unk.ok_or_else(|| windows::core::Error::new(HRESULT(0x80004005u32 as i32), "activation vide"))?.cast()?;
        let wf = WAVEFORMATEX {
            wFormatTag: TAG_PCM,
            nChannels: 2,
            nSamplesPerSec: 48_000,
            nAvgBytesPerSec: 48_000 * 4,
            nBlockAlign: 4,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            BUFFER_HNS,
            0,
            &wf,
            None,
        )?;
        let event = CreateEventW(None, false, false, None)?;
        let setup = (|| -> windows::core::Result<IAudioCaptureClient> {
            client.SetEventHandle(event)?;
            let capture: IAudioCaptureClient = client.GetService()?;
            client.Start()?;
            Ok(capture)
        })();
        let capture = match setup {
            Ok(c) => c,
            Err(e) => {
                let _ = CloseHandle(event);
                return Err(e);
            }
        };
        Ok(Stream {
            client,
            capture,
            fmt: Fmt { rate: 48_000, channels: 2, sample: Sample::I16, block: 4 },
            event: Some(event),
            name: "Tout le son du PC".into(),
            id: None,
        })
    }
}

/// Vide les paquets disponibles. Erreur = périphérique perdu.
/// Âge (secondes) d'un horodatage WASAPI exprimé en unités de 100 ns du
/// compteur de performance.
fn qpc_age(pos_100ns: u64) -> Option<f64> {
    use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
    if pos_100ns == 0 {
        return None;
    }
    let (mut now, mut freq) = (0i64, 0i64);
    unsafe {
        QueryPerformanceCounter(&mut now).ok()?;
        QueryPerformanceFrequency(&mut freq).ok()?;
    }
    if freq <= 0 {
        return None;
    }
    let now_100ns = (now as i128 * 10_000_000 / freq as i128) as f64;
    Some((now_100ns - pos_100ns as f64) / 1e7)
}

fn drain(s: &Stream, clock: &SystemClock, tx: &Sender<SourceMsg>, mono: &mut Vec<f32>) -> windows::core::Result<bool> {
    unsafe {
        match s.event {
            Some(e) => {
                let _ = WaitForSingleObject(e, 100) == WAIT_OBJECT_0;
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
        loop {
            let n = s.capture.GetNextPacketSize()?;
            if n == 0 {
                return Ok(true);
            }
            let mut data: *mut u8 = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            let mut qpc_pos = 0u64;
            s.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc_pos))?;
            mono.clear();
            let frames_us = frames as usize;
            if flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 || data.is_null() {
                mono.resize(frames_us, 0.0);
            } else {
                let bytes = std::slice::from_raw_parts(data, frames_us * s.fmt.block);
                to_mono(bytes, frames_us, &s.fmt, mono);
            }
            s.capture.ReleaseBuffer(frames)?;
            if frames > 0 {
                // Datation : instant de capture fourni par Windows (compteur haute
                // précision), sinon « maintenant moins la durée du paquet ». Un
                // paquet resté en attente garde ainsi sa vraie place.
                let fallback = clock.now() - frames as f64 / s.fmt.rate as f64;
                let at = match qpc_age(qpc_pos) {
                    Some(age) if (0.0..2.0).contains(&age) => clock.now() - age,
                    _ => fallback,
                };
                if tx.send(SourceMsg::Audio { samples: std::mem::take(mono), rate: s.fmt.rate, at }).is_err() {
                    return Ok(false);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Sources
// ---------------------------------------------------------------------------
#[derive(Debug, Clone)]
pub enum Target {
    /// Périphérique par défaut de Windows, suivi s'il change.
    Default,
    /// Périphérique choisi ; repli sur le défaut s'il est absent.
    Device(String, String),
    /// Tout le son du PC (son du PC uniquement).
    AllAudio,
}

pub struct WasapiSource {
    pub role: Role,
    pub target: Target,
    pub clock: Arc<SystemClock>,
}

/// Message lisible d'une erreur Windows, avec son code quand le texte manque.
fn describe(e: &windows::core::Error) -> String {
    let m = e.message().trim().to_string();
    let code = format!("0x{:08X}", e.code().0 as u32);
    if m.is_empty() { format!("erreur {code}") } else { format!("{m} [{code}]") }
}

impl WasapiSource {
    fn flow(&self) -> EDataFlow {
        match self.role {
            Role::Mic => eCapture,
            Role::Pc => eRender,
        }
    }

    /// Ouvre le flux voulu, avec les replis. Renvoie aussi un message d'information.
    fn open(&self, en: &IMMDeviceEnumerator, process_ok: &mut bool) -> Result<(Stream, Option<String>), String> {
        let loopback = self.role == Role::Pc;
        let default = || -> Result<Stream, String> {
            let d = unsafe { en.GetDefaultAudioEndpoint(self.flow(), eConsole) }
                .map_err(|_| if loopback { "Aucune sortie audio".to_string() } else { "Aucun micro branché".to_string() })?;
            open_endpoint(&d, loopback).map_err(|e| describe(&e))
        };
        match &self.target {
            Target::AllAudio if *process_ok => match open_process_loopback() {
                Ok(s) => Ok((s, None)),
                Err(e) => {
                    *process_ok = false;
                    default().map(|s| (s, Some(format!("Capture globale indisponible ({}), capture de la sortie par défaut.", describe(&e)))))
                }
            },
            Target::AllAudio | Target::Default => default().map(|s| (s, None)),
            Target::Device(id, name) => {
                let wide: Vec<u16> = id.encode_utf16().chain(Some(0)).collect();
                let dev = unsafe { en.GetDevice(windows::core::PCWSTR(wide.as_ptr())) };
                match dev.ok().and_then(|d| open_endpoint(&d, loopback).ok()) {
                    Some(s) => Ok((s, None)),
                    None => default().map(|s| (s, Some(format!("{name} indisponible, périphérique par défaut utilisé.")))),
                }
            }
        }
    }
}

impl Source for WasapiSource {
    fn run(self: Box<Self>, tx: Sender<SourceMsg>, stop: Arc<AtomicBool>) {
        let _com = Com::init();
        let status = |state, device: Option<String>, message: Option<String>| {
            let _ = tx.send(SourceMsg::Status(SourceStatus::new(state, device, message)));
        };
        let Ok(en) = enumerator() else {
            status(SourceState::Failed, None, Some("Service audio de Windows inaccessible.".into()));
            return;
        };
        let mut process_ok = true;
        let mut process_failed_at: Option<Instant> = None;
        let mut mono = Vec::with_capacity(4800);
        let mut announced_wait = false;
        while !stop.load(Ordering::SeqCst) {
            // Un échec de la capture « tout le son du PC » peut être passager :
            // on la retente à la réouverture suivante, au plus une fois par minute.
            if !process_ok {
                let t = *process_failed_at.get_or_insert_with(Instant::now);
                if t.elapsed() >= Duration::from_secs(60) {
                    process_ok = true;
                    process_failed_at = None;
                }
            }
            let (stream, info) = match self.open(&en, &mut process_ok) {
                Ok(x) => x,
                Err(msg) => {
                    if !announced_wait {
                        status(SourceState::Waiting, None, Some(msg));
                        announced_wait = true;
                    }
                    for _ in 0..10 {
                        if stop.load(Ordering::SeqCst) {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    continue;
                }
            };
            announced_wait = false;
            status(SourceState::Active, Some(stream.name.clone()), info);
            // Suivi du périphérique par défaut : vérifié chaque seconde.
            let follow = matches!(self.target, Target::Default) || (matches!(self.target, Target::AllAudio) && stream.id.is_some());
            let mut last_check = Instant::now();
            let lost = loop {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                match drain(&stream, &self.clock, &tx, &mut mono) {
                    Ok(true) => {}
                    Ok(false) => return,
                    Err(_) => break true,
                }
                if follow && last_check.elapsed() >= Duration::from_secs(1) {
                    last_check = Instant::now();
                    let def = unsafe { en.GetDefaultAudioEndpoint(self.flow(), eConsole) }.ok().map(|d| device_id(&d));
                    if def.is_some() && def != stream.id {
                        break false;
                    }
                }
            };
            drop(stream);
            status(
                SourceState::Waiting,
                None,
                Some(if lost { "Périphérique débranché, recherche d'un remplaçant…".into() } else { "Changement de périphérique…".into() }),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Diagnostic (Verbatim.exe --audio-test rapport.txt)
// ---------------------------------------------------------------------------
pub fn diagnose(seconds: f64) -> (Vec<String>, bool) {
    let mut out = vec![];
    let mut opened = vec![];
    let devs = list_devices();
    out.push(format!("Entrées ({}) :", devs.inputs.len()));
    for d in &devs.inputs {
        out.push(format!("  {} {}", if d.default { "*" } else { " " }, d.name));
    }
    out.push(format!("Sorties ({}) :", devs.outputs.len()));
    for d in &devs.outputs {
        out.push(format!("  {} {}", if d.default { "*" } else { " " }, d.name));
    }
    let clock = Arc::new(SystemClock::new());
    for (role, target, label) in [
        (Role::Mic, Target::Default, "Micro par défaut"),
        (Role::Pc, Target::AllAudio, "Tout le son du PC"),
        (Role::Pc, Target::Default, "Sortie par défaut (repli)"),
    ] {
        crate::trace(&format!("début : {label}"));
        let (tx, rx) = crossbeam_channel::unbounded();
        let stop = Arc::new(AtomicBool::new(false));
        let src = Box::new(WasapiSource { role, target, clock: clock.clone() });
        let stop2 = stop.clone();
        let h = std::thread::spawn(move || src.run(tx, stop2));
        std::thread::sleep(Duration::from_secs_f64(seconds));
        stop.store(true, Ordering::SeqCst);
        let _ = h.join();
        let (mut packets, mut samples, mut peak, mut rate) = (0usize, 0usize, 0f32, 0u32);
        let mut statuses = vec![];
        let mut active = false;
        for m in rx.try_iter() {
            match m {
                SourceMsg::Audio { samples: s, rate: r, .. } => {
                    packets += 1;
                    samples += s.len();
                    rate = r;
                    peak = s.iter().fold(peak, |p, x| p.max(x.abs()));
                }
                SourceMsg::Status(s) => statuses.push({ active |= s.state == SourceState::Active; format!(
                    "{:?}{}{}",
                    s.state,
                    s.device.map(|d| format!(" [{d}]")).unwrap_or_default(),
                    s.message.map(|m| format!(" ({m})")).unwrap_or_default()
                ) }),
            }
        }
        let secs = if rate > 0 { samples as f64 / rate as f64 } else { 0.0 };
        let peak_db = if peak > 0.0 { 20.0 * peak.log10() } else { -120.0 };
        out.push(format!(
            "{label} : {} | {packets} paquets, {secs:.2} s d'audio à {rate} Hz, crête {peak_db:.1} dBFS",
            statuses.join(" → ")
        ));
        if active && packets == 0 && role == Role::Pc {
            out.push("    (aucun paquet : normal si aucun son ne jouait pendant le test)".into());
        }
        opened.push(active);
    }
    let ok = opened.first().copied().unwrap_or(false) && opened.get(1).copied().unwrap_or(false);
    (out, ok)
}

/// Enregistrement réel de quelques secondes (micro + tout le son du PC), puis
/// finalisation FLAC : vérifie l'alignement des pistes et la chaîne complète.
pub fn record_check(seconds: f64) -> (Vec<String>, bool) {
    use crate::recorder::{finalize, Recorder};
    let mut out = vec![];
    let dir = std::env::temp_dir().join(format!("verbatim-rectest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let sys = Arc::new(SystemClock::new());
    let clock: Arc<dyn Clock> = sys.clone();
    let sources: Vec<(Role, Box<dyn Source>)> = vec![
        (Role::Mic, Box::new(WasapiSource { role: Role::Mic, target: Target::Default, clock: sys.clone() })),
        (Role::Pc, Box::new(WasapiSource { role: Role::Pc, target: Target::AllAudio, clock: sys.clone() })),
    ];
    crate::trace("début : enregistrement réel");
    let rec = match Recorder::start(&dir, clock, sources) {
        Ok(r) => r,
        Err(e) => return (vec![format!("Démarrage impossible : {e}")], false),
    };
    let mut seen: Vec<String> = vec![];
    let t0 = Instant::now();
    while t0.elapsed().as_secs_f64() < seconds {
        std::thread::sleep(Duration::from_millis(200));
        for t in rec.live().tracks {
            let line = format!(
                "{} : {:?}{}{}",
                t.role.label(),
                t.status.state,
                t.status.device.map(|d| format!(" [{d}]")).unwrap_or_default(),
                t.status.message.map(|m| format!(" ({m})")).unwrap_or_default()
            );
            if !seen.contains(&line) {
                out.push(format!("  {:>5.1} s  {line}", t0.elapsed().as_secs_f64()));
                seen.push(line);
            }
            if let Some(e) = t.error {
                out.push(format!("  erreur d'écriture : {e}"));
            }
        }
    }
    crate::trace("arrêt de l'enregistrement");
    let duration = match rec.stop() {
        Ok(d) => d,
        Err(e) => return (vec![format!("Arrêt impossible : {e}")], false),
    };
    let mut ok = (duration - seconds).abs() < 1.0;
    out.push(format!("Durée enregistrée : {duration:.2} s"));
    crate::trace("finalisation");
    match finalize(&dir, &[Role::Mic, Role::Pc], false, |_| {}) {
        Ok(f) => {
            for t in &f.tracks {
                let secs = std::fs::metadata(dir.join(&t.file)).map(|m| m.len()).unwrap_or(0);
                out.push(format!(
                    "  {} → {} ({} Ko), parole {:.0} %, niveau parole {:.1} dBFS, bruit {:.1} dBFS",
                    t.role.label(),
                    t.file,
                    secs / 1024,
                    t.activity.speech_ratio * 100.0,
                    t.activity.speech_db,
                    t.activity.floor_db
                ));
            }
            out.push(format!(
                "Pistes finalisées : {} pistes de {:.2} s, écho micro/PC {}",
                f.tracks.len(),
                f.duration,
                f.echo.map(|e| format!("{e:.2}")).unwrap_or_else(|| "non mesurable".into())
            ));
            ok &= f.tracks.len() == 2 && (f.duration - duration).abs() < 0.2;
        }
        Err(e) => {
            out.push(format!("Finalisation impossible : {e}"));
            ok = false;
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    (out, ok)
}
