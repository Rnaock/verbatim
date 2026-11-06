//! Logique de Verbatim, indépendante de Tauri pour pouvoir être testée seule.

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(test)]
use serde_json::json;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::vault;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const DEFAULT_API: &str = "https://api.elevenlabs.io";
pub const MAX_UPLOAD: u64 = 5 * 1024 * 1024 * 1024; // plafond ElevenLabs
pub const DEFAULT_CREDITS: f64 = 330.0;
pub const AUDIO_EXT: &[&str] = &[
    "m4a", "mp3", "mp4", "wav", "ogg", "opus", "flac", "webm", "aac", "mov", "mkv", "wma", "amr", "3gp",
];

// ---------------------------------------------------------------------------
// Erreurs renvoyées à l'interface
// ---------------------------------------------------------------------------
#[derive(Debug, Serialize, Clone)]
pub struct ApiError {
    /// Code HTTP d'ElevenLabs ; 0 = réseau, -1 = annulé, 4xx/5xx sinon.
    pub status: i32,
    pub message: String,
}

impl ApiError {
    pub fn new(status: i32, message: impl Into<String>) -> Self {
        Self { status, message: message.into() }
    }
}

impl From<io::Error> for ApiError {
    fn from(e: io::Error) -> Self {
        ApiError::new(500, e.to_string())
    }
}

pub type Res<T> = Result<T, ApiError>;

// ---------------------------------------------------------------------------
// Emplacements
// ---------------------------------------------------------------------------
#[derive(Clone, Debug)]
pub struct Paths {
    pub data_dir: PathBuf,
    /// Bibliothèque par défaut (Documents\\Verbatim).
    pub out_dir: PathBuf,
}

impl Paths {
    /// Mêmes dossiers que les versions Python : tout est repris tel quel.
    pub fn system() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        Paths {
            data_dir: dirs::data_local_dir().unwrap_or_else(|| home.clone()).join("Verbatim"),
            out_dir: dirs::document_dir().unwrap_or_else(|| home.join("Documents")).join("Verbatim"),
        }
    }
    pub fn vault(&self) -> PathBuf {
        self.data_dir.join("key.dpapi")
    }
    pub fn settings(&self) -> PathBuf {
        self.data_dir.join("settings.json")
    }
    pub fn library(&self, s: &Settings) -> PathBuf {
        if s.library_dir.trim().is_empty() {
            self.out_dir.clone()
        } else {
            PathBuf::from(s.library_dir.trim())
        }
    }
}

pub fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, data)?;
    // OneDrive, l'indexeur ou un antivirus peuvent tenir le fichier quelques
    // instants : on réessaie avant d'abandonner.
    let mut last = None;
    for attempt in 0..8 {
        match fs::rename(&tmp, path) {
            Ok(()) => return Ok(()),
            Err(e) => last = Some(e),
        }
        std::thread::sleep(std::time::Duration::from_millis(50 << attempt.min(4)));
    }
    let _ = fs::remove_file(&tmp);
    Err(last.unwrap())
}

// ---------------------------------------------------------------------------
// Clé API : elle vit ici et nulle part ailleurs
// ---------------------------------------------------------------------------
#[derive(Default)]
pub struct KeyStore {
    key: Option<String>,
    remember: bool,
}

#[derive(Serialize, Debug, Clone)]
pub struct KeyState {
    pub has_key: bool,
    pub masked: Option<String>,
    pub remember: bool,
}

impl KeyStore {
    pub fn load(paths: &Paths) -> Self {
        let read = || -> io::Result<String> {
            let raw = fs::read(paths.vault())?;
            let blob = B64
                .decode(raw.trim_ascii())
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            String::from_utf8(vault::unprotect(&blob)?).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
        };
        match read() {
            Ok(k) if !k.is_empty() => KeyStore { key: Some(k), remember: true },
            _ => KeyStore::default(),
        }
    }

    pub fn key(&self) -> Option<&str> {
        self.key.as_deref()
    }

    pub fn state(&self) -> KeyState {
        KeyState { has_key: self.key.is_some(), masked: self.key.as_deref().map(mask), remember: self.remember }
    }

    fn persist(&self, paths: &Paths) -> io::Result<()> {
        match (&self.key, self.remember) {
            (Some(k), true) => write_atomic(&paths.vault(), B64.encode(vault::protect(k.as_bytes())?).as_bytes()),
            _ => match fs::remove_file(paths.vault()) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            },
        }
    }

    pub fn update(&mut self, paths: &Paths, key: Option<String>, remember: Option<bool>) -> Res<KeyState> {
        if let Some(k) = key {
            let k = k.trim().to_string();
            if k.len() > 512 || k.chars().any(|c| !c.is_ascii_graphic()) {
                return Err(ApiError::new(400, "clé invalide"));
            }
            self.key = if k.is_empty() { None } else { Some(k) };
        }
        if let Some(r) = remember {
            self.remember = r;
        }
        self.persist(paths)?;
        Ok(self.state())
    }

    pub fn clear(&mut self, paths: &Paths) -> Res<KeyState> {
        self.key = None;
        self.remember = false;
        self.persist(paths)?;
        Ok(self.state())
    }
}

pub fn mask(k: &str) -> String {
    let c: Vec<char> = k.chars().collect();
    if c.len() > 10 {
        format!("{}…{}", c[..3].iter().collect::<String>(), c[c.len() - 4..].iter().collect::<String>())
    } else {
        format!("…{}", c[c.len().saturating_sub(2)..].iter().collect::<String>())
    }
}

// ---------------------------------------------------------------------------
// Réglages (aucun secret)
// ---------------------------------------------------------------------------
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Settings {
    // Transcription
    pub lang: String,
    pub speakers: String,
    pub no_verbatim: bool,
    pub keyterms: String,
    /// "auto" | "mixed" | "separate" : traitement des enregistrements à deux sources.
    pub mode: String,
    pub auto_transcribe: bool,
    pub credits_per_min: f64,
    pub names: Vec<String>,
    pub my_name: String,
    // Enregistrement
    pub rec_mic: bool,
    pub rec_pc: bool,
    /// "default" (suit Windows) ou identifiant de périphérique.
    pub mic_device: String,
    pub mic_device_name: String,
    /// "all" (tout le son du PC), "default" (sortie par défaut) ou identifiant.
    pub pc_device: String,
    pub pc_device_name: String,
    /// Dossier de la bibliothèque ; vide = Documents\Verbatim.
    pub library_dir: String,
    /// Thème : « auto » (celui de Windows), « light » ou « dark ».
    #[serde(default = "default_theme")]
    pub theme: String,
    /// Annuaire : adresse e-mail (minuscules) → prénom ou nom affiché.
    #[serde(default)]
    pub people: std::collections::BTreeMap<String, String>,
}

fn default_theme() -> String {
    "auto".into()
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            lang: "fr".into(),
            speakers: "auto".into(),
            no_verbatim: false,
            keyterms: String::new(),
            mode: "auto".into(),
            auto_transcribe: true,
            credits_per_min: DEFAULT_CREDITS,
            names: vec![],
            my_name: "Moi".into(),
            rec_mic: true,
            rec_pc: true,
            mic_device: "default".into(),
            mic_device_name: String::new(),
            pc_device: "all".into(),
            pc_device_name: String::new(),
            library_dir: String::new(),
            people: Default::default(),
            theme: default_theme(),
        }
    }
}

fn valid_lang(v: &str) -> bool {
    v == "auto" || ((2..=3).contains(&v.len()) && v.chars().all(|c| c.is_ascii_lowercase()))
}

fn valid_speakers(v: &str) -> bool {
    v == "auto" || matches!(v, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8")
}

/// N'applique que les champs valides ; le reste est ignoré sans bruit.
pub fn apply_patch(s: &mut Settings, raw: &Value) {
    let Some(o) = raw.as_object() else { return };
    if let Some(v) = o.get("lang").and_then(Value::as_str).filter(|v| valid_lang(v)) {
        s.lang = v.into();
    }
    if let Some(v) = o.get("speakers").and_then(Value::as_str).filter(|v| valid_speakers(v)) {
        s.speakers = v.into();
    }
    if let Some(v) = o.get("no_verbatim").and_then(Value::as_bool) {
        s.no_verbatim = v;
    }
    if let Some(v) = o.get("keyterms").and_then(Value::as_str).filter(|v| v.len() <= 20_000) {
        s.keyterms = v.into();
    }
    if let Some(v) = o.get("credits_per_min").and_then(Value::as_f64).filter(|v| (50.0..=3000.0).contains(v)) {
        s.credits_per_min = v;
    }
    if let Some(v) = o.get("names").and_then(Value::as_array) {
        s.names = v
            .iter()
            .filter_map(Value::as_str)
            .map(|n| n.trim().chars().take(40).collect::<String>())
            .filter(|n| !n.is_empty())
            .take(30)
            .collect();
    }
    if let Some(v) = o.get("mode").and_then(Value::as_str).filter(|v| matches!(*v, "auto" | "mixed" | "separate")) {
        s.mode = v.into();
    }
    for (k, f) in [("auto_transcribe", &mut s.auto_transcribe), ("rec_mic", &mut s.rec_mic), ("rec_pc", &mut s.rec_pc)] {
        if let Some(v) = o.get(k).and_then(Value::as_bool) {
            *f = v;
        }
    }
    if let Some(v) = o.get("my_name").and_then(Value::as_str) {
        let n: String = v.trim().chars().filter(|c| !c.is_control()).take(40).collect();
        s.my_name = if n.is_empty() { "Moi".into() } else { n };
    }
    let dev = |v: &str| v.len() <= 512 && !v.chars().any(char::is_control);
    for (k, f) in [("mic_device", &mut s.mic_device), ("pc_device", &mut s.pc_device), ("mic_device_name", &mut s.mic_device_name), ("pc_device_name", &mut s.pc_device_name)] {
        if let Some(v) = o.get(k).and_then(Value::as_str).filter(|v| dev(v)) {
            *f = v.into();
        }
    }
    if s.mic_device.is_empty() {
        s.mic_device = "default".into();
    }
    if s.pc_device.is_empty() {
        s.pc_device = "all".into();
    }
    if let Some(v) = o.get("library_dir").and_then(Value::as_str).filter(|v| dev(v)) {
        s.library_dir = v.into();
    }
    if let Some(v) = o.get("theme").and_then(Value::as_str).filter(|v| ["auto", "light", "dark"].contains(v)) {
        s.theme = v.into();
    }
    if let Some(v) = o.get("people").and_then(Value::as_object) {
        s.people = v
            .iter()
            .filter_map(|(email, name)| {
                let email = email.trim().to_lowercase();
                let name: String = name.as_str()?.trim().chars().filter(|c| !c.is_control()).take(40).collect();
                (email.contains('@') && email.len() <= 254 && !name.is_empty()).then_some((email, name))
            })
            .take(1000)
            .collect();
    }
}

pub fn load_settings(paths: &Paths) -> Settings {
    let mut s = Settings::default();
    if let Ok(raw) = fs::read(paths.settings()) {
        if let Ok(v) = serde_json::from_slice::<Value>(&raw) {
            apply_patch(&mut s, &v);
        }
    }
    s
}

pub fn save_settings(paths: &Paths, patch: &Value) -> Res<Settings> {
    let mut s = load_settings(paths);
    apply_patch(&mut s, patch);
    let data = serde_json::to_vec_pretty(&s).map_err(|e| ApiError::new(500, e.to_string()))?;
    write_atomic(&paths.settings(), &data)?;
    Ok(s)
}

// ---------------------------------------------------------------------------
// Noms de fichiers
// ---------------------------------------------------------------------------
pub fn safe_stem(name: &str) -> String {
    let base = name.replace('\\', "/");
    let base = base.rsplit('/').next().unwrap_or("");
    let stem = match base.rfind('.') {
        Some(i) if i > 0 => &base[..i],
        _ => base,
    };
    let cleaned: String = stem
        .chars()
        .map(|c| if "<>:\"/\\|?*".contains(c) || (c as u32) < 0x20 { '_' } else { c })
        .collect();
    let cleaned: String = cleaned.trim_matches(|c: char| c == ' ' || c == '.').chars().take(80).collect();
    let cleaned = if cleaned.is_empty() { "transcription".to_string() } else { cleaned };
    let low = cleaned.to_ascii_lowercase();
    let reserved = ["con", "prn", "aux", "nul"].contains(&low.as_str())
        || ((low.starts_with("com") || low.starts_with("lpt"))
            && low.len() == 4
            && low.as_bytes()[3].is_ascii_digit());
    if reserved {
        format!("_{cleaned}")
    } else {
        cleaned
    }
}

/// Un nom propre pour un fichier exporté : le radical nettoyé, plus l'extension.
pub fn safe_file_name(name: &str, ext: &str) -> String {
    format!("{}.{}", safe_stem(&format!("{name}.x")), ext)
}

pub fn unique_stem(out_dir: &Path, stem: &str, now: chrono::DateTime<chrono::Local>) -> String {
    let base = format!("{} {}", now.format("%Y-%m-%d %Hh%M"), stem);
    let mut cand = base.clone();
    let mut n = 2;
    while out_dir.join(format!("{cand}.json")).exists() {
        cand = format!("{base} ({n})");
        n += 1;
    }
    cand
}

/// Un radical renvoyé par la page doit désigner un fichier de OUT_DIR, rien d'autre.
pub fn checked_stem(stem: &str) -> Res<&str> {
    if stem.is_empty() || safe_stem(&format!("{stem}.x")) != stem {
        return Err(ApiError::new(400, "nom invalide"));
    }
    Ok(stem)
}

// ---------------------------------------------------------------------------
// Vocabulaire spécifique (mêmes règles que l'interface, revérifiées ici)
// ---------------------------------------------------------------------------
pub fn filter_keyterms(raw: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = vec![];
    for t in raw {
        let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
        if t.is_empty()
            || t.chars().count() >= 50
            || t.split(' ').count() > 5
            || t.chars().any(|c| "<>{}[]\\".contains(c))
        {
            continue;
        }
        if seen.insert(t.to_lowercase()) {
            out.push(t);
        }
        if out.len() >= 100 {
            break;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// ElevenLabs
// ---------------------------------------------------------------------------
pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(format!("Verbatim/{VERSION}"))
        .connect_timeout(Duration::from_secs(20))
        .build()
        .expect("client HTTP")
}

fn detail(status: u16, body: &[u8]) -> String {
    if let Ok(v) = serde_json::from_slice::<Value>(body) {
        let d = &v["detail"];
        if let Some(m) = d.get("message").and_then(Value::as_str) {
            // Le code « quota_exceeded » sert à reconnaître le manque de crédits.
            return match d.get("status").and_then(Value::as_str).filter(|c| c.contains("quota")) {
                Some(code) => format!("{m} [{code}]"),
                None => m.into(),
            };
        }
        if let Some(m) = d.as_str() {
            return m.into();
        }
        if !d.is_null() {
            return d.to_string().chars().take(300).collect();
        }
    }
    let t: String = String::from_utf8_lossy(body).chars().take(300).collect();
    if t.trim().is_empty() {
        format!("HTTP {status}")
    } else {
        t
    }
}

fn net_err(e: reqwest::Error) -> ApiError {
    ApiError::new(0, format!("ElevenLabs injoignable : {e}"))
}

#[derive(Serialize, Debug)]
pub struct Balance {
    pub count: Option<f64>,
    pub limit: Option<f64>,
    pub tier: Option<String>,
    pub reset: Option<i64>,
}

pub async fn balance(client: &reqwest::Client, api: &str, key: &str) -> Res<Balance> {
    let r = client
        .get(format!("{api}/v1/user/subscription"))
        .header("xi-api-key", key)
        .header("Accept", "application/json")
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(net_err)?;
    let status = r.status().as_u16();
    let body = r.bytes().await.map_err(net_err)?;
    if status != 200 {
        return Err(ApiError::new(status as i32, detail(status, &body)));
    }
    let d: Value = serde_json::from_slice(&body).map_err(|e| ApiError::new(502, e.to_string()))?;
    Ok(Balance {
        count: d["character_count"].as_f64(),
        limit: d["character_limit"].as_f64(),
        tier: d["tier"].as_str().map(String::from),
        reset: d["next_character_count_reset_unix"].as_i64(),
    })
}

impl Balance {
    /// Crédits restants, si ElevenLabs les donne.
    pub fn remaining(&self) -> Option<f64> {
        Some((self.limit? - self.count?).max(0.0))
    }
}

/// Coût estimé en crédits : minutes facturées × tarif, +20 % avec vocabulaire.
pub fn estimate_credits(billed_seconds: f64, credits_per_min: f64, keyterms: bool) -> f64 {
    billed_seconds / 60.0 * credits_per_min * if keyterms { KEYTERM_FACTOR } else { 1.0 }
}
pub const KEYTERM_FACTOR: f64 = 1.2;

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
pub struct Options {
    pub lang: String,
    pub speakers: String,
    pub no_verbatim: bool,
    #[serde(default)]
    pub keyterms: Vec<String>,
}

impl Options {
    pub fn from_settings(s: &Settings) -> Self {
        Options {
            lang: s.lang.clone(),
            speakers: s.speakers.clone(),
            no_verbatim: s.no_verbatim,
            keyterms: s.keyterms.split(['\n', ',', ';']).map(|t| t.trim().to_string()).collect(),
        }
    }
}

/// Envoie un fichier à Scribe v2 en flux continu et rend la réponse brute.
/// `progress(envoyé, total)` est appelé au fil de l'envoi.
pub async fn transcribe_file<F>(client: &reqwest::Client, api: &str, key: &str, path: &Path, opt: &Options, progress: F) -> Res<Value>
where
    F: Fn(u64, u64) + Send + Sync + 'static,
{
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "audio".into());
    let file = tokio::fs::File::open(path).await.map_err(|e| ApiError::new(404, format!("fichier inaccessible : {e}")))?;
    let len = file.metadata().await?.len();
    if len == 0 {
        return Err(ApiError::new(400, "fichier vide"));
    }
    if len > MAX_UPLOAD {
        return Err(ApiError::new(413, "fichier au-delà de 5 Go, la limite d'ElevenLabs"));
    }
    let mut sent = 0u64;
    let stream = tokio_util::io::ReaderStream::with_capacity(file, 64 * 1024).map(move |chunk| {
        if let Ok(c) = &chunk {
            sent += c.len() as u64;
            progress(sent, len);
        }
        chunk
    });
    let part = reqwest::multipart::Part::stream_with_length(reqwest::Body::wrap_stream(stream), len)
        .file_name(name)
        .mime_str("application/octet-stream")
        .map_err(net_err)?;
    let mut form = reqwest::multipart::Form::new()
        .part("file", part)
        .text("model_id", "scribe_v2")
        .text("diarize", "true")
        .text("tag_audio_events", "false")
        .text("timestamps_granularity", "word");
    if opt.lang != "auto" && valid_lang(&opt.lang) {
        form = form.text("language_code", opt.lang.clone());
    }
    if opt.speakers != "auto" && valid_speakers(&opt.speakers) {
        form = form.text("num_speakers", opt.speakers.clone());
    }
    if opt.no_verbatim {
        form = form.text("no_verbatim", "true");
    }
    for k in filter_keyterms(&opt.keyterms) {
        form = form.text("keyterms", k);
    }
    let r = client
        .post(format!("{api}/v1/speech-to-text"))
        .header("xi-api-key", key)
        .header("Accept", "application/json")
        .multipart(form)
        .timeout(Duration::from_secs(3 * 3600))
        .send()
        .await
        .map_err(net_err)?;
    let status = r.status().as_u16();
    let body = r.bytes().await.map_err(net_err)?;
    if status != 200 {
        return Err(ApiError::new(status as i32, detail(status, &body)));
    }
    serde_json::from_slice(&body).map_err(|e| ApiError::new(502, format!("réponse illisible : {e}")))
}

pub fn write_text_file(path: &Path, text: &str) -> Res<()> {
    let mut data = "\u{feff}".as_bytes().to_vec();
    data.extend_from_slice(text.as_bytes());
    write_atomic(path, &data)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    fn tmp_paths(tag: &str) -> Paths {
        let base = std::env::temp_dir().join(format!("vbt-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        Paths { data_dir: base.join("data"), out_dir: base.join("out") }
    }

    /// Fausse API ElevenLabs : enregistre les requêtes, répond selon le chemin.
    fn mock(responses: Vec<(u16, String)>) -> (String, Arc<Mutex<Vec<(String, String, Vec<u8>)>>>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = format!("http://{}", l.local_addr().unwrap());
        let log = Arc::new(Mutex::new(vec![]));
        let log2 = log.clone();
        std::thread::spawn(move || {
            for (stream, (code, body)) in l.incoming().zip(responses.into_iter()) {
                let mut s = stream.unwrap();
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut line = String::new();
                r.read_line(&mut line).unwrap();
                let (mut len, mut key, mut chunked) = (0usize, String::new(), false);
                loop {
                    let mut h = String::new();
                    r.read_line(&mut h).unwrap();
                    if h == "\r\n" {
                        break;
                    }
                    let low = h.to_ascii_lowercase();
                    if let Some(v) = low.strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                    if low.starts_with("transfer-encoding:") && low.contains("chunked") {
                        chunked = true;
                    }
                    if let Some(v) = h.strip_prefix("xi-api-key:") {
                        key = v.trim().into();
                    }
                }
                let mut body_in = vec![0; len];
                r.read_exact(&mut body_in).unwrap();
                assert!(!chunked, "le corps doit avoir une longueur connue");
                log2.lock().unwrap().push((line.trim().into(), key, body_in));
                write!(s, "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        (addr, log)
    }

    #[test]
    fn noms_de_fichiers() {
        assert_eq!(safe_stem("C:\\Users\\m\\entretien partie 1.m4a"), "entretien partie 1");
        assert_eq!(safe_stem("a<b>c:d?.wav"), "a_b_c_d_");
        assert_eq!(safe_stem("CON.mp3"), "_CON");
        assert_eq!(safe_stem("com7.wav"), "_com7");
        assert_eq!(safe_stem("...."), "transcription");
        assert!(checked_stem("../../evil").is_err());
        assert!(checked_stem("2026-10-12 20h42 entretien").is_ok());
        assert_eq!(safe_file_name("01_a/b", "txt"), "b.txt");
    }

    #[test]
    fn masque() {
        assert_eq!(mask("sk_test_KEY1234567890abcd"), "sk_…abcd");
        assert_eq!(mask("court"), "…rt");
    }

    #[test]
    fn vocabulaire() {
        let raw: Vec<String> = ["CeSIA", "cesia", "mot<x>", "un deux trois quatre cinq six", "  Claire  ", ""]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(filter_keyterms(&raw), vec!["CeSIA", "Claire"]);
        let many: Vec<String> = (0..150).map(|i| format!("terme{i}")).collect();
        assert_eq!(filter_keyterms(&many).len(), 100);
    }

    #[test]
    fn coffre_aller_retour_et_compatibilite_v2() {
        let p = tmp_paths("vault");
        let mut k = KeyStore::default();
        let st = k.update(&p, Some("sk_test_KEY1234567890abcd".into()), Some(true)).unwrap();
        assert!(st.has_key && st.remember);
        let raw = fs::read(p.vault()).unwrap();
        assert!(!String::from_utf8_lossy(&raw).contains("sk_test"), "pas de clé en clair sur le disque");
        assert_eq!(KeyStore::load(&p).key(), Some("sk_test_KEY1234567890abcd"));
        // Format de la v2 Python : base64 du blob protégé.
        let v2 = B64.encode(vault::protect(b"sk_v2_ancienne_cle_1234").unwrap());
        fs::write(p.vault(), v2).unwrap();
        assert_eq!(KeyStore::load(&p).key(), Some("sk_v2_ancienne_cle_1234"));
        k.update(&p, None, Some(false)).unwrap();
        assert!(!p.vault().exists(), "décocher retire le fichier");
        assert!(k.update(&p, Some("a b".into()), None).is_err());
    }

    #[test]
    fn reglages_valides_et_persistes() {
        let p = tmp_paths("settings");
        let s = save_settings(&p, &json!({"speakers": "2", "no_verbatim": true, "lang": "xx-YY", "credits_per_min": 9,
            "mode": "separate", "rec_pc": false, "my_name": "  Manuel ", "mic_device": "{0.0.1.00000000}.{abc}", "pc_device": ""})).unwrap();
        assert_eq!((s.speakers.as_str(), s.no_verbatim, s.mode.as_str(), s.rec_pc), ("2", true, "separate", false));
        assert_eq!(s.lang, "fr", "langue invalide ignorée");
        assert_eq!(s.credits_per_min, DEFAULT_CREDITS, "taux hors bornes ignoré");
        assert_eq!(s.my_name, "Manuel");
        assert_eq!(s.pc_device, "all", "vide = valeur par défaut");
        assert_eq!(load_settings(&p), s);
        assert_eq!(p.library(&s), p.out_dir);
        let o = Options::from_settings(&save_settings(&p, &json!({"keyterms": "CeSIA\nClaire, Tangui"})).unwrap());
        assert_eq!(filter_keyterms(&o.keyterms), vec!["CeSIA", "Claire", "Tangui"]);
    }

    #[tokio::test]
    async fn envoi_multipart_et_erreurs() {
        let p = tmp_paths("api");
        fs::create_dir_all(&p.data_dir).unwrap();
        let audio = p.data_dir.join("entretien partie 1.flac");
        fs::write(&audio, vec![7u8; 300_000]).unwrap();
        let reply = json!({"text": "Bonjour", "words": []});
        let (api, log) = mock(vec![
            (200, reply.to_string()),
            (429, r#"{"detail":{"status":"too_many_concurrent_requests","message":"Too many"}}"#.into()),
            (401, r#"{"detail":{"status":"invalid_api_key","message":"Invalid API key"}}"#.into()),
        ]);
        let seen = Arc::new(Mutex::new((0u64, 0u64)));
        let s2 = seen.clone();
        let opt = Options { lang: "fr".into(), speakers: "2".into(), no_verbatim: true, keyterms: vec!["CeSIA".into(), "x<y".into()] };
        let r = transcribe_file(&http_client(), &api, "sk_good", &audio, &opt, move |a, b| *s2.lock().unwrap() = (a, b)).await.unwrap();
        assert_eq!(r["text"], "Bonjour");
        assert_eq!(*seen.lock().unwrap(), (300_000, 300_000));
        let (line, key, body) = log.lock().unwrap()[0].clone();
        assert_eq!((line.as_str(), key.as_str()), ("POST /v1/speech-to-text HTTP/1.1", "sk_good"));
        let b = String::from_utf8_lossy(&body);
        for (field, val) in [("model_id", "scribe_v2"), ("diarize", "true"), ("num_speakers", "2"), ("no_verbatim", "true"), ("language_code", "fr"), ("keyterms", "CeSIA")] {
            assert!(b.contains(&format!("name=\"{field}\"\r\n\r\n{val}\r\n")), "champ {field}");
        }
        assert!(!b.contains("x<y"));
        let e = transcribe_file(&http_client(), &api, "k", &audio, &opt, |_, _| {}).await.unwrap_err();
        assert_eq!(e.status, 429);
        assert!(e.message.starts_with("Too many"), "{}", e.message);
        let e = balance(&http_client(), &api, "k").await.unwrap_err();
        assert_eq!((e.status, e.message.as_str()), (401, "Invalid API key"));
        let e = transcribe_file(&http_client(), "http://127.0.0.1:9", "k", &audio, &opt, |_, _| {}).await.unwrap_err();
        assert_eq!(e.status, 0, "réseau coupé → statut 0, relançable");
        let e = transcribe_file(&http_client(), &api, "k", Path::new("/nexiste/pas.wav"), &opt, |_, _| {}).await.unwrap_err();
        assert_eq!(e.status, 404);
    }
}
