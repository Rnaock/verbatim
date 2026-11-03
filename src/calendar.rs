//! Agenda : lecture de l'adresse iCal secrète (Google Agenda, Outlook…).
//!
//! Lecture seule. L'adresse est un secret (quiconque la connaît lit l'agenda) :
//! elle est chiffrée par DPAPI comme la clé ElevenLabs et ne repart jamais vers
//! l'interface. Seuls les événements proches de maintenant sont exposés.

use crate::core::{write_atomic, ApiError, Paths, Res};
use crate::vault;
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use chrono::{DateTime, Duration as CDuration, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::PathBuf;

/// Un événement est « maintenant » de 10 min avant son début jusqu'à sa fin.
pub const LEAD_MIN: i64 = 10;
const MAX_FEED: usize = 30 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Adresse secrète
// ---------------------------------------------------------------------------
fn store_path(p: &Paths) -> PathBuf {
    p.data_dir.join("agenda.dpapi")
}

pub fn load_url(p: &Paths) -> Option<String> {
    let raw = fs::read(store_path(p)).ok()?;
    let blob = B64.decode(raw.trim_ascii()).ok()?;
    String::from_utf8(vault::unprotect(&blob).ok()?).ok().filter(|u| !u.is_empty())
}

pub fn save_url(p: &Paths, url: Option<&str>) -> io::Result<()> {
    match url {
        Some(u) => write_atomic(&store_path(p), B64.encode(vault::protect(u.as_bytes())?).as_bytes()),
        None => match fs::remove_file(store_path(p)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        },
    }
}

/// Vérifie la forme de l'adresse. `allow_local` : http://127.0.0.1 (tests).
pub fn validate_url(url: &str, allow_local: bool) -> Result<String, String> {
    let u = url.trim();
    let u = u.strip_prefix("webcal://").map(|r| format!("https://{r}")).unwrap_or_else(|| u.to_string());
    let parsed = reqwest::Url::parse(&u).map_err(|_| "adresse invalide".to_string())?;
    let local = allow_local && parsed.scheme() == "http" && parsed.host_str() == Some("127.0.0.1");
    if parsed.scheme() != "https" && !local {
        return Err("l'adresse doit commencer par https://".into());
    }
    if u.len() > 2000 {
        return Err("adresse trop longue".into());
    }
    Ok(u)
}

/// Compte de l'agenda, lu dans l'adresse Google (…/calendar/ical/<compte>/private-…).
pub fn account_of(url: &str) -> Option<String> {
    let after = url.split("/calendar/ical/").nth(1)?;
    let enc = after.split('/').next()?;
    let dec = percent_decode(enc);
    dec.contains('@').then(|| dec.to_lowercase())
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Version masquée pour l'affichage : on montre le compte, jamais le jeton.
#[derive(Serialize, Clone, Debug)]
pub struct CalState {
    pub connected: bool,
    pub account: Option<String>,
}

pub fn state(url: Option<&str>) -> CalState {
    CalState { connected: url.is_some(), account: url.and_then(account_of) }
}

pub async fn fetch(http: &reqwest::Client, url: &str) -> Res<String> {
    use futures_util::StreamExt;
    let r = http
        .get(url)
        .timeout(std::time::Duration::from_secs(25))
        .send()
        .await
        .map_err(|e| ApiError::new(0, format!("agenda injoignable : {e}")))?;
    let st = r.status().as_u16();
    if st == 404 || st == 401 || st == 403 {
        return Err(ApiError::new(st as i32, "adresse de l'agenda refusée : elle a peut-être été réinitialisée dans Google Agenda"));
    }
    if !r.status().is_success() {
        return Err(ApiError::new(st as i32, "l'agenda ne répond pas comme prévu"));
    }
    let mut buf = Vec::new();
    let mut s = r.bytes_stream();
    while let Some(c) = s.next().await {
        let c = c.map_err(|e| ApiError::new(0, e.to_string()))?;
        buf.extend_from_slice(&c);
        if buf.len() > MAX_FEED {
            return Err(ApiError::new(413, "agenda trop volumineux"));
        }
    }
    let text = String::from_utf8_lossy(&buf).into_owned();
    if !text.contains("BEGIN:VCALENDAR") {
        return Err(ApiError::new(415, "cette adresse ne renvoie pas un agenda iCal"));
    }
    Ok(text)
}

// ---------------------------------------------------------------------------
// Lecture du format iCalendar (RFC 5545), juste ce qu'il faut
// ---------------------------------------------------------------------------
#[derive(Debug, Clone)]
struct Prop {
    name: String,
    params: BTreeMap<String, String>,
    value: String,
    /// Ligne d'origine dépliée (pour la règle de répétition).
    raw: String,
}

fn unfold(text: &str) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if (line.starts_with(' ') || line.starts_with('\t')) && !out.is_empty() {
            out.last_mut().unwrap().push_str(&line[1..]);
        } else if !line.is_empty() {
            out.push(line.to_string());
        }
    }
    out
}

fn parse_line(line: &str) -> Option<Prop> {
    // Séparateur nom/paramètres : le premier « : » hors guillemets.
    let mut in_q = false;
    let mut colon = None;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_q = !in_q,
            ':' if !in_q => {
                colon = Some(i);
                break;
            }
            _ => {}
        }
    }
    let colon = colon?;
    let (head, value) = (&line[..colon], &line[colon + 1..]);
    let mut parts = vec![];
    let (mut cur, mut in_q) = (String::new(), false);
    for c in head.chars() {
        match c {
            '"' => in_q = !in_q,
            ';' if !in_q => parts.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    parts.push(cur);
    let name = parts.remove(0).to_ascii_uppercase();
    let params = parts
        .into_iter()
        .filter_map(|p| p.split_once('=').map(|(k, v)| (k.to_ascii_uppercase(), v.to_string())))
        .collect();
    Some(Prop { name, params, value: value.to_string(), raw: line.to_string() })
}

fn unescape(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut it = v.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next() {
                Some('n') | Some('N') => out.push('\n'),
                Some(x) => out.push(x),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Instant d'une valeur DATE-TIME / DATE. Renvoie (instant, jour entier).
fn parse_dt(p: &Prop) -> Option<(DateTime<Utc>, bool)> {
    let v = p.value.trim();
    if p.params.get("VALUE").map(|x| x == "DATE").unwrap_or(false) || (v.len() == 8 && v.chars().all(|c| c.is_ascii_digit())) {
        let d = NaiveDate::parse_from_str(v, "%Y%m%d").ok()?;
        let local = Local.from_local_datetime(&d.and_hms_opt(0, 0, 0)?).earliest()?;
        return Some((local.with_timezone(&Utc), true));
    }
    if let Some(z) = v.strip_suffix('Z') {
        let n = NaiveDateTime::parse_from_str(z, "%Y%m%dT%H%M%S").ok()?;
        return Some((Utc.from_utc_datetime(&n), false));
    }
    let n = NaiveDateTime::parse_from_str(v, "%Y%m%dT%H%M%S").ok()?;
    let at = match p.params.get("TZID").map(|t| t.trim_matches('"')).and_then(|t| t.parse::<chrono_tz::Tz>().ok()) {
        Some(tz) => tz.from_local_datetime(&n).earliest()?.with_timezone(&Utc),
        None => Local.from_local_datetime(&n).earliest()?.with_timezone(&Utc),
    };
    Some((at, false))
}

/// « PT1H30M », « P1D »…
fn parse_duration(v: &str) -> Option<CDuration> {
    let v = v.trim().trim_start_matches('+');
    let v = v.strip_prefix('P')?;
    let (mut secs, mut num, mut in_time) = (0i64, String::new(), false);
    for c in v.chars() {
        match c {
            'T' => in_time = true,
            '0'..='9' => num.push(c),
            _ => {
                let n: i64 = num.parse().ok()?;
                num.clear();
                secs += n * match (c, in_time) {
                    ('W', _) => 7 * 86400,
                    ('D', _) => 86400,
                    ('H', true) => 3600,
                    ('M', true) => 60,
                    ('S', true) => 1,
                    _ => return None,
                };
            }
        }
    }
    Some(CDuration::seconds(secs))
}

#[derive(Debug, Clone)]
pub struct Attendee {
    pub email: String,
    pub cn: Option<String>,
    pub declined: bool,
}

fn attendee(p: &Prop) -> Option<Attendee> {
    let v = p.value.trim();
    let email = v.strip_prefix("mailto:").or_else(|| v.strip_prefix("MAILTO:")).unwrap_or(v).trim().to_lowercase();
    if !email.contains('@') {
        return None;
    }
    let cn = p.params.get("CN").map(|c| c.trim_matches('"').trim().to_string()).filter(|c| !c.is_empty());
    let declined = p.params.get("PARTSTAT").map(|s| s.eq_ignore_ascii_case("DECLINED")).unwrap_or(false);
    Some(Attendee { email, cn, declined })
}

#[derive(Debug, Clone, Default)]
pub struct VEvent {
    pub uid: String,
    pub summary: String,
    start: Option<(DateTime<Utc>, bool)>,
    end: Option<DateTime<Utc>>,
    duration: Option<CDuration>,
    recurrence_id: Option<DateTime<Utc>>,
    cancelled: bool,
    rule_lines: Vec<String>,
    dtstart_raw: String,
    pub attendees: Vec<Attendee>,
    pub organizer: Option<Attendee>,
    description: String,
    location: String,
    conference: Option<String>,
}

pub fn parse(text: &str) -> Vec<VEvent> {
    let mut out = vec![];
    let mut cur: Option<VEvent> = None;
    let mut depth = 0; // composants imbriqués (VALARM) ignorés
    for line in unfold(text) {
        let Some(p) = parse_line(&line) else { continue };
        match (p.name.as_str(), p.value.trim()) {
            ("BEGIN", "VEVENT") => {
                cur = Some(VEvent::default());
                depth = 0;
                continue;
            }
            ("END", "VEVENT") => {
                if let Some(e) = cur.take() {
                    if e.start.is_some() {
                        out.push(e);
                    }
                }
                continue;
            }
            ("BEGIN", _) if cur.is_some() => {
                depth += 1;
                continue;
            }
            ("END", _) if cur.is_some() => {
                depth -= 1;
                continue;
            }
            _ => {}
        }
        let Some(e) = cur.as_mut() else { continue };
        if depth > 0 {
            continue;
        }
        match p.name.as_str() {
            "UID" => e.uid = p.value.trim().to_string(),
            "SUMMARY" => e.summary = unescape(&p.value).trim().to_string(),
            "DTSTART" => {
                e.start = parse_dt(&p);
                e.dtstart_raw = p.raw.clone();
            }
            "DTEND" => e.end = parse_dt(&p).map(|x| x.0),
            "DURATION" => e.duration = parse_duration(&p.value),
            "RECURRENCE-ID" => e.recurrence_id = parse_dt(&p).map(|x| x.0),
            "STATUS" => e.cancelled = p.value.trim().eq_ignore_ascii_case("CANCELLED"),
            "RRULE" | "EXDATE" | "RDATE" => e.rule_lines.push(p.raw.clone()),
            "ATTENDEE" => e.attendees.extend(attendee(&p)),
            "ORGANIZER" => e.organizer = attendee(&p),
            "DESCRIPTION" => e.description = unescape(&p.value),
            "LOCATION" => e.location = unescape(&p.value),
            "X-GOOGLE-CONFERENCE" | "URL" if e.conference.is_none() => {
                let v = p.value.trim();
                if meeting_link(v).is_some() {
                    e.conference = Some(v.to_string());
                }
            }
            _ => {}
        }
    }
    out
}

/// Lien de visio reconnu dans un texte (Meet, Zoom, Teams).
pub fn meeting_link(text: &str) -> Option<String> {
    const PREFIXES: &[&str] = &["https://meet.google.com/", "https://teams.microsoft.com/", "https://teams.live.com/"];
    for word in text.split(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\'' | '(' | ')' | '[' | ']')) {
        let w = word.trim_end_matches(['.', ',', ';']);
        if PREFIXES.iter().any(|p| w.starts_with(p) && w.len() > p.len()) || is_zoom(w) {
            return Some(w.to_string());
        }
    }
    None
}

fn is_zoom(w: &str) -> bool {
    reqwest::Url::parse(w)
        .ok()
        .filter(|u| u.scheme() == "https")
        .and_then(|u| u.host_str().map(|h| h == "zoom.us" || h.ends_with(".zoom.us")))
        .unwrap_or(false)
        && w.contains("/j/")
}

/// Liens que Verbatim accepte d'ouvrir dans le navigateur.
pub fn is_meeting_url(url: &str) -> bool {
    meeting_link(url).as_deref() == Some(url)
}

// ---------------------------------------------------------------------------
// Occurrences proches de maintenant
// ---------------------------------------------------------------------------
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Person {
    pub email: String,
    pub name: String,
    /// Nom deviné depuis l'adresse (à confirmer).
    #[serde(default)]
    pub guessed: bool,
    #[serde(default)]
    pub me: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CalEvent {
    pub uid: String,
    pub title: String,
    /// Secondes depuis 1970 (UTC).
    pub start: i64,
    pub end: i64,
    pub people: Vec<Person>,
    pub meet: Option<String>,
}

/// « jean.dupont@x.fr » → « Jean Dupont » (deviné).
pub fn guess_name(email: &str) -> String {
    let local = email.split('@').next().unwrap_or(email);
    let words: Vec<String> = local
        .split(['.', '_', '-', '+'])
        .filter(|w| !w.is_empty() && !w.chars().all(|c| c.is_ascii_digit()))
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + &c.as_str().to_lowercase(),
                None => String::new(),
            }
        })
        .collect();
    if words.is_empty() {
        local.to_string()
    } else {
        words.join(" ")
    }
}

pub fn resolve(email: &str, cn: Option<&str>, people: &BTreeMap<String, String>) -> (String, bool) {
    if let Some(n) = people.get(email) {
        return (n.clone(), false);
    }
    match cn.filter(|c| !c.contains('@') && !c.trim().is_empty()) {
        Some(c) => (c.trim().chars().take(40).collect(), false),
        None => (guess_name(email), true),
    }
}

fn occurrences(e: &VEvent, from: DateTime<Utc>, to: DateTime<Utc>) -> Vec<DateTime<Utc>> {
    let Some((start, _)) = e.start else { return vec![] };
    let has_rule = e.rule_lines.iter().any(|l| l.to_ascii_uppercase().starts_with("RRULE"));
    if !has_rule || e.recurrence_id.is_some() {
        return vec![start];
    }
    let mut text = e.dtstart_raw.clone();
    for l in &e.rule_lines {
        text.push('\n');
        text.push_str(l);
    }
    let set = match text.parse::<rrule::RRuleSet>() {
        Ok(s) => s,
        Err(_) => return vec![start], // règle illisible : au moins la première occurrence
    };
    let tz = rrule::Tz::UTC;
    set.after(from.with_timezone(&tz))
        .before(to.with_timezone(&tz))
        .all(400)
        .dates
        .into_iter()
        .map(|d| d.with_timezone(&Utc))
        .collect()
}

/// Événements qui chevauchent [from, to], répétitions et exceptions comprises.
pub fn events_between(events: &[VEvent], from: DateTime<Utc>, to: DateTime<Utc>, me: Option<&str>, people: &BTreeMap<String, String>) -> Vec<CalEvent> {
    // Exceptions : (uid, instant d'origine) remplacées par leur propre VEVENT.
    let overridden: Vec<(&str, DateTime<Utc>)> = events.iter().filter_map(|e| e.recurrence_id.map(|r| (e.uid.as_str(), r))).collect();
    let mut out = vec![];
    for e in events {
        let Some((start, all_day)) = e.start else { continue };
        if all_day || e.cancelled {
            continue;
        }
        let len = match (e.end, e.duration) {
            (Some(end), _) if end > start => end - start,
            (_, Some(d)) if d > CDuration::zero() => d,
            _ => CDuration::hours(1),
        };
        if len > CDuration::hours(14) {
            continue; // blocs de journée, voyages…
        }
        let me_declined = e.attendees.iter().any(|a| Some(a.email.as_str()) == me && a.declined);
        if me_declined {
            continue;
        }
        for s in occurrences(e, from - len, to) {
            if e.recurrence_id.is_none() && overridden.iter().any(|(u, r)| *u == e.uid && *r == s) {
                continue;
            }
            let end = s + len;
            if end <= from || s >= to {
                continue;
            }
            let mut ppl: Vec<Person> = vec![];
            let mut add = |a: &Attendee| {
                if a.declined || ppl.iter().any(|p| p.email == a.email) {
                    return;
                }
                let is_me = Some(a.email.as_str()) == me;
                let (name, guessed) = resolve(&a.email, a.cn.as_deref(), people);
                ppl.push(Person { email: a.email.clone(), name, guessed, me: is_me });
            };
            if let Some(o) = &e.organizer {
                add(o);
            }
            e.attendees.iter().for_each(&mut add);
            // Salles et ressources Google : pas des personnes.
            ppl.retain(|p| !p.email.ends_with("resource.calendar.google.com") && !p.email.contains("group.calendar.google.com"));
            let meet = e.conference.clone().or_else(|| meeting_link(&e.location)).or_else(|| meeting_link(&e.description));
            out.push(CalEvent {
                uid: format!("{}@{}", e.uid, s.timestamp()),
                title: if e.summary.is_empty() { "Rendez-vous".into() } else { e.summary.chars().take(120).collect() },
                start: s.timestamp(),
                end: end.timestamp(),
                people: ppl,
                meet,
            });
        }
    }
    out.sort_by_key(|e| (e.start, e.end));
    out
}

/// Ce qui compte pour démarrer un enregistrement : en cours ou dans 10 min.
pub fn current(list: &[CalEvent], now: DateTime<Utc>) -> Vec<CalEvent> {
    let t = now.timestamp();
    list.iter().filter(|e| e.start - LEAD_MIN * 60 <= t && t < e.end).cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FEED: &str = "BEGIN:VCALENDAR\r\nPRODID:-//Google Inc//Google Calendar 70.9054//EN\r\nVERSION:2.0\r\n\
BEGIN:VTIMEZONE\r\nTZID:Europe/Paris\r\nEND:VTIMEZONE\r\n\
BEGIN:VEVENT\r\nDTSTART;TZID=Europe/Paris:20261103T140000\r\nDTEND;TZID=Europe/Paris:20261103T143000\r\n\
DTSTAMP:20261101T120000Z\r\nORGANIZER;CN=alex.martin@example.org:mailto:alex.martin@example.org\r\n\
UID:abc123@google.com\r\nATTENDEE;CUTYPE=INDIVIDUAL;ROLE=REQ-PARTICIPANT;PARTSTAT=ACCEPTED;CN=Claire\r\n  Martin;X-NUM-GUESTS=0:mailto:claire@example.org\r\n\
ATTENDEE;CUTYPE=INDIVIDUAL;ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION;CN=jdupont@exam\r\n ple.org;X-NUM-GUESTS=0:mailto:jdupont@example.org\r\n\
ATTENDEE;CUTYPE=INDIVIDUAL;PARTSTAT=ACCEPTED;CN=alex.martin@example.org:mailto:alex.martin@example.org\r\n\
ATTENDEE;CUTYPE=RESOURCE;CN=Salle 3:mailto:c_188@resource.calendar.google.com\r\n\
X-GOOGLE-CONFERENCE:https://meet.google.com/abc-defg-hij\r\n\
DESCRIPTION:Ordre du jour\\, points :\\nbudget\r\nSUMMARY:Point projet\\, budget\r\n\
BEGIN:VALARM\r\nACTION:DISPLAY\r\nDESCRIPTION:Rappel\r\nEND:VALARM\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nDTSTART;TZID=Europe/Paris:20261006T100000\r\nDTEND;TZID=Europe/Paris:20261006T110000\r\n\
RRULE:FREQ=WEEKLY;BYDAY=TU\r\nEXDATE;TZID=Europe/Paris:20261027T100000\r\nUID:hebdo@google.com\r\n\
SUMMARY:Hebdo équipe\r\nLOCATION:https://zoom.us/j/123456789?pwd=x\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nDTSTART;TZID=Europe/Paris:20261103T113000\r\nDTEND;TZID=Europe/Paris:20261103T120000\r\n\
RECURRENCE-ID;TZID=Europe/Paris:20261103T100000\r\nUID:hebdo@google.com\r\nSUMMARY:Hebdo équipe (décalé)\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nDTSTART;VALUE=DATE:20261103\r\nDTEND;VALUE=DATE:20261104\r\nUID:jour@google.com\r\nSUMMARY:Anniversaire\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nDTSTART:20261103T150000Z\r\nDTEND:20261103T160000Z\r\nUID:annule@google.com\r\nSTATUS:CANCELLED\r\nSUMMARY:Annulé\r\nEND:VEVENT\r\n\
END:VCALENDAR\r\n";

    fn at(s: &str) -> DateTime<Utc> {
        chrono_tz::Europe::Paris.from_local_datetime(&NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap()).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn lecture_google_agenda() {
        let ev = parse(FEED);
        assert_eq!(ev.len(), 5, "VALARM ignoré, 5 VEVENT");
        let me = account_of("https://calendar.google.com/calendar/ical/alex.martin%40example.org/private-0123abcd/basic.ics");
        assert_eq!(me.as_deref(), Some("alex.martin@example.org"));
        let mut people = BTreeMap::new();
        let day = events_between(&ev, at("2026-11-03 00:00"), at("2026-11-04 00:00"), me.as_deref(), &people);
        let titles: Vec<&str> = day.iter().map(|e| e.title.as_str()).collect();
        // L'occurrence de 10 h du mardi est remplacée par l'exception de 11 h 30 ;
        // la journée entière et l'annulé sont ignorés.
        assert_eq!(titles, ["Hebdo équipe (décalé)", "Point projet, budget"]);
        let p = &day[1];
        assert_eq!(p.meet.as_deref(), Some("https://meet.google.com/abc-defg-hij"));
        let names: Vec<(&str, bool, bool)> = p.people.iter().map(|x| (x.name.as_str(), x.guessed, x.me)).collect();
        assert_eq!(names, [("Alex Martin", true, true), ("Claire Martin", false, false), ("Jdupont", true, false)]);
        // L'annuaire corrige un nom, une fois pour toutes.
        people.insert("jdupont@example.org".into(), "Jean".into());
        let day = events_between(&ev, at("2026-11-03 00:00"), at("2026-11-04 00:00"), me.as_deref(), &people);
        assert!(day[1].people.iter().any(|x| x.name == "Jean" && !x.guessed));
        // « Maintenant » : 10 min avant jusqu'à la fin.
        assert_eq!(current(&day, at("2026-11-03 13:51")).len(), 1);
        assert_eq!(current(&day, at("2026-11-03 13:45")).len(), 0);
        assert_eq!(current(&day, at("2026-11-03 14:29")).len(), 1);
        assert_eq!(current(&day, at("2026-11-03 14:30")).len(), 0);
    }

    #[test]
    fn repetitions_exceptions_et_heure_d_hiver() {
        let ev = parse(FEED);
        let none = BTreeMap::new();
        let week = |d: &str| events_between(&ev, at(&format!("{d} 00:00")), at(&format!("{d} 23:59")), None, &none);
        // 13 octobre (heure d'été) et 10 novembre (heure d'hiver) : toujours 10 h à Paris.
        for d in ["2026-10-13", "2026-11-10"] {
            let w = week(d);
            assert_eq!(w.len(), 1, "{d}");
            assert_eq!(w[0].start, at(&format!("{d} 10:00")).timestamp(), "{d}");
            assert_eq!(w[0].meet.as_deref(), Some("https://zoom.us/j/123456789?pwd=x"));
        }
        assert!(week("2026-10-27").is_empty(), "EXDATE respecté");
    }

    #[test]
    fn liens_et_adresses() {
        assert_eq!(meeting_link("Rejoindre : https://meet.google.com/abc-defg-hij."), Some("https://meet.google.com/abc-defg-hij".into()));
        assert_eq!(meeting_link("<https://teams.microsoft.com/l/meetup-join/19%3ameeting>"), Some("https://teams.microsoft.com/l/meetup-join/19%3ameeting".into()));
        assert!(meeting_link("https://evil.example/meet.google.com/x").is_none());
        assert!(meeting_link("https://zoom.us.evil.example/j/1").is_none());
        assert!(is_meeting_url("https://acme.zoom.us/j/42"));
        assert!(!is_meeting_url("https://meet.google.com/"));
        assert!(validate_url("webcal://calendar.google.com/calendar/ical/x/basic.ics", false).unwrap().starts_with("https://"));
        assert!(validate_url("http://calendar.google.com/x.ics", false).is_err());
        assert!(validate_url("http://127.0.0.1:9/x.ics", true).is_ok());
        assert_eq!(guess_name("jean-pierre.dupont42@x.fr"), "Jean Pierre Dupont42");
        assert_eq!(parse_duration("PT1H30M"), Some(CDuration::minutes(90)));
    }
}
