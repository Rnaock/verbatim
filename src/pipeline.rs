//! Import de fichiers, préparation des envois, assemblage des transcriptions.
//! Tout est bloquant et testable sans réseau ; l'envoi lui-même est dans core.

use crate::audio::{self, BlockReader, EnvelopeBuilder, FlacWriter, STORE_RATE, UPLOAD_RATE};
use crate::core::Options;
use crate::library::{Envelopes, Item, JobInfo, Kind, Library, SourceFile, State, TranscriptRef, CACHE};
use crate::recorder::{self, Finalized, Role};
use crate::transcript::{self, Levels, Mode, Transcript};
use serde::Serialize;
use serde_json::{json, Value};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Copié tel quel dans la bibliothèque jusqu'à cette taille.
const COPY_LIMIT: u64 = 500 * 1024 * 1024;
/// Formats compressés envoyés tels quels : plus légers qu'une conversion.
const COMPRESSED: &[&str] = &["m4a", "mp3", "aac", "ogg", "opus", "oga", "wma", "amr", "weba"];
pub const VIDEO: &[&str] = &["mp4", "m4v", "mov", "mkv", "avi", "wmv", "flv", "mpeg", "mpg", "webm", "3gp", "ts"];
pub const AUDIO: &[&str] = &["m4a", "mp3", "aac", "ogg", "opus", "oga", "wma", "amr", "weba", "wav", "flac", "aiff", "aif", "caf", "alac"];
/// Seuil d'écho au-delà duquel les pistes séparées produiraient des doublons.
pub const ECHO_LIMIT: f32 = 0.5;
/// Une piste où l'on parle moins que ça n'est pas envoyée à part.
const MIN_SPEECH: f32 = 0.01;

fn ext(p: &Path) -> String {
    p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase()
}

pub fn is_media(p: &Path) -> bool {
    let e = ext(p);
    AUDIO.contains(&e.as_str()) || VIDEO.contains(&e.as_str())
}

// ---------------------------------------------------------------------------
// Fin d'enregistrement
// ---------------------------------------------------------------------------
pub fn apply_finalized(lib: &Library, id: &str, f: &Finalized) -> io::Result<Item> {
    let mut env = Envelopes::default();
    for (role, e) in &f.envelopes {
        env.set(role.stem(), e);
    }
    lib.save_envelopes(id, &env)?;
    lib.update(id, |i| {
        i.state = State::Ready;
        i.duration = Some(f.duration);
        i.tracks = f.tracks.clone();
        i.playback = Some(f.playback.clone());
        i.echo = f.echo;
        i.recovered |= f.recovered;
        if f.recovered {
            i.notes.push("Enregistrement récupéré après une interruption : les dernières secondes ont pu être perdues.".into());
        }
        if f.echo.map(|e| e > ECHO_LIMIT).unwrap_or(false) {
            i.notes.push("Le micro capte le son des haut-parleurs. Avec un casque, la séparation des voix est meilleure.".into());
        }
    })
}

pub fn finalize_item<P: FnMut(f32)>(lib: &Library, id: &str, recover: bool, progress: P) -> io::Result<Item> {
    let item = lib.load(id)?;
    let roles = if item.roles.is_empty() { vec![Role::Mic, Role::Pc] } else { item.roles.clone() };
    lib.update(id, |i| i.state = State::Finalizing)?;
    let dir = lib.dir(id)?;
    match recorder::finalize(&dir, &roles, recover, progress) {
        Ok(f) => {
            // Si l'enregistrement de l'état échoue, les WAV restent : la
            // finalisation sera refaite au prochain lancement.
            let item = apply_finalized(lib, id, &f)?;
            recorder::remove_wavs(&dir, &roles);
            Ok(item)
        }
        Err(e) => {
            lib.update(id, |i| {
                i.state = State::Failed;
                i.error = Some(format!("Finalisation impossible : {e}"));
            })?;
            Err(e)
        }
    }
}

// ---------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------
/// Crée la conversation tout de suite (pour l'afficher), puis `import_fill`
/// fait le travail long.
pub fn import_create(lib: &Library, src: &Path) -> io::Result<Item> {
    let meta = fs::metadata(src)?;
    if !meta.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "ce n'est pas un fichier"));
    }
    let name = src.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let title = src.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| name.clone());
    let item = lib.create(&title, Kind::Import, State::Importing)?;
    lib.update(&item.id, |i| {
        i.source = Some(SourceFile {
            name: name.clone(),
            original: src.to_string_lossy().into_owned(),
            file: None,
            size: meta.len(),
            video: VIDEO.contains(&ext(src).as_str()),
            decodable: false,
        })
    })
}

pub fn import_fill<P: FnMut(f32)>(lib: &Library, id: &str, mut progress: P) -> io::Result<Item> {
    let item = lib.load(id)?;
    let dir = lib.dir(id)?;
    let src_file = item.source.clone().unwrap_or_default();
    let src = PathBuf::from(&src_file.original);
    let res = (|| -> io::Result<Item> {
        // 1. Copie de l'original si raisonnable : la bibliothèque reste complète
        //    même si le fichier du téléphone disparaît.
        let mut copied = None;
        if src_file.size <= COPY_LIMIT {
            let stem = src.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "original".into());
            let dest = dir.join(crate::core::safe_file_name(&stem, &ext(&src)));
            fs::copy(&src, &dest)?;
            copied = Some(dest.file_name().unwrap().to_string_lossy().into_owned());
        }
        progress(0.2);
        // 2. Décodage : durée, niveaux, et pour les vidéos, extraction de l'audio.
        let info = audio::media_info(&src).ok();
        let mut env = EnvelopeBuilder::new(STORE_RATE);
        let extract = src_file.video;
        let flac_tmp = dir.join("audio.flac.tmp");
        let mut flac = if extract { Some(FlacWriter::create(&flac_tmp, STORE_RATE)?) } else { None };
        let expected = info.as_ref().and_then(|i| i.duration).unwrap_or(0.0) * STORE_RATE as f64;
        let mut done = 0f64;
        let mut write_err = None;
        let decoded = audio::decode_mono(&src, STORE_RATE, |b| {
            env.push(b);
            if let Some(w) = flac.as_mut() {
                if let Err(e) = w.write(b) {
                    write_err = Some(e);
                    return false;
                }
            }
            done += b.len() as f64;
            if expected > 0.0 {
                progress(0.2 + 0.8 * (done / expected).min(1.0) as f32);
            }
            true
        });
        if let Some(e) = write_err {
            return Err(e);
        }
        let (decodable, duration, playback) = match decoded {
            Ok(n) if n > 0 => {
                let pb = if let Some(w) = flac.take() {
                    w.finish()?;
                    fs::rename(&flac_tmp, dir.join("audio.flac"))?;
                    "audio.flac".to_string()
                } else {
                    copied.clone().unwrap_or_else(|| src_file.original.clone())
                };
                let mut e = Envelopes::default();
                e.set("audio", &env.finish());
                lib.save_envelopes(id, &e)?;
                (true, Some(n as f64 / STORE_RATE as f64), pb)
            }
            _ => {
                drop(flac);
                let _ = fs::remove_file(&flac_tmp);
                // Format que Verbatim ne lit pas (Opus, AMR…) : ElevenLabs s'en chargera.
                (false, info.and_then(|i| i.duration), copied.clone().unwrap_or_else(|| src_file.original.clone()))
            }
        };
        lib.update(id, |i| {
            i.state = State::Ready;
            i.duration = duration;
            i.playback = Some(playback.clone());
            if let Some(s) = i.source.as_mut() {
                s.file = copied.clone();
                s.decodable = decodable;
            }
            if !decodable {
                i.notes.push("Format lu directement par ElevenLabs : pas de frise de niveaux pour ce fichier.".into());
            }
            if copied.is_none() {
                i.notes.push(format!("Fichier volumineux laissé à son emplacement d'origine : {}", src_file.original));
            }
        })
    })();
    if let Err(e) = &res {
        let _ = lib.update(id, |i| {
            i.state = State::Failed;
            i.error = Some(format!("Import impossible : {e}"));
        });
    }
    progress(1.0);
    res
}

// ---------------------------------------------------------------------------
// Préparation des envois
// ---------------------------------------------------------------------------
#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub mode: Mode,
    /// Pistes à envoyer : source connue ou non.
    pub parts: Vec<Option<Role>>,
    pub seconds: f64,
    pub notes: Vec<String>,
}

impl Plan {
    pub fn billed_seconds(&self) -> f64 {
        self.seconds * self.parts.len() as f64
    }
}

/// Décide comment transcrire une conversation, sans rien encoder.
/// `pref` : "auto", "mixed" ou "separate".
pub fn plan(item: &Item, pref: &str) -> io::Result<Plan> {
    let seconds = item.duration.unwrap_or(0.0);
    if item.state != State::Ready {
        return Err(io::Error::other("conversation pas encore prête"));
    }
    if item.tracks.len() < 2 {
        let parts = match item.tracks.first() {
            Some(t) => vec![Some(t.role)],
            None => {
                if item.source.is_none() && item.kind == Kind::Legacy {
                    return Err(io::Error::other("l'audio de cette conversation n'a pas été conservé"));
                }
                vec![None]
            }
        };
        let mode = if parts[0].is_some() { Mode::Separate } else { Mode::Single };
        return Ok(Plan { mode, parts, seconds, notes: vec![] });
    }
    let mut notes = vec![];
    let speaking: Vec<Role> = item.tracks.iter().filter(|t| t.activity.speech_ratio >= MIN_SPEECH).map(|t| t.role).collect();
    let echo = item.echo.map(|e| e > ECHO_LIMIT).unwrap_or(false);
    // Automatique : une seule piste parle → elle seule est envoyée ; les deux
    // parlent → une piste mixée (facturée une fois). Les pistes locales servent
    // ensuite à retrouver qui parle. « separate » reste l'option qualité max (×2).
    let mode = match pref {
        "mixed" => Mode::Mixed,
        "separate" if echo => {
            notes.push("Le micro capte les haut-parleurs : les deux sources sont transcrites ensemble pour éviter les doublons.".into());
            Mode::Mixed
        }
        "separate" => Mode::Separate,
        _ if speaking.len() < 2 => Mode::Separate,
        _ => Mode::Mixed,
    };
    let parts = match mode {
        Mode::Mixed => vec![None],
        _ => {
            if speaking.len() < item.tracks.len() {
                let quiet: Vec<&str> = item.tracks.iter().filter(|t| !speaking.contains(&t.role)).map(|t| t.role.label()).collect();
                notes.push(format!("Piste presque muette non envoyée : {}.", quiet.join(", ")));
            }
            if speaking.is_empty() {
                item.tracks.iter().map(|t| Some(t.role)).take(1).collect()
            } else {
                speaking.into_iter().map(Some).collect()
            }
        }
    };
    Ok(Plan { mode, parts, seconds, notes })
}

fn upload_name(part: Option<Role>) -> String {
    format!("envoi-{}.flac", part.map(|r| r.stem()).unwrap_or("mix"))
}

fn encode_upload<F: FnMut() -> io::Result<Option<Vec<f32>>>>(out: &Path, mut next: F, total: f64, progress: &mut dyn FnMut(f32)) -> io::Result<()> {
    let tmp = out.with_extension("tmp");
    let mut w = FlacWriter::create(&tmp, UPLOAD_RATE)?;
    let mut done = 0f64;
    while let Some(b) = next()? {
        w.write(&b)?;
        done += b.len() as f64;
        if total > 0.0 {
            progress((done / total).min(1.0) as f32);
        }
    }
    w.finish()?;
    fs::rename(tmp, out)
}

/// Produit les fichiers à envoyer (16 kHz, niveaux égalisés) dans `.cache`.
/// Renvoie un chemin par partie du plan.
pub fn prepare(lib: &Library, item: &Item, plan: &Plan, progress: &mut dyn FnMut(f32)) -> io::Result<Vec<PathBuf>> {
    let dir = lib.dir(&item.id)?;
    let cache = dir.join(CACHE);
    fs::create_dir_all(&cache)?;
    let total = plan.seconds * UPLOAD_RATE as f64;
    let mut out = vec![];
    let n = plan.parts.len().max(1) as f32;
    for (k, part) in plan.parts.iter().enumerate() {
        let mut sub = |p: f32| progress((k as f32 + p) / n);
        let dest = cache.join(upload_name(*part));
        let track = |r: Role| item.tracks.iter().find(|t| t.role == r).cloned();
        match (plan.mode, part) {
            (Mode::Mixed, None) if item.tracks.len() >= 2 => {
                let ta = track(Role::Mic).ok_or_else(|| io::Error::other("piste micro absente"))?;
                let tb = track(Role::Pc).ok_or_else(|| io::Error::other("piste PC absente"))?;
                let mut a = BlockReader::open(&dir.join(&ta.file), UPLOAD_RATE)?;
                let mut b = BlockReader::open(&dir.join(&tb.file), UPLOAD_RATE)?;
                let (ga, gb) = (audio::db_to_gain(ta.gain_db), audio::db_to_gain(tb.gain_db));
                let (mut x, mut y) = (vec![], vec![]);
                encode_upload(
                    &dest,
                    || {
                        x.clear();
                        y.clear();
                        let na = a.read(16_000, &mut x)?;
                        let nb = b.read(16_000, &mut y)?;
                        if na == 0 && nb == 0 {
                            return Ok(None);
                        }
                        Ok(Some((0..na.max(nb)).map(|i| recorder::mix_sample(x.get(i).unwrap_or(&0.0) * ga, y.get(i).unwrap_or(&0.0) * gb)).collect()))
                    },
                    total,
                    &mut sub,
                )?;
                out.push(dest);
            }
            (_, Some(role)) => {
                let t = track(*role).ok_or_else(|| io::Error::other("piste absente"))?;
                let mut r = BlockReader::open(&dir.join(&t.file), UPLOAD_RATE)?;
                let g = audio::db_to_gain(t.gain_db);
                let mut x = vec![];
                encode_upload(
                    &dest,
                    || {
                        x.clear();
                        Ok((r.read(16_000, &mut x)? > 0).then(|| x.iter().map(|s| (s * g).clamp(-1.0, 1.0)).collect()))
                    },
                    total,
                    &mut sub,
                )?;
                out.push(dest);
            }
            (_, None) => {
                // Fichier importé (ou reprise sans pistes).
                let src = item.source.as_ref().ok_or_else(|| io::Error::other("source introuvable"))?;
                let original = src.file.as_ref().map(|f| dir.join(f)).filter(|p| p.is_file()).unwrap_or_else(|| PathBuf::from(&src.original));
                if !original.is_file() {
                    return Err(io::Error::new(io::ErrorKind::NotFound, format!("fichier d'origine introuvable : {}", original.display())));
                }
                let compressed = COMPRESSED.contains(&ext(&original).as_str());
                if compressed || !src.decodable {
                    sub(1.0);
                    out.push(original);
                } else {
                    let base = item.playback.as_ref().filter(|p| !Path::new(p).is_absolute()).map(|p| dir.join(p)).filter(|p| p.is_file()).unwrap_or(original);
                    let mut r = BlockReader::open(&base, UPLOAD_RATE)?;
                    let mut x = vec![];
                    encode_upload(
                        &dest,
                        || {
                            x.clear();
                            Ok((r.read(16_000, &mut x)? > 0).then(|| x.clone()))
                        },
                        total,
                        &mut sub,
                    )?;
                    out.push(dest);
                }
            }
        }
    }
    Ok(out)
}

pub fn clear_cache(lib: &Library, id: &str) {
    if let Ok(d) = lib.dir(id) {
        let _ = fs::remove_dir_all(d.join(CACHE));
    }
}

/// Assemble les réponses, écrit tout sur le disque, et en fait la transcription courante.
pub fn finish(lib: &Library, id: &str, plan: &Plan, responses: &[(Option<Role>, Value)], options: &Options, my_name: &str) -> io::Result<(Item, Transcript)> {
    let item = lib.load(id)?;
    let (rel, tdir) = lib.new_transcription_dir(id)?;
    // Les réponses brutes d'abord : c'est ce qui a été payé.
    for (part, resp) in responses {
        let name = format!("brut-{}.json", part.map(|r| r.stem()).unwrap_or(if plan.mode == Mode::Mixed { "mix" } else { "audio" }));
        crate::core::write_atomic(&tdir.join(name), &serde_json::to_vec(resp).map_err(io::Error::other)?)?;
    }
    let env = lib.envelopes(id);
    let (mic_env, pc_env) = (env.as_ref().and_then(|e| e.get("micro")), env.as_ref().and_then(|e| e.get("pc")));
    let gain = |r: Role| item.tracks.iter().find(|t| t.role == r).map(|t| t.gain_db).unwrap_or(0.0);
    let levels = match (&mic_env, &pc_env) {
        (Some(m), Some(p)) if plan.mode == Mode::Mixed => Some(Levels { mic: m, pc: p, mic_gain_db: gain(Role::Mic), pc_gain_db: gain(Role::Pc) }),
        _ => None,
    };
    let opts_json = json!({
        "lang": options.lang, "speakers": options.speakers, "no_verbatim": options.no_verbatim,
        "keyterms": crate::core::filter_keyterms(&options.keyterms).len(),
    });
    let mut t = transcript::build(transcript::Input {
        mode: plan.mode,
        responses: responses.iter().map(|(p, v)| (*p, v)).collect(),
        levels,
        my_name,
        options: opts_json.clone(),
        created: chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
        notes: plan.notes.clone(),
    });
    if let Some(ev) = &item.event {
        name_from_event(&mut t, ev, my_name);
    }
    // Transcription écrite avant de la déclarer courante : jamais de version
    // « courante » sans fichier derrière.
    let mut pending = item.clone();
    pending.current = Some(rel.clone());
    lib.save_transcript(&pending, &rel, &t)?;
    let item = lib.update(id, |i| {
        i.current = Some(rel.clone());
        i.transcripts.push(TranscriptRef {
            dir: rel.clone(),
            created: t.created.clone(),
            mode: plan.mode,
            billed_seconds: plan.billed_seconds(),
            options: opts_json.clone(),
        });
        i.job = None;
    })?;
    clear_cache(lib, id);
    Ok((item, t))
}

/// Rendez-vous lié : s'il n'y a qu'une voix côté PC et qu'un seul autre
/// participant, c'est lui. Dans les autres cas, ses participants sont proposés
/// à la correction, sans deviner.
pub fn name_from_event(t: &mut Transcript, ev: &crate::calendar::CalEvent, my_name: &str) {
    let others: Vec<&crate::calendar::Person> = ev.people.iter().filter(|p| !p.me && p.name != my_name).collect();
    let pc: Vec<usize> = t.speakers.iter().enumerate().filter(|(_, s)| s.source == Some(Role::Pc)).map(|(i, _)| i).collect();
    if pc.len() == 1 && others.len() == 1 {
        t.speakers[pc[0]].name = others[0].name.clone();
    }
}

pub fn set_job(lib: &Library, id: &str, state: &str, message: Option<String>, progress: Option<f32>) -> io::Result<Item> {
    lib.update(id, |i| i.job = Some(JobInfo { state: state.into(), message, progress }))
}

/// Reconstruit la transcription courante depuis les réponses brutes (annule
/// toutes les corrections de locuteurs).
pub fn rebuild(lib: &Library, id: &str, my_name: &str) -> io::Result<Transcript> {
    let item = lib.load(id)?;
    let rel = item.current.clone().ok_or_else(|| io::Error::other("aucune transcription"))?;
    let tdir = lib.dir(id)?.join(&rel);
    // Mode d'origine : celui de la transcription, sinon celui noté dans l'historique.
    let (mode, options, created, notes) = match lib.current_transcript(&item) {
        Some(old) => (old.mode, old.options, old.created, old.notes),
        None => {
            let r = item.transcripts.iter().find(|r| r.dir == rel).ok_or_else(|| io::Error::other("transcription illisible"))?;
            (r.mode, r.options.clone(), r.created.clone(), vec![])
        }
    };
    let mut responses = vec![];
    for (name, part) in [("brut-micro.json", Some(Role::Mic)), ("brut-pc.json", Some(Role::Pc)), ("brut-mix.json", None), ("brut-audio.json", None), ("brut.json", None)] {
        if let Ok(raw) = fs::read(tdir.join(name)) {
            if let Ok(v) = serde_json::from_slice::<Value>(&raw) {
                responses.push((part, v));
            }
        }
    }
    if responses.is_empty() {
        return Err(io::Error::other("réponses d'origine introuvables"));
    }
    let env = lib.envelopes(id);
    let (m, p) = (env.as_ref().and_then(|e| e.get("micro")), env.as_ref().and_then(|e| e.get("pc")));
    let gain = |r: Role| item.tracks.iter().find(|t| t.role == r).map(|t| t.gain_db).unwrap_or(0.0);
    let levels = match (&m, &p) {
        (Some(m), Some(p)) if mode == Mode::Mixed => Some(Levels { mic: m, pc: p, mic_gain_db: gain(Role::Mic), pc_gain_db: gain(Role::Pc) }),
        _ => None,
    };
    let mut t = transcript::build(transcript::Input {
        mode,
        responses: responses.iter().map(|(p, v)| (*p, v)).collect(),
        levels,
        my_name,
        options,
        created,
        notes,
    });
    if let Some(ev) = &item.event {
        name_from_event(&mut t, ev, my_name);
    }
    lib.save_transcript(&item, &rel, &t)?;
    Ok(t)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::tests::{sine, tmpdir};
    use crate::audio::WavWriter;
    use crate::recorder::wav_path;

    fn fake_recording(lib: &Library, mic_amp: f32, pc_follows_mic: bool) -> String {
        let item = lib.create("Appel", Kind::Recording, State::Recording).unwrap();
        lib.update(&item.id, |i| i.roles = vec![Role::Mic, Role::Pc]).unwrap();
        let dir = lib.dir(&item.id).unwrap();
        let r = STORE_RATE;
        let mut mic = WavWriter::create(&wav_path(&dir, Role::Mic), r).unwrap();
        let mut pc = WavWriter::create(&wav_path(&dir, Role::Pc), r).unwrap();
        for i in 0..40 {
            let pc_talks = i % 2 == 1;
            let amp_pc = if pc_talks { 0.2 + 0.02 * (i % 5) as f32 } else { 0.0003 };
            let amp_mic = if pc_follows_mic { amp_pc * 0.2 } else if pc_talks { 0.0003 } else { mic_amp };
            mic.write(&sine(200.0, r, 1.0, amp_mic)).unwrap();
            pc.write(&sine(300.0, r, 1.0, amp_pc)).unwrap();
        }
        mic.finish().unwrap();
        pc.finish().unwrap();
        item.id
    }

    #[test]
    fn plan_selon_les_cas() {
        let root = tmpdir("plan");
        let lib = Library::open(&root).unwrap();
        // Casque, les deux parlent : piste mixée (facturée une fois) ;
        // pistes séparées sur demande (×2).
        let id = fake_recording(&lib, 0.1, false);
        let it = finalize_item(&lib, &id, false, |_| {}).unwrap();
        let p = plan(&it, "auto").unwrap();
        assert_eq!((p.mode, p.parts.clone()), (Mode::Mixed, vec![None]));
        assert!((p.billed_seconds() - 40.0).abs() < 0.1);
        let p = plan(&it, "separate").unwrap();
        assert_eq!((p.mode, p.parts.clone()), (Mode::Separate, vec![Some(Role::Mic), Some(Role::Pc)]));
        assert!((p.billed_seconds() - 80.0).abs() < 0.1);
        // Micro muet (personne ne parle au micro) : une seule piste envoyée.
        let id = fake_recording(&lib, 0.0003, false);
        let it = finalize_item(&lib, &id, false, |_| {}).unwrap();
        let p = plan(&it, "auto").unwrap();
        assert_eq!(p.parts, vec![Some(Role::Pc)]);
        assert!(p.notes[0].contains("Micro"));
        // Haut-parleurs : le micro suit le PC → piste mixée.
        let id = fake_recording(&lib, 0.1, true);
        let it = finalize_item(&lib, &id, false, |_| {}).unwrap();
        assert!(it.echo.unwrap() > ECHO_LIMIT, "{:?}", it.echo);
        let p = plan(&it, "auto").unwrap();
        assert_eq!((p.mode, p.parts.clone()), (Mode::Mixed, vec![None]));
        assert!(it.notes.iter().any(|n| n.contains("casque")));
        // Préparation : un FLAC 16 kHz de 40 s.
        let files = prepare(&lib, &it, &p, &mut |_| {}).unwrap();
        let info = audio::media_info(&files[0]).unwrap();
        assert_eq!(info.rate, Some(UPLOAD_RATE));
        assert!((info.duration.unwrap() - 40.0).abs() < 0.05);
    }

    #[test]
    fn import_wav_puis_video_inconnue() {
        let root = tmpdir("imp");
        let lib = Library::open(&root).unwrap();
        let src_dir = tmpdir("imp-src");
        // WAV stéréo 44,1 kHz : copié, décodé, converti à l'envoi.
        let wav = src_dir.join("Réunion téléphone.wav");
        let mut w = WavWriter::create(&wav, 44_100).unwrap();
        w.write(&sine(250.0, 44_100, 3.0, 0.3)).unwrap();
        w.finish().unwrap();
        let it = import_create(&lib, &wav).unwrap();
        assert_eq!((it.state, it.title.as_str()), (State::Importing, "Réunion téléphone"));
        let it = import_fill(&lib, &it.id, |_| {}).unwrap();
        assert_eq!(it.state, State::Ready);
        assert!((it.duration.unwrap() - 3.0).abs() < 0.01);
        let s = it.source.clone().unwrap();
        assert!(s.decodable && s.file.is_some());
        assert!(lib.envelopes(&it.id).unwrap().get("audio").unwrap().len() >= 149);
        let p = plan(&it, "auto").unwrap();
        assert_eq!((p.mode, p.parts.clone()), (Mode::Single, vec![None]));
        let files = prepare(&lib, &it, &p, &mut |_| {}).unwrap();
        assert!(files[0].ends_with(".cache/envoi-mix.flac") || files[0].to_string_lossy().contains("envoi-mix.flac"));
        assert_eq!(audio::media_info(&files[0]).unwrap().rate, Some(UPLOAD_RATE));
        // Fichier illisible pour Verbatim (contenu quelconque) : envoyé tel quel.
        let bad = src_dir.join("note vocale.opus");
        fs::write(&bad, vec![1u8; 5000]).unwrap();
        let it = import_create(&lib, &bad).unwrap();
        let it = import_fill(&lib, &it.id, |_| {}).unwrap();
        assert_eq!(it.state, State::Ready);
        assert!(!it.source.as_ref().unwrap().decodable);
        let files = prepare(&lib, &it, &plan(&it, "auto").unwrap(), &mut |_| {}).unwrap();
        assert!(files[0].to_string_lossy().ends_with("note vocale.opus"));
    }

    /// Casque + piste mixée (mode automatique) : même si ElevenLabs confond les
    /// deux voix, les pistes locales disent qui parle.
    #[test]
    fn piste_mixee_avec_casque_retrouve_qui_parle() {
        let root = tmpdir("mixcasque");
        let lib = Library::open(&root).unwrap();
        let id = fake_recording(&lib, 0.1, false);
        let it = finalize_item(&lib, &id, false, |_| {}).unwrap();
        let p = plan(&it, "auto").unwrap();
        assert_eq!(p.mode, Mode::Mixed);
        let opts = Options { lang: "fr".into(), speakers: "auto".into(), no_verbatim: false, keyterms: vec![] };
        // Toutes les secondes paires : moi au micro ; impaires : le PC.
        let words = |same: bool| {
            let w: Vec<Value> = (0..30)
                .map(|i| {
                    let me = i % 2 == 0;
                    let spk = if same || me { "speaker_0" } else { "speaker_1" };
                    json!({"text": if me { "moi" } else { "lui" }, "type": "word", "start": i as f64 + 0.2, "end": i as f64 + 0.7, "speaker_id": spk})
                })
                .collect();
            json!({"language_code": "fra", "words": w})
        };
        for same in [false, true] {
            let (_, t) = finish(&lib, &id, &p, &[(None, words(same))], &opts, "Manuel").unwrap();
            let names: Vec<&str> = t.speakers.iter().map(|s| s.name.as_str()).collect();
            assert_eq!(names, ["Manuel", "Interlocuteur"], "même locuteur ElevenLabs : {same}");
            for seg in &t.segments {
                let me = seg.text.contains("moi");
                assert!(seg.text.contains(if me { "moi" } else { "lui" }) && !(me && seg.text.contains("lui")), "{seg:?}");
                assert_eq!(seg.source, Some(if me { Role::Mic } else { Role::Pc }));
            }
        }
    }

    #[test]
    fn assemblage_puis_reconstruction() {
        let root = tmpdir("fin2");
        let lib = Library::open(&root).unwrap();
        let id = fake_recording(&lib, 0.1, false);
        let it = finalize_item(&lib, &id, false, |_| {}).unwrap();
        let p = plan(&it, "separate").unwrap();
        let mic = json!({"language_code": "fra", "words": [{"text": "Allô ?", "type": "word", "start": 0.1, "end": 0.6, "speaker_id": "speaker_0"}]});
        let pc = json!({"words": [{"text": "Oui, bonjour.", "type": "word", "start": 1.1, "end": 1.8, "speaker_id": "speaker_0"}]});
        let opts = Options { lang: "fr".into(), speakers: "auto".into(), no_verbatim: false, keyterms: vec![] };
        let (item, t) = finish(&lib, &id, &p, &[(Some(Role::Mic), mic), (Some(Role::Pc), pc)], &opts, "Manuel").unwrap();
        assert_eq!(t.speakers.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["Manuel", "Interlocuteur"]);
        assert_eq!(item.transcripts.len(), 1);
        assert!(item.job.is_none());
        let txt = fs::read_to_string(lib.dir(&id).unwrap().join("transcription.txt")).unwrap();
        assert!(txt.contains("[00:00] Manuel\nAllô ?") && txt.contains("[00:01] Interlocuteur\nOui, bonjour."), "{txt}");
        // Correction puis réinitialisation depuis les réponses brutes.
        let mut t2 = t.clone();
        let mut a = t2.attribution();
        a.speakers[1].name = "Claire".into();
        t2.apply(&a).unwrap();
        lib.save_transcript(&item, item.current.as_ref().unwrap(), &t2).unwrap();
        assert_eq!(lib.current_transcript(&lib.load(&id).unwrap()).unwrap().speakers[1].name, "Claire");
        let back = rebuild(&lib, &id, "Manuel").unwrap();
        assert_eq!(back.speakers[1].name, "Interlocuteur");
        assert!(!lib.dir(&id).unwrap().join(CACHE).exists(), "cache nettoyé");
    }
}
