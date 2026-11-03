//! Modèle de transcription de Verbatim.
//!
//! Les réponses brutes d'ElevenLabs sont conservées telles quelles à côté ;
//! ce module en tire un modèle stable : des locuteurs nommés et des segments
//! (paragraphes) attribués à un locuteur. Les corrections portent sur
//! l'attribution, jamais sur le texte.
//!
//! Principe des sources : une source (micro, son du PC) est une frontière
//! entre groupes de locuteurs, la diarisation subdivise à l'intérieur. On ne
//! suppose jamais qu'une source correspond à une seule personne.

use crate::audio::{env_db, ENV_FPS};
use crate::recorder::Role;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};

const PAUSE_SPLIT: f64 = 1.6;
const LONG_PARA: usize = 900;
/// Écart de niveau (dB) pour qu'un mot soit attribué à une source.
const DOMINANCE_DB: f32 = 6.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Une seule piste (fichier importé, ou une seule source enregistrée).
    Single,
    /// Micro et PC mélangés en une piste, sources retrouvées par les niveaux.
    Mixed,
    /// Chaque source transcrite à part, puis fusionnée.
    Separate,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Speaker {
    pub id: String,
    pub name: String,
    pub source: Option<Role>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Segment {
    pub id: u32,
    pub speaker: String,
    pub source: Option<Role>,
    pub start: f64,
    pub end: f64,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transcript {
    pub v: u32,
    pub created: String,
    pub mode: Mode,
    pub language: Option<String>,
    pub options: Value,
    pub speakers: Vec<Speaker>,
    pub segments: Vec<Segment>,
    #[serde(default)]
    pub notes: Vec<String>,
}

// ---------------------------------------------------------------------------
// Lecture des réponses ElevenLabs
// ---------------------------------------------------------------------------
#[derive(Debug, Clone, PartialEq)]
enum Kind {
    Word,
    Spacing,
}

#[derive(Debug, Clone)]
struct Token {
    text: String,
    start: f64,
    end: f64,
    kind: Kind,
    speaker: String,
    source: Option<Role>,
}

fn tokens(resp: &Value, prefix: &str, source: Option<Role>) -> Vec<Token> {
    let words = resp.get("words").and_then(Value::as_array);
    let Some(words) = words.filter(|w| !w.is_empty()) else {
        let text = resp.get("text").and_then(Value::as_str).unwrap_or("").trim().to_string();
        return if text.is_empty() {
            vec![]
        } else {
            vec![Token { text, start: 0.0, end: 0.0, kind: Kind::Word, speaker: format!("{prefix}speaker_0"), source }]
        };
    };
    let mut out = Vec::with_capacity(words.len());
    let mut last_end = 0.0;
    for w in words {
        let kind = match w.get("type").and_then(Value::as_str).unwrap_or("word") {
            "spacing" => Kind::Spacing,
            "audio_event" => continue,
            _ => Kind::Word,
        };
        let text = w.get("text").and_then(Value::as_str).unwrap_or("").to_string();
        let start = w.get("start").and_then(Value::as_f64).unwrap_or(last_end);
        let end = w.get("end").and_then(Value::as_f64).unwrap_or(start);
        last_end = end;
        let spk = w.get("speaker_id").and_then(Value::as_str).unwrap_or("speaker_0");
        out.push(Token { text, start, end, kind, speaker: format!("{prefix}{spk}"), source });
    }
    out
}

// ---------------------------------------------------------------------------
// Attribution aux sources, par les niveaux de chaque piste
// ---------------------------------------------------------------------------
/// Enveloppes (une valeur toutes les 20 ms) et gains appliqués au mixage.
pub struct Levels<'a> {
    pub mic: &'a [u8],
    pub pc: &'a [u8],
    pub mic_gain_db: f32,
    pub pc_gain_db: f32,
}

impl Levels<'_> {
    fn mean_db(env: &[u8], gain: f32, a: f64, b: f64) -> f32 {
        let i0 = ((a * ENV_FPS as f64).floor() as usize).min(env.len());
        let i1 = ((b * ENV_FPS as f64).ceil() as usize).clamp(i0 + 1, env.len().max(i0 + 1)).min(env.len());
        if i0 >= i1 {
            return -90.0;
        }
        let p: f32 = env[i0..i1].iter().map(|&q| 10f32.powf(env_db(q) / 10.0)).sum::<f32>() / (i1 - i0) as f32;
        10.0 * p.max(1e-12).log10() + gain
    }

    /// > 0 : le micro domine ; < 0 : le PC domine.
    pub fn dominance(&self, a: f64, b: f64) -> f32 {
        Self::mean_db(self.mic, self.mic_gain_db, a, b) - Self::mean_db(self.pc, self.pc_gain_db, a, b)
    }
}

/// Donne une source à chaque mot ; scinde un locuteur que la diarisation a
/// formé avec des voix venant des deux sources.
fn attribute(toks: &mut [Token], lv: &Levels) {
    let dom: Vec<Option<Role>> = toks
        .iter()
        .map(|t| {
            if t.kind != Kind::Word {
                return None;
            }
            let d = lv.dominance(t.start, t.end.max(t.start + 0.02));
            if d >= DOMINANCE_DB {
                Some(Role::Mic)
            } else if d <= -DOMINANCE_DB {
                Some(Role::Pc)
            } else {
                None
            }
        })
        .collect();
    let mut counts: HashMap<String, (usize, usize)> = HashMap::new();
    for (t, d) in toks.iter().zip(&dom) {
        let c = counts.entry(t.speaker.clone()).or_default();
        match d {
            Some(Role::Mic) => c.0 += 1,
            Some(Role::Pc) => c.1 += 1,
            None => {}
        }
    }
    // Source dominante de chaque locuteur ; scission s'il est clairement mixte.
    let mut split = HashMap::new();
    let mut major = HashMap::new();
    for (s, &(m, p)) in &counts {
        let tot = (m + p).max(1);
        let mixed = m >= 8 && p >= 8 && m * 5 >= tot && p * 5 >= tot;
        if mixed {
            split.insert(s.clone(), ());
        }
        major.insert(s.clone(), if m == 0 && p == 0 { None } else if m >= p { Some(Role::Mic) } else { Some(Role::Pc) });
    }
    let mut current: HashMap<String, Role> = HashMap::new();
    for (t, d) in toks.iter_mut().zip(dom) {
        let base = t.speaker.clone();
        let maj = major.get(&base).copied().flatten();
        if split.contains_key(&base) {
            let src = d.or_else(|| current.get(&base).copied()).or(maj).unwrap_or(Role::Mic);
            current.insert(base.clone(), src);
            t.source = Some(src);
            t.speaker = format!("{base}~{}", src.stem());
        } else {
            t.source = maj;
        }
    }
}

// ---------------------------------------------------------------------------
// Segmentation et noms
// ---------------------------------------------------------------------------
/// `breaks` : instants où une autre piste commence un mot (triés).
fn segment(toks: &[Token], next_id: &mut u32, breaks: &[f64]) -> Vec<Segment> {
    // Une autre piste a-t-elle pris la parole dans le blanc ]last_end, start[ ?
    let interrupted = |last_end: f64, start: f64| {
        let i = breaks.partition_point(|&b| b < last_end - 0.05);
        breaks.get(i).map(|&b| b < start).unwrap_or(false)
    };
    let mut out: Vec<Segment> = vec![];
    let mut cur: Option<Segment> = None;
    let mut last_end = 0.0;
    for t in toks {
        if t.kind == Kind::Spacing {
            if let Some(c) = cur.as_mut() {
                c.text.push_str(&t.text);
            }
            continue;
        }
        let new = match &cur {
            None => true,
            Some(c) => {
                c.speaker != t.speaker
                    || (t.start - last_end > PAUSE_SPLIT && !c.text.trim().is_empty())
                    || (!breaks.is_empty() && !c.text.trim().is_empty() && interrupted(last_end, t.start))
                    || (c.text.len() > LONG_PARA && c.text.trim_end().ends_with(['.', '!', '?', '…']))
            }
        };
        if new {
            if let Some(c) = cur.take() {
                out.push(c);
            }
            cur = Some(Segment { id: *next_id, speaker: t.speaker.clone(), source: t.source, start: t.start, end: t.end, text: String::new() });
            *next_id += 1;
        }
        let c = cur.as_mut().unwrap();
        if c.text.is_empty() {
            c.text.push_str(t.text.trim_start());
        } else {
            c.text.push_str(&t.text);
        }
        c.end = c.end.max(t.end);
        last_end = t.end;
    }
    if let Some(c) = cur {
        out.push(c);
    }
    for s in &mut out {
        s.text = s.text.split_whitespace().collect::<Vec<_>>().join(" ");
    }
    out.retain(|s| !s.text.is_empty());
    out
}

/// Noms par défaut : ton prénom pour l'unique voix du micro, « Interlocuteur »
/// côté PC, « Locuteur N » quand la source est inconnue.
pub fn default_names(speakers: &mut [Speaker], my_name: &str) {
    let count = |r: Option<Role>| speakers.iter().filter(|s| s.source == r).count();
    let (n_mic, n_pc, n_none) = (count(Some(Role::Mic)), count(Some(Role::Pc)), count(None));
    let (mut i_mic, mut i_pc, mut i_none) = (0, 0, 0);
    for s in speakers.iter_mut() {
        s.name = match s.source {
            Some(Role::Mic) => {
                i_mic += 1;
                if n_mic == 1 {
                    my_name.to_string()
                } else {
                    format!("Micro {i_mic}")
                }
            }
            Some(Role::Pc) => {
                i_pc += 1;
                if n_pc == 1 {
                    "Interlocuteur".into()
                } else {
                    format!("Interlocuteur {i_pc}")
                }
            }
            None => {
                i_none += 1;
                if n_none == 1 && speakers_len_hint(n_mic, n_pc) {
                    "Locuteur".into()
                } else {
                    format!("Locuteur {i_none}")
                }
            }
        };
    }
}

fn speakers_len_hint(n_mic: usize, n_pc: usize) -> bool {
    n_mic + n_pc == 0
}

pub struct Input<'a> {
    pub mode: Mode,
    /// Une réponse par piste transcrite, avec sa source si elle est connue.
    pub responses: Vec<(Option<Role>, &'a Value)>,
    pub levels: Option<Levels<'a>>,
    pub my_name: &'a str,
    pub options: Value,
    pub created: String,
    pub notes: Vec<String>,
}

pub fn build(input: Input) -> Transcript {
    let mut next_id = 1u32;
    let mut segments = vec![];
    let mut language = None;
    // Pistes séparées : un paragraphe est coupé là où l'autre piste prend la
    // parole pendant un blanc, pour que les répliques s'intercalent au bon
    // endroit. Un vrai chevauchement (les deux parlent) ne coupe rien.
    let starts_of = |resp: &Value| -> Vec<f64> {
        let mut v: Vec<f64> = tokens(resp, "", None).iter().filter(|t| t.kind == Kind::Word).map(|t| t.start).collect();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        v
    };
    for (k, (src, resp)) in input.responses.iter().enumerate() {
        if language.is_none() {
            language = resp.get("language_code").and_then(Value::as_str).map(String::from);
        }
        let prefix = match (input.mode, src) {
            (Mode::Separate, Some(r)) => format!("{}:", r.stem()),
            _ => String::new(),
        };
        let mut toks = tokens(resp, &prefix, if input.mode == Mode::Separate { *src } else { None });
        if input.mode == Mode::Mixed {
            if let Some(lv) = &input.levels {
                attribute(&mut toks, lv);
            }
        }
        let mut breaks = vec![];
        if input.mode == Mode::Separate {
            for (j, (_, other)) in input.responses.iter().enumerate() {
                if j != k {
                    breaks.extend(starts_of(other));
                }
            }
            breaks.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        }
        segments.extend(segment(&toks, &mut next_id, &breaks));
    }
    segments.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap().then(a.id.cmp(&b.id)));
    for (i, s) in segments.iter_mut().enumerate() {
        s.id = i as u32 + 1;
    }
    let mut speakers: Vec<Speaker> = vec![];
    for s in &segments {
        if !speakers.iter().any(|k| k.id == s.speaker) {
            speakers.push(Speaker { id: s.speaker.clone(), name: String::new(), source: s.source });
        }
    }
    default_names(&mut speakers, input.my_name);
    Transcript {
        v: 1,
        created: input.created,
        mode: input.mode,
        language,
        options: input.options,
        speakers,
        segments,
        notes: input.notes,
    }
}

// ---------------------------------------------------------------------------
// Corrections
// ---------------------------------------------------------------------------
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Attribution {
    pub speakers: Vec<Speaker>,
    /// segment → locuteur
    pub assign: BTreeMap<u32, String>,
}

impl Transcript {
    pub fn attribution(&self) -> Attribution {
        Attribution { speakers: self.speakers.clone(), assign: self.segments.iter().map(|s| (s.id, s.speaker.clone())).collect() }
    }

    /// Applique une attribution complète venue de l'interface, après contrôle.
    /// Les locuteurs sans segment disparaissent ; le texte n'est jamais touché.
    pub fn apply(&mut self, a: &Attribution) -> Result<(), String> {
        let mut seen = std::collections::HashSet::new();
        for s in &a.speakers {
            if s.id.is_empty() || s.id.len() > 64 || !seen.insert(s.id.clone()) {
                return Err("identifiant de locuteur invalide".into());
            }
        }
        for seg in &self.segments {
            match a.assign.get(&seg.id) {
                Some(spk) if seen.contains(spk) => {}
                _ => return Err(format!("segment {} sans locuteur valide", seg.id)),
            }
        }
        if a.assign.len() != self.segments.len() {
            return Err("segments inconnus".into());
        }
        for seg in &mut self.segments {
            seg.speaker = a.assign[&seg.id].clone();
        }
        let used: std::collections::HashSet<&String> = self.segments.iter().map(|s| &s.speaker).collect();
        self.speakers = a
            .speakers
            .iter()
            .filter(|s| used.contains(&s.id))
            .map(|s| Speaker {
                id: s.id.clone(),
                name: {
                    let n: String = s.name.trim().chars().filter(|c| !c.is_control()).take(40).collect();
                    if n.is_empty() {
                        "Sans nom".into()
                    } else {
                        n
                    }
                },
                source: s.source,
            })
            .collect();
        Ok(())
    }

    fn name(&self, id: &str) -> &str {
        self.speakers.iter().find(|s| s.id == id).map(|s| s.name.as_str()).unwrap_or("?")
    }

    /// Tours de parole : segments consécutifs du même locuteur.
    pub fn turns(&self) -> Vec<(&str, f64, Vec<&str>)> {
        let mut out: Vec<(&str, f64, Vec<&str>)> = vec![];
        for s in &self.segments {
            match out.last_mut() {
                Some(t) if t.0 == s.speaker => t.2.push(&s.text),
                _ => out.push((&s.speaker, s.start, vec![&s.text])),
            }
        }
        out
    }

    pub fn plain_text(&self) -> String {
        self.segments.iter().map(|s| s.text.as_str()).collect::<Vec<_>>().join(" ")
    }
}

// ---------------------------------------------------------------------------
// Exports
// ---------------------------------------------------------------------------
pub fn clock(t: f64) -> String {
    let s = t.max(0.0) as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{:02}:{:02}", s / 60, s % 60)
    }
}

pub fn export_txt(t: &Transcript, title: &str, duration: Option<f64>) -> String {
    let names: Vec<&str> = t.speakers.iter().map(|s| s.name.as_str()).collect();
    let mut out = format!("{title}\n");
    if let Some(d) = duration {
        out.push_str(&format!("Durée {} · ", clock(d)));
    }
    out.push_str(&format!("Participants : {}\n\n", names.join(", ")));
    for (spk, start, paras) in t.turns() {
        out.push_str(&format!("[{}] {}\n{}\n\n", clock(start), t.name(spk), paras.join("\n\n")));
    }
    out.trim_end().to_string() + "\n"
}

pub fn export_md(t: &Transcript, title: &str, duration: Option<f64>) -> String {
    let names: Vec<&str> = t.speakers.iter().map(|s| s.name.as_str()).collect();
    let mut out = format!("# {title}\n\n");
    if let Some(d) = duration {
        out.push_str(&format!("*Durée {}* · ", clock(d)));
    }
    out.push_str(&format!("*Participants : {}*\n\n", names.join(", ")));
    for (spk, start, paras) in t.turns() {
        out.push_str(&format!("**{}** · {}\n\n{}\n\n", t.name(spk), clock(start), paras.join("\n\n")));
    }
    out.trim_end().to_string() + "\n"
}

fn srt_time(t: f64) -> String {
    let ms = (t.max(0.0) * 1000.0).round() as u64;
    format!("{:02}:{:02}:{:02},{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
}

/// Sous-titres : chaque segment découpé en blocs de 84 caractères au plus,
/// le temps réparti au prorata du texte.
pub fn export_srt(t: &Transcript) -> String {
    let mut out = String::new();
    let mut n = 1;
    for s in &t.segments {
        let mut chunks: Vec<String> = vec![];
        let mut cur = String::new();
        for w in s.text.split(' ') {
            if !cur.is_empty() && cur.len() + 1 + w.len() > 84 {
                chunks.push(std::mem::take(&mut cur));
            }
            if !cur.is_empty() {
                cur.push(' ');
            }
            cur.push_str(w);
        }
        if !cur.is_empty() {
            chunks.push(cur);
        }
        let total: usize = chunks.iter().map(|c| c.len()).sum::<usize>().max(1);
        let span = (s.end - s.start).max(0.5);
        let mut at = s.start;
        for (i, c) in chunks.iter().enumerate() {
            let d = span * c.len() as f64 / total as f64;
            let label = if i == 0 { format!("{} : ", t.name(&s.speaker)) } else { String::new() };
            out.push_str(&format!("{n}\n{} --> {}\n{label}{c}\n\n", srt_time(at), srt_time(at + d)));
            at += d;
            n += 1;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::quantize_db;
    use serde_json::json;

    /// Réponse ElevenLabs simulée : (locuteur, début, fin, mot).
    fn resp(words: &[(&str, f64, f64, &str)]) -> Value {
        let mut v = vec![];
        for (i, (s, a, b, w)) in words.iter().enumerate() {
            if i > 0 {
                v.push(json!({"text": " ", "type": "spacing", "start": a, "end": a, "speaker_id": s}));
            }
            v.push(json!({"text": w, "type": "word", "start": a, "end": b, "speaker_id": s}));
        }
        v.push(json!({"text": "(rires)", "type": "audio_event", "start": 99.0, "end": 99.5, "speaker_id": "speaker_0"}));
        json!({"language_code": "fra", "text": "", "words": v})
    }

    fn input<'a>(mode: Mode, responses: Vec<(Option<Role>, &'a Value)>, levels: Option<Levels<'a>>) -> Input<'a> {
        Input { mode, responses, levels, my_name: "Manuel", options: json!({}), created: "t".into(), notes: vec![] }
    }

    #[test]
    fn segmentation_pauses_et_typographie() {
        let r = resp(&[
            ("speaker_0", 0.0, 0.3, "Bonjour,"),
            ("speaker_0", 0.4, 0.8, "on"),
            ("speaker_0", 0.9, 1.2, "commence ?"),
            ("speaker_1", 2.0, 2.3, "Oui."),
            ("speaker_1", 5.0, 5.3, "Après"),
            ("speaker_1", 5.4, 5.7, "pause."),
        ]);
        let t = build(input(Mode::Single, vec![(None, &r)], None));
        assert_eq!(t.segments.len(), 3);
        assert_eq!(t.segments[0].text, "Bonjour, on commence ?");
        assert_eq!(t.segments[2].text, "Après pause.");
        assert_eq!(t.speakers.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["Locuteur 1", "Locuteur 2"]);
        assert_eq!(t.turns().len(), 2, "deux paragraphes du même locuteur forment un tour");
        assert_eq!(t.language.as_deref(), Some("fra"));
        assert!(!t.plain_text().contains("rires"));
    }

    /// Échange rapide sur pistes séparées : la réplique du PC s'intercale au
    /// milieu du tour du micro, au lieu d'être repoussée après.
    #[test]
    fn pistes_separees_echange_rapide() {
        let mic = resp(&[("speaker_0", 0.0, 1.0, "Bonjour"), ("speaker_0", 2.2, 2.6, "ça"), ("speaker_0", 2.6, 3.0, "va ?")]);
        let pc = resp(&[("speaker_0", 1.1, 2.0, "Salut")]);
        let t = build(input(Mode::Separate, vec![(Some(Role::Mic), &mic), (Some(Role::Pc), &pc)], None));
        let texts: Vec<&str> = t.segments.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, ["Bonjour", "Salut", "ça va ?"]);
        assert_eq!(t.segments[1].source, Some(Role::Pc));
        assert_eq!(t.speakers.len(), 2);
    }

    /// Pistes séparées : deux personnes au micro, deux côté PC, avec
    /// chevauchement. Personne n'est fusionné, l'ordre suit le temps.
    #[test]
    fn pistes_separees_plusieurs_personnes_par_source() {
        let mic = resp(&[
            ("speaker_0", 0.0, 0.5, "Salut"),
            ("speaker_0", 0.6, 1.0, "tout"),
            ("speaker_0", 1.1, 1.4, "le monde."),
            ("speaker_1", 3.0, 3.4, "Moi"),
            ("speaker_1", 3.5, 3.9, "aussi."),
        ]);
        let pc = resp(&[
            ("speaker_0", 0.8, 1.2, "Bonjour"),
            ("speaker_0", 1.3, 1.6, "Paris."),
            ("speaker_1", 2.0, 2.5, "Coucou."),
        ]);
        let t = build(input(Mode::Separate, vec![(Some(Role::Mic), &mic), (Some(Role::Pc), &pc)], None));
        let names: Vec<_> = t.speakers.iter().map(|s| (s.id.as_str(), s.name.as_str())).collect();
        assert_eq!(
            names,
            [("micro:speaker_0", "Micro 1"), ("pc:speaker_0", "Interlocuteur 1"), ("pc:speaker_1", "Interlocuteur 2"), ("micro:speaker_1", "Micro 2")]
        );
        let starts: Vec<f64> = t.segments.iter().map(|s| s.start).collect();
        assert!(starts.windows(2).all(|w| w[0] <= w[1]), "ordre chronologique");
        assert!(t.segments.iter().any(|s| s.text == "Bonjour Paris."), "le chevauchement n'efface rien");
    }

    #[test]
    fn noms_par_defaut() {
        let mic = resp(&[("speaker_0", 0.0, 0.5, "Allô ?")]);
        let pc = resp(&[("speaker_0", 1.0, 1.5, "Oui ?")]);
        let t = build(input(Mode::Separate, vec![(Some(Role::Mic), &mic), (Some(Role::Pc), &pc)], None));
        assert_eq!(t.speakers[0].name, "Manuel");
        assert_eq!(t.speakers[1].name, "Interlocuteur");
        let t = build(input(Mode::Single, vec![(None, &mic)], None));
        assert_eq!(t.speakers[0].name, "Locuteur");
    }

    /// Mode mixé : la diarisation a confondu ma voix et celle de l'interlocuteur
    /// (même locuteur) ; les niveaux des pistes la séparent. Un deuxième
    /// interlocuteur côté PC reste distinct.
    #[test]
    fn mixe_scission_par_les_niveaux() {
        let fps = ENV_FPS as f64;
        let dur = 40.0;
        let n = (dur * fps) as usize;
        let mut mic = vec![quantize_db(-70.0); n];
        let mut pc = vec![quantize_db(-75.0); n];
        let mut words = vec![];
        // 0-10 s : moi (micro) ; 10-20 s : distant A (PC) ; 20-30 : moi ; 30-40 : distant B.
        for k in 0..40 {
            let a = k as f64;
            let me = (k / 10) % 2 == 0;
            let (spk, env) = if k >= 30 { ("speaker_1", &mut pc) } else if me { ("speaker_0", &mut mic) } else { ("speaker_0", &mut pc) };
            for i in (a * fps) as usize..((a + 0.8) * fps) as usize {
                env[i] = quantize_db(-22.0);
            }
            words.push((spk, a, a + 0.8, "mot"));
        }
        let r = resp(&words);
        let lv = Levels { mic: &mic, pc: &pc, mic_gain_db: 0.0, pc_gain_db: 0.0 };
        let t = build(input(Mode::Mixed, vec![(None, &r)], Some(lv)));
        let ids: Vec<(&str, Option<Role>)> = t.speakers.iter().map(|s| (s.id.as_str(), s.source)).collect();
        assert_eq!(ids, [("speaker_0~micro", Some(Role::Mic)), ("speaker_0~pc", Some(Role::Pc)), ("speaker_1", Some(Role::Pc))]);
        assert_eq!(t.speakers[0].name, "Manuel");
        assert_eq!(t.speakers[1].name, "Interlocuteur 1");
        assert_eq!(t.speakers[2].name, "Interlocuteur 2");
        let s20 = t.segments.iter().find(|s| s.start >= 20.0).unwrap();
        assert_eq!(s20.speaker, "speaker_0~micro");
    }

    /// Plusieurs personnes autour du micro, PC muet : un seul groupe, sans scission.
    #[test]
    fn mixe_plusieurs_personnes_au_micro() {
        let n = 20 * ENV_FPS as usize;
        let mic = vec![quantize_db(-22.0); n];
        let pc = vec![quantize_db(-80.0); n];
        let words: Vec<_> = (0..20).map(|k| (if k % 4 < 2 { "speaker_0" } else { "speaker_1" }, k as f64, k as f64 + 0.8, "mot")).collect();
        let r = resp(&words);
        let lv = Levels { mic: &mic, pc: &pc, mic_gain_db: 0.0, pc_gain_db: 0.0 };
        let t = build(input(Mode::Mixed, vec![(None, &r)], Some(lv)));
        assert_eq!(t.speakers.len(), 2);
        assert!(t.speakers.iter().all(|s| s.source == Some(Role::Mic)));
        assert_eq!(t.speakers[0].name, "Micro 1");
    }

    #[test]
    fn corrections_controlees() {
        let r = resp(&[("speaker_0", 0.0, 0.5, "A."), ("speaker_1", 2.0, 2.5, "B."), ("speaker_2", 5.0, 5.5, "C.")]);
        let mut t = build(input(Mode::Single, vec![(None, &r)], None));
        let mut a = t.attribution();
        // Fusion de speaker_2 dans speaker_0, renommage, segment 2 vers un nouveau locuteur.
        for v in a.assign.values_mut() {
            if v == "speaker_2" {
                *v = "speaker_0".into();
            }
        }
        a.speakers[0].name = "  Claire\u{7} ".into();
        a.speakers.push(Speaker { id: "nouveau".into(), name: "Tangui".into(), source: None });
        a.assign.insert(2, "nouveau".into());
        t.apply(&a).unwrap();
        assert_eq!(t.speakers.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["Claire", "Tangui"]);
        assert_eq!(t.segments[2].speaker, "speaker_0");
        let txt = export_txt(&t, "Réunion", Some(65.0));
        assert!(txt.contains("Durée 01:05") && txt.contains("[00:02] Tangui\nB."), "{txt}");
        // Refus : locuteur inconnu, segment manquant.
        let mut bad = t.attribution();
        bad.assign.insert(1, "fantome".into());
        assert!(t.apply(&bad).is_err());
        let mut bad = t.attribution();
        bad.assign.remove(&3);
        assert!(t.apply(&bad).is_err());
        assert_eq!(t.segments[0].text, "A.", "texte intact");
    }

    #[test]
    fn exports_md_srt() {
        let long = (0..40).map(|i| format!("mot{i}")).collect::<Vec<_>>().join(" ");
        let mut words = vec![("speaker_0", 0.0, 0.5, "Bonjour.")];
        let leaked: &'static str = Box::leak(long.into_boxed_str());
        words.push(("speaker_1", 1.0, 13.0, leaked));
        let r = resp(&words);
        let t = build(input(Mode::Single, vec![(None, &r)], None));
        let md = export_md(&t, "Titre", None);
        assert!(md.starts_with("# Titre\n") && md.contains("**Locuteur 1** · 00:00"));
        let srt = export_srt(&t);
        let cues: Vec<&str> = srt.split("\n\n").filter(|c| !c.is_empty()).collect();
        assert!(cues.len() >= 4, "{srt}");
        assert!(cues[0].starts_with("1\n00:00:00,000 --> 00:00:00,500\nLocuteur 1 : Bonjour."));
        assert!(cues.iter().all(|c| c.lines().last().unwrap().len() <= 100));
        assert_eq!(srt_time(3725.5), "01:02:05,500");
        assert_eq!(clock(3725.0), "1:02:05");
    }
}
