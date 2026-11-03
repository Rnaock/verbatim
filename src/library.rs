//! Bibliothèque : un dossier par conversation, lisible même sans Verbatim.
//!
//! ```text
//! Documents\Verbatim\
//!   2026-10-21 15h40 Appel avec Claire\
//!     item.json            métadonnées
//!     micro.flac, pc.flac  pistes d'origine (enregistrements)
//!     ecoute.flac          les deux pistes mélangées, pour réécouter
//!     envelope.json        niveaux toutes les 20 ms (frise, attribution)
//!     transcription.txt    texte à jour, noms compris
//!     transcriptions\2026-10-21 15h52\
//!       transcript.json    modèle Verbatim (locuteurs, segments)
//!       brut-micro.json    réponses ElevenLabs, intactes
//! ```

use crate::audio::ENV_FPS;
use crate::core::{safe_stem, write_atomic};
use crate::recorder::{self, Role, TrackInfo};
use crate::transcript::{self, Mode, Transcript};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const ITEM_FILE: &str = "item.json";
pub const TRASH: &str = ".corbeille";
pub const CACHE: &str = ".cache";
pub const TRANSCRIPTIONS: &str = "transcriptions";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Recording,
    Import,
    Legacy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// Enregistrement en cours.
    Recording,
    /// Conversion des pistes après l'arrêt.
    Finalizing,
    /// Import en cours (copie, extraction de l'audio).
    Importing,
    Ready,
    Failed,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourceFile {
    pub name: String,
    /// Chemin d'origine au moment de l'import.
    pub original: String,
    /// Copie conservée dans le dossier, si le fichier a été copié.
    pub file: Option<String>,
    pub size: u64,
    pub video: bool,
    /// Lisible par Verbatim (sinon il part tel quel chez ElevenLabs).
    pub decodable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptRef {
    pub dir: String,
    pub created: String,
    pub mode: Mode,
    pub billed_seconds: f64,
    pub options: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobInfo {
    /// queued | preparing | uploading | processing | error
    pub state: String,
    pub message: Option<String>,
    pub progress: Option<f32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub v: u32,
    pub id: String,
    pub title: String,
    pub created: String,
    pub kind: Kind,
    pub state: State,
    pub duration: Option<f64>,
    #[serde(default)]
    pub roles: Vec<Role>,
    #[serde(default)]
    pub tracks: Vec<TrackInfo>,
    #[serde(default)]
    pub devices: BTreeMap<String, String>,
    /// Fichier à écouter : relatif au dossier, ou chemin absolu (original non copié).
    pub playback: Option<String>,
    pub source: Option<SourceFile>,
    pub echo: Option<f32>,
    #[serde(default)]
    pub recovered: bool,
    #[serde(default)]
    pub transcripts: Vec<TranscriptRef>,
    pub current: Option<String>,
    pub job: Option<JobInfo>,
    pub error: Option<String>,
    #[serde(default)]
    pub notes: Vec<String>,
    /// Rendez-vous de l'agenda associé (titre, participants, lien de visio).
    #[serde(default)]
    pub event: Option<crate::calendar::CalEvent>,
}

/// Texte indexé pour la recherche, hors transcription : titre et participants.
fn head_key(item: &Item) -> String {
    let mut t = item.title.clone();
    if let Some(e) = &item.event {
        for p in &e.people {
            t.push(' ');
            t.push_str(&p.name);
            t.push(' ');
            t.push_str(&p.email);
        }
    }
    fold(&t)
}

impl Item {
    fn new(id: String, title: String, kind: Kind, state: State) -> Self {
        Item {
            v: 4,
            id,
            title,
            created: chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
            kind,
            state,
            duration: None,
            roles: vec![],
            tracks: vec![],
            devices: BTreeMap::new(),
            playback: None,
            source: None,
            echo: None,
            recovered: false,
            transcripts: vec![],
            current: None,
            job: None,
            error: None,
            notes: vec![],
            event: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub id: String,
    pub title: String,
    pub created: String,
    pub kind: Kind,
    pub state: State,
    pub duration: Option<f64>,
    pub roles: Vec<Role>,
    pub speakers: Vec<String>,
    pub has_transcript: bool,
    pub job: Option<JobInfo>,
    pub snippet: String,
    pub recovered: bool,
    pub error: Option<String>,
    /// Titre du rendez-vous lié, s'il y en a un.
    pub event: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Envelopes {
    pub fps: u32,
    /// "micro", "pc" ou "audio" → niveaux en base64
    pub tracks: BTreeMap<String, String>,
}

impl Envelopes {
    pub fn get(&self, k: &str) -> Option<Vec<u8>> {
        self.tracks.get(k).and_then(|b| B64.decode(b).ok())
    }
    pub fn set(&mut self, k: &str, v: &[u8]) {
        self.fps = ENV_FPS;
        self.tracks.insert(k.into(), B64.encode(v));
    }
}

pub struct Library {
    pub root: PathBuf,
    lock: Mutex<()>,
    /// id → (titre, texte) en minuscules, pour la recherche.
    index: Mutex<HashMap<String, (String, String)>>,
}

fn now_stamp() -> String {
    chrono::Local::now().format("%Y-%m-%d %Hh%M").to_string()
}

pub fn fold(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| match c {
            'à' | 'â' | 'ä' | 'á' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'î' | 'ï' | 'í' => 'i',
            'ô' | 'ö' | 'ó' => 'o',
            'ù' | 'û' | 'ü' | 'ú' => 'u',
            'ç' => 'c',
            'œ' => 'o',
            other => other,
        })
        .collect()
}

impl Library {
    pub fn open(root: &Path) -> io::Result<Self> {
        fs::create_dir_all(root)?;
        Ok(Library { root: root.to_path_buf(), lock: Mutex::new(()), index: Mutex::new(HashMap::new()) })
    }

    /// Dossier d'une conversation ; refuse tout identifiant qui sortirait de la bibliothèque.
    pub fn dir(&self, id: &str) -> io::Result<PathBuf> {
        if id.is_empty() || id.starts_with('.') || id.contains(['/', '\\', ':']) || id.contains("..") {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "identifiant invalide"));
        }
        let d = self.root.join(id);
        if !d.join(ITEM_FILE).is_file() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "conversation introuvable"));
        }
        Ok(d)
    }

    pub fn create(&self, title: &str, kind: Kind, state: State) -> io::Result<Item> {
        let _g = self.lock.lock().unwrap();
        let clean: String = safe_stem(&format!("{}.x", title.trim())).chars().take(60).collect();
        let base = format!("{} {}", now_stamp(), clean).trim().to_string();
        let mut id = base.clone();
        let mut n = 2;
        while self.root.join(&id).exists() {
            id = format!("{base} ({n})");
            n += 1;
        }
        fs::create_dir_all(self.root.join(&id))?;
        let item = Item::new(id, title.trim().chars().take(120).collect(), kind, state);
        self.write(&item)?;
        Ok(item)
    }

    fn write(&self, item: &Item) -> io::Result<()> {
        let data = serde_json::to_vec_pretty(item).map_err(io::Error::other)?;
        write_atomic(&self.root.join(&item.id).join(ITEM_FILE), &data)
    }

    pub fn load(&self, id: &str) -> io::Result<Item> {
        let d = self.dir(id)?;
        let mut item: Item = serde_json::from_slice(&fs::read(d.join(ITEM_FILE))?).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        item.id = id.to_string();
        Ok(item)
    }

    /// Lecture, modification, écriture atomiques.
    pub fn update<F: FnOnce(&mut Item)>(&self, id: &str, f: F) -> io::Result<Item> {
        let _g = self.lock.lock().unwrap();
        let mut item = self.load(id)?;
        f(&mut item);
        self.write(&item)?;
        if let Ok(mut ix) = self.index.lock() {
            if let Some(e) = ix.get_mut(id) {
                e.0 = head_key(&item);
            }
        }
        Ok(item)
    }

    pub fn ids(&self) -> Vec<String> {
        let Ok(rd) = fs::read_dir(&self.root) else { return vec![] };
        rd.filter_map(|e| e.ok())
            .filter(|e| e.path().join(ITEM_FILE).is_file())
            .filter_map(|e| e.file_name().to_str().map(String::from))
            .filter(|n| !n.starts_with('.'))
            .collect()
    }

    pub fn current_transcript(&self, item: &Item) -> Option<Transcript> {
        let cur = item.current.as_ref()?;
        let p = self.root.join(&item.id).join(cur).join("transcript.json");
        serde_json::from_slice(&fs::read(p).ok()?).ok()
    }

    pub fn summary(&self, item: &Item) -> Summary {
        let t = self.current_transcript(item);
        let (speakers, snippet) = match &t {
            Some(t) => {
                let s: String = t.plain_text().chars().take(160).collect();
                if let Ok(mut ix) = self.index.lock() {
                    ix.insert(item.id.clone(), (head_key(&item), fold(&t.plain_text())));
                }
                (t.speakers.iter().map(|s| s.name.clone()).collect(), s)
            }
            None => {
                if let Ok(mut ix) = self.index.lock() {
                    ix.insert(item.id.clone(), (head_key(&item), String::new()));
                }
                (vec![], String::new())
            }
        };
        Summary {
            id: item.id.clone(),
            title: item.title.clone(),
            created: item.created.clone(),
            kind: item.kind,
            state: item.state,
            duration: item.duration,
            roles: if item.tracks.is_empty() { item.roles.clone() } else { item.tracks.iter().map(|t| t.role).collect() },
            speakers,
            has_transcript: t.is_some(),
            job: item.job.clone(),
            snippet,
            recovered: item.recovered,
            error: item.error.clone(),
            event: item.event.as_ref().map(|e| e.title.clone()),
        }
    }

    pub fn list(&self) -> Vec<Summary> {
        let mut v: Vec<Summary> = self.ids().iter().filter_map(|id| self.load(id).ok()).map(|i| self.summary(&i)).collect();
        v.sort_by(|a, b| b.created.cmp(&a.created).then(b.id.cmp(&a.id)));
        v
    }

    /// Conversations dont le titre ou le texte contient tous les mots cherchés.
    pub fn search(&self, q: &str) -> Vec<String> {
        let words: Vec<String> = fold(q).split_whitespace().map(String::from).collect();
        if words.is_empty() {
            return self.ids();
        }
        if self.index.lock().unwrap().len() < self.ids().len() {
            let _ = self.list();
        }
        let ix = self.index.lock().unwrap();
        ix.iter()
            .filter(|(_, (title, text))| words.iter().all(|w| title.contains(w) || text.contains(w)))
            .map(|(id, _)| id.clone())
            .collect()
    }

    // -- transcriptions ------------------------------------------------------
    pub fn new_transcription_dir(&self, id: &str) -> io::Result<(String, PathBuf)> {
        let d = self.dir(id)?.join(TRANSCRIPTIONS);
        let base = now_stamp();
        let mut name = base.clone();
        let mut n = 2;
        while d.join(&name).exists() {
            name = format!("{base} ({n})");
            n += 1;
        }
        fs::create_dir_all(d.join(&name))?;
        Ok((format!("{TRANSCRIPTIONS}/{name}"), d.join(name)))
    }

    /// Écrit le modèle et le texte lisible à la racine du dossier.
    pub fn save_transcript(&self, item: &Item, rel: &str, t: &Transcript) -> io::Result<()> {
        let d = self.root.join(&item.id);
        write_atomic(&d.join(rel).join("transcript.json"), &serde_json::to_vec_pretty(t).map_err(io::Error::other)?)?;
        if item.current.as_deref() == Some(rel) || item.current.is_none() {
            let mut txt = "\u{feff}".to_string();
            txt.push_str(&transcript::export_txt(t, &item.title, item.duration));
            // Simple copie lisible : un fichier verrouillé (OneDrive, éditeur
            // ouvert) ne doit pas faire échouer une transcription payée.
            if let Err(e) = write_atomic(&d.join("transcription.txt"), txt.as_bytes()) {
                crate::trace(&format!("transcription.txt de {} non mis à jour : {e}", item.id));
            }
        }
        if let Ok(mut ix) = self.index.lock() {
            ix.insert(item.id.clone(), (head_key(&item), fold(&t.plain_text())));
        }
        Ok(())
    }

    // -- niveaux -------------------------------------------------------------
    pub fn envelopes(&self, id: &str) -> Option<Envelopes> {
        serde_json::from_slice(&fs::read(self.dir(id).ok()?.join("envelope.json")).ok()?).ok()
    }

    pub fn save_envelopes(&self, id: &str, e: &Envelopes) -> io::Result<()> {
        write_atomic(&self.dir(id)?.join("envelope.json"), &serde_json::to_vec(e).map_err(io::Error::other)?)
    }

    // -- suppression ---------------------------------------------------------
    /// Déplace la conversation dans la corbeille de la bibliothèque (récupérable).
    pub fn trash(&self, id: &str) -> io::Result<()> {
        let d = self.dir(id)?;
        let t = self.root.join(TRASH);
        fs::create_dir_all(&t)?;
        let mut dest = t.join(id);
        let mut n = 2;
        while dest.exists() {
            dest = t.join(format!("{id} ({n})"));
            n += 1;
        }
        fs::rename(d, dest)?;
        self.index.lock().unwrap().remove(id);
        Ok(())
    }

    // -- démarrage -----------------------------------------------------------
    /// Conversations à reprendre : enregistrements interrompus (à finaliser) ;
    /// les transcriptions interrompues passent en erreur, sans renvoi automatique
    /// (ElevenLabs a pu facturer la requête).
    pub fn startup_scan(&self) -> Vec<String> {
        let mut to_finalize = vec![];
        for id in self.ids() {
            let Ok(item) = self.load(&id) else { continue };
            let has_wav = self.dir(&id).map(|d| [Role::Mic, Role::Pc].iter().any(|r| recorder::wav_path(&d, *r).exists())).unwrap_or(false);
            match item.state {
                State::Recording | State::Finalizing => to_finalize.push(id.clone()),
                // Échec de finalisation avec l'audio encore là : on réessaie.
                State::Failed if item.kind == Kind::Recording && has_wav => to_finalize.push(id.clone()),
                // Prête, mais arrêt avant le ménage : les FLAC vérifiés font foi.
                State::Ready if has_wav && !item.tracks.is_empty() => {
                    if let Ok(d) = self.dir(&id) {
                        let roles: Vec<Role> = item.tracks.iter().map(|t| t.role).collect();
                        if roles.iter().all(|r| d.join(format!("{}.flac", r.stem())).exists()) {
                            recorder::remove_wavs(&d, &roles);
                        }
                    }
                }
                _ => {}
            }
            if item.state == State::Importing {
                let _ = self.update(&id, |i| {
                    i.state = State::Failed;
                    i.error = Some("Import interrompu par la fermeture de Verbatim.".into());
                });
            }
            if item.job.as_ref().map(|j| j.state != "error").unwrap_or(false) {
                let _ = self.update(&id, |i| {
                    i.job = Some(JobInfo {
                        state: "error".into(),
                        message: Some("Transcription interrompue par la fermeture de Verbatim. Relance-la.".into()),
                        progress: None,
                    });
                });
            }
        }
        to_finalize
    }

    /// Reprend les transcriptions des versions 2 et 3 (fichiers .json à plat).
    pub fn migrate_legacy(&self, my_name: &str) -> usize {
        let Ok(rd) = fs::read_dir(&self.root) else { return 0 };
        let mut n = 0;
        let files: Vec<PathBuf> = rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_file() && p.extension().map(|e| e == "json").unwrap_or(false))
            .collect();
        for p in files {
            let Ok(raw) = fs::read(&p) else { continue };
            let Ok(doc) = serde_json::from_slice::<Value>(&raw) else { continue };
            let Some(result) = doc.get("elevenlabs") else { continue };
            let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            if self.root.join(&stem).exists() {
                continue;
            }
            let title = doc.get("source_file").and_then(Value::as_str).map(|s| safe_stem(s)).unwrap_or_else(|| stem.clone());
            let d = self.root.join(&stem);
            if fs::create_dir_all(d.join(TRANSCRIPTIONS).join("import")).is_err() {
                continue;
            }
            let mut item = Item::new(stem.clone(), title, Kind::Legacy, State::Ready);
            if let Some(at) = doc.get("transcribed_at").and_then(Value::as_str) {
                item.created = at.to_string();
            }
            item.notes.push("Transcription reprise d'une version précédente : l'audio n'était pas conservé.".into());
            let words = result.get("words").and_then(Value::as_array);
            item.duration = words.and_then(|w| w.iter().rev().find_map(|x| x.get("end").and_then(Value::as_f64)));
            let mut t = transcript::build(transcript::Input {
                mode: Mode::Single,
                responses: vec![(None, result)],
                levels: None,
                my_name,
                options: json!({}),
                created: item.created.clone(),
                notes: vec![],
            });
            if let Some(names) = doc.get("speakers").and_then(Value::as_object) {
                for s in &mut t.speakers {
                    if let Some(n) = names.get(&s.id).and_then(Value::as_str).filter(|n| !n.trim().is_empty()) {
                        s.name = n.trim().chars().take(40).collect();
                    }
                }
            }
            let rel = format!("{TRANSCRIPTIONS}/import");
            item.current = Some(rel.clone());
            item.transcripts.push(TranscriptRef { dir: rel.clone(), created: item.created.clone(), mode: Mode::Single, billed_seconds: 0.0, options: json!({}) });
            let ok = fs::rename(&p, d.join(&rel).join("brut.json")).is_ok()
                && self.write(&item).is_ok()
                && self.save_transcript(&item, &rel, &t).is_ok();
            if ok {
                let _ = fs::remove_file(p.with_extension("txt"));
                n += 1;
            }
        }
        n
    }
}

/// Fichier lisible par l'interface : chemin absolu de l'écoute.
pub fn playback_path(lib: &Library, item: &Item) -> Option<PathBuf> {
    let p = item.playback.as_ref()?;
    let pb = PathBuf::from(p);
    let full = if pb.is_absolute() { pb } else { lib.root.join(&item.id).join(p) };
    full.is_file().then_some(full)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::tests::tmpdir;

    #[test]
    fn creation_mise_a_jour_liste_recherche_corbeille() {
        let root = tmpdir("lib");
        let lib = Library::open(&root).unwrap();
        let a = lib.create("Appel avec Claire", Kind::Recording, State::Recording).unwrap();
        let b = lib.create("Appel avec Claire", Kind::Import, State::Ready).unwrap();
        assert_ne!(a.id, b.id, "noms uniques");
        assert!(a.id.ends_with("Appel avec Claire"));
        lib.update(&a.id, |i| {
            i.state = State::Ready;
            i.duration = Some(12.0);
        })
        .unwrap();
        assert_eq!(lib.load(&a.id).unwrap().state, State::Ready);
        assert_eq!(lib.list().len(), 2);
        assert!(lib.dir("../x").is_err() && lib.dir("..").is_err() && lib.dir("a/b").is_err() && lib.dir(TRASH).is_err());
        let resp = json!({"words": [{"text": "Réunion budgétaire", "type": "word", "start": 0.0, "end": 1.0, "speaker_id": "speaker_0"}]});
        let t = transcript::build(transcript::Input {
            mode: Mode::Single,
            responses: vec![(None, &resp)],
            levels: None,
            my_name: "Moi",
            options: json!({}),
            created: "x".into(),
            notes: vec![],
        });
        let (rel, _) = lib.new_transcription_dir(&b.id).unwrap();
        let item = lib.update(&b.id, |i| i.current = Some(rel.clone())).unwrap();
        lib.save_transcript(&item, &rel, &t).unwrap();
        assert!(fs::read_to_string(root.join(&b.id).join("transcription.txt")).unwrap().contains("Réunion budgétaire"));
        assert_eq!(lib.search("budgetaire REUNION"), vec![b.id.clone()], "insensible aux accents et à la casse");
        assert_eq!(lib.search("claire").len(), 2);
        let s = lib.summary(&lib.load(&b.id).unwrap());
        assert!(s.has_transcript && s.speakers == ["Locuteur"]);
        lib.trash(&a.id).unwrap();
        assert_eq!(lib.list().len(), 1);
        assert!(root.join(TRASH).join(&a.id).join(ITEM_FILE).exists(), "récupérable");
    }

    #[test]
    fn reprise_au_demarrage() {
        let root = tmpdir("scan");
        let lib = Library::open(&root).unwrap();
        let rec = lib.create("Enregistrement", Kind::Recording, State::Recording).unwrap();
        let tr = lib.create("Fichier", Kind::Import, State::Ready).unwrap();
        lib.update(&tr.id, |i| i.job = Some(JobInfo { state: "uploading".into(), message: None, progress: Some(0.4) })).unwrap();
        let todo = lib.startup_scan();
        assert_eq!(todo, vec![rec.id.clone()]);
        let j = lib.load(&tr.id).unwrap().job.unwrap();
        assert_eq!(j.state, "error");
        assert!(j.message.unwrap().contains("interrompue"));
    }

    #[test]
    fn migration_des_versions_precedentes() {
        let root = tmpdir("legacy");
        let doc = json!({
            "verbatim_version": 3, "source_file": "entretien partie 1.m4a", "transcribed_at": "2026-10-12T20:42:00",
            "speakers": {"speaker_0": "Manuel", "speaker_1": "Claire"},
            "elevenlabs": {"text": "x", "words": [
                {"text": "Bonjour.", "type": "word", "start": 0.0, "end": 0.5, "speaker_id": "speaker_0"},
                {"text": " ", "type": "spacing", "start": 0.5, "end": 0.6, "speaker_id": "speaker_0"},
                {"text": "Salut.", "type": "word", "start": 1.0, "end": 1.4, "speaker_id": "speaker_1"}]}
        });
        fs::write(root.join("2026-10-12 20h42 entretien partie 1.json"), doc.to_string()).unwrap();
        fs::write(root.join("2026-10-12 20h42 entretien partie 1.txt"), "ancien").unwrap();
        fs::write(root.join("settings.json"), "{}").unwrap();
        let lib = Library::open(&root).unwrap();
        assert_eq!(lib.migrate_legacy("Moi"), 1);
        assert_eq!(lib.migrate_legacy("Moi"), 0, "idempotent");
        let id = "2026-10-12 20h42 entretien partie 1";
        let item = lib.load(id).unwrap();
        assert_eq!((item.kind, item.title.as_str(), item.created.as_str()), (Kind::Legacy, "entretien partie 1", "2026-10-12T20:42:00"));
        let t = lib.current_transcript(&item).unwrap();
        assert_eq!(t.speakers.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["Manuel", "Claire"]);
        assert!(root.join(id).join("transcriptions/import/brut.json").exists());
        assert!(!root.join(format!("{id}.json")).exists() && !root.join(format!("{id}.txt")).exists());
        assert!(fs::read_to_string(root.join(id).join("transcription.txt")).unwrap().contains("[00:01] Claire"));
        assert!(root.join("settings.json").exists(), "les autres fichiers ne bougent pas");
    }
}
