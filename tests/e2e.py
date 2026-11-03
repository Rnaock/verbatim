"""
Banc d'essai Verbatim 4 : l'app Tauri réelle (build Linux, sources audio
simulées en temps réel) sous Xvfb, pilotée par WebDriver, branchée sur une
fausse API ElevenLabs.
"""
import base64
import http.server
import json
import os
import re
import shutil
import socketserver
import struct
import subprocess
import sys
import tempfile
import threading
import time
import wave

from selenium import webdriver
from selenium.webdriver.common.by import By
from selenium.webdriver.common.keys import Keys
from selenium.webdriver.support.ui import WebDriverWait
from selenium.common.exceptions import JavascriptException, StaleElementReferenceException

SC = os.path.dirname(os.path.abspath(__file__))
APP = os.path.join(SC, "v4", "target", "debug", "Verbatim")
TMP = tempfile.mkdtemp(prefix="vb4e2e-")
HOME = os.path.join(TMP, "home")
LIB = os.path.join(HOME, "Documents", "Verbatim")
DATA = os.path.join(HOME, ".local", "share", "Verbatim")
os.makedirs(LIB)
GOOD_KEY = "sk_test_KEY1234567890abcd"
OK = []


def check(name, cond, detail=""):
    OK.append(bool(cond))
    print(("  OK  " if cond else "  ÉCHEC ") + name + (f"  [{detail}]" if detail and not cond else ""), flush=True)


# ------------------------------------------------------------------ fausse API
UP = {"calls": [], "used": 10000, "attempts": {}}


def words(spec):
    out = []
    for i, (sp, st, en, tx) in enumerate(spec):
        if i:
            out.append({"text": " ", "type": "spacing", "start": st, "end": st, "speaker_id": sp})
        out.append({"text": tx, "type": "word", "start": st, "end": en, "speaker_id": sp})
    return out


MIC_WORDS = words([("speaker_0", 0.2, 0.9, "Bonjour,"), ("speaker_0", 1.0, 2.5, "je lance l'enregistrement."),
                   ("speaker_1", 6.2, 7.5, "Moi aussi je suis là,"), ("speaker_1", 7.6, 8.4, "à côté.")])
PC_WORDS = words([("speaker_0", 3.2, 4.0, "Très bien,"), ("speaker_0", 4.1, 5.6, "on vous entend."),
                  ("speaker_0", 9.2, 10.4, "Parfait.")])
FILE_WORDS = words([("speaker_0", 0.1, 1.0, "Note"), ("speaker_0", 1.1, 2.0, "vocale."), ("speaker_1", 2.2, 2.9, "Reçu.")])


def ics_feed():
    # Rendez-vous en cours (commencé il y a 5 min), heure de Paris.
    import datetime as dt
    from zoneinfo import ZoneInfo
    now = dt.datetime.now(ZoneInfo("Europe/Paris"))
    f = lambda t: t.strftime("%Y%m%dT%H%M%S")
    return "\r\n".join([
        "BEGIN:VCALENDAR", "VERSION:2.0", "PRODID:-//Google Inc//Google Calendar 70.9054//EN",
        "BEGIN:VEVENT", "DTSTART;TZID=Europe/Paris:" + f(now - dt.timedelta(minutes=5)), "DTEND;TZID=Europe/Paris:" + f(now + dt.timedelta(minutes=25)),
        "UID:e2e@google.com", "ORGANIZER;CN=moi@example.org:mailto:moi@example.org",
        "ATTENDEE;CUTYPE=INDIVIDUAL;PARTSTAT=ACCEPTED;CN=Claire Martin:mailto:claire@example.org",
        "ATTENDEE;CUTYPE=INDIVIDUAL;PARTSTAT=ACCEPTED;CN=moi@example.org:mailto:moi@example.org",
        "X-GOOGLE-CONFERENCE:https://meet.google.com/abc-defg-hij", "SUMMARY:Point budget avec Claire", "END:VEVENT",
        "END:VCALENDAR", ""])


CAL_URL = "http://127.0.0.1:9914/calendar/ical/moi%40example.org/private-0123456789abcdef/basic.ics"


class Up(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def _j(self, code, obj):
        b = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)

    def do_GET(self):
        if self.path.startswith("/calendar/ical/"):
            b = ics_feed().encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/calendar")
            self.send_header("Content-Length", str(len(b)))
            self.end_headers()
            self.wfile.write(b)
            return
        if self.path == "/v1/user/subscription":
            if self.headers.get("xi-api-key") != GOOD_KEY:
                return self._j(401, {"detail": {"message": "Invalid API key"}})
            return self._j(200, {"character_count": UP["used"], "character_limit": 100000, "tier": "starter",
                                 "next_character_count_reset_unix": 1793000000})
        self._j(404, {})

    def do_POST(self):
        n = int(self.headers.get("Content-Length") or 0)
        body = b""
        while len(body) < n:
            c = self.rfile.read(min(65536, n - len(body)))
            if not c:
                break
            body += c
        form, fname, fbytes = {}, "", b""
        for part in body.split(b"\r\n--"):
            m = re.search(rb'name="([^"]+)"(?:; filename="([^"]*)")?\r\n(?:[Cc]ontent-[Tt]ype: [^\r]+\r\n)?\r\n', part)
            if not m:
                continue
            val = part[m.end():]
            if m.group(2) is not None:
                fname, fbytes = m.group(2).decode(), val
            else:
                form.setdefault(m.group(1).decode(), []).append(val.decode(errors="replace"))
        UP["calls"].append({"key": self.headers.get("xi-api-key"), "form": form, "file": fname, "len": n, "head": fbytes[:4]})
        if self.headers.get("xi-api-key") != GOOD_KEY:
            return self._j(401, {"detail": {"message": "Invalid API key"}})
        att = UP["attempts"][fname] = UP["attempts"].get(fname, 0) + 1
        time.sleep(0.5)
        if fname == "envoi-pc.flac" and att == 1:
            return self._j(429, {"detail": {"message": "Too many concurrent requests"}})
        UP["used"] += 200
        if fname == "envoi-micro.flac":
            return self._j(200, {"language_code": "fra", "words": MIC_WORDS})
        if fname == "envoi-pc.flac":
            return self._j(200, {"language_code": "fra", "words": PC_WORDS})
        return self._j(200, {"language_code": "fra", "words": FILE_WORDS})


class TS(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


up = TS(("127.0.0.1", 9914), Up)
threading.Thread(target=up.serve_forever, daemon=True).start()


# ------------------------------------------------------------------ données de départ
def wav(path, seconds, rate=24000, freq=220.0, amp=0.2, header_ok=True):
    import math
    frames = b"".join(struct.pack("<h", int(amp * 32767 * math.sin(2 * math.pi * freq * i / rate))) for i in range(int(seconds * rate)))
    with open(path, "wb") as f:
        size = 0 if not header_ok else len(frames)
        f.write(b"RIFF" + struct.pack("<I", 36 + size) + b"WAVEfmt " + struct.pack("<IHHIIHH", 16, 1, 1, rate, rate * 2, 2, 16) + b"data" + struct.pack("<I", size))
        f.write(frames)


# Transcription d'une version précédente (fichier .json à plat).
legacy = {"verbatim_version": 3, "source_file": "ancien entretien.m4a", "transcribed_at": "2026-10-12T20:42:00",
          "speakers": {"speaker_0": "Manuel", "speaker_1": "Claire"},
          "elevenlabs": {"text": "x", "words": words([("speaker_0", 0.0, 0.5, "Ancienne"), ("speaker_1", 1.0, 1.5, "transcription.")])}}
open(os.path.join(LIB, "2026-10-12 20h42 ancien entretien.json"), "w").write(json.dumps(legacy))
# Enregistrement interrompu par un plantage : WAV aux en-têtes vides.
crash = os.path.join(LIB, "2026-10-20 10h00 Appel interrompu")
os.makedirs(crash)
wav(os.path.join(crash, "micro.wav"), 6, header_ok=False)
wav(os.path.join(crash, "pc.wav"), 6, freq=330.0, header_ok=False)
json.dump({"v": 4, "id": "x", "title": "Appel interrompu", "created": "2026-10-20T10:00:00", "kind": "recording", "state": "recording",
           "duration": None, "roles": ["mic", "pc"], "playback": None, "source": None, "echo": None, "current": None, "job": None, "error": None},
          open(os.path.join(crash, "item.json"), "w"))
# Fichiers à importer.
SRC = os.path.join(TMP, "src")
os.makedirs(SRC)
wav(os.path.join(SRC, "Mémo téléphone.wav"), 4, rate=44100, freq=180.0)
open(os.path.join(SRC, "note vocale.opus"), "wb").write(os.urandom(6000))
open(os.path.join(SRC, "liste de courses.txt"), "w").write("non")
open(os.path.join(SRC, "Verbatim-4.2.zip"), "wb").write(b"PK\x05\x06" + b"\x00" * 18)

# ------------------------------------------------------------------ lancement
xvfb = subprocess.Popen(["Xvfb", ":79", "-screen", "0", "1400x1000x24"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
env = dict(os.environ, DISPLAY=":79", HOME=HOME, VERBATIM_API_BASE="http://127.0.0.1:9914",
           XDG_DATA_HOME=os.path.join(HOME, ".local", "share"), XDG_CONFIG_HOME=os.path.join(HOME, ".config"),
           NO_AT_BRIDGE="1", WEBKIT_DISABLE_COMPOSITING_MODE="1")
drv = subprocess.Popen([os.path.expanduser("~/.cargo/bin/tauri-driver"), "--port", "4466"], env=env,
                       stdout=open(os.path.join(SC, "driver4.log"), "w"), stderr=subprocess.STDOUT)
time.sleep(2)
from selenium.webdriver.common.options import ArgOptions
opts = ArgOptions()
opts.set_capability("browserName", "wry")
opts.set_capability("tauri:options", {"application": APP})
d = webdriver.Remote("http://127.0.0.1:4466", options=opts)
d.set_window_size(1360, 960)
W = lambda t=30: WebDriverWait(d, t, ignored_exceptions=(JavascriptException, StaleElementReferenceException, IndexError, StopIteration, KeyError, TypeError))
js = d.execute_script
aw = d.execute_async_script
q = lambda css: d.find_elements(By.CSS_SELECTOR, css)
text = lambda css: (q(css)[0].text if q(css) else "")
state = lambda: js("return JSON.parse(JSON.stringify(window.Verbatim.state(), (k, v) => v instanceof Set ? [...v] : v))")
inv = lambda cmd, args="{}": aw(f"const done=arguments[arguments.length-1]; window.__TAURI__.core.invoke('{cmd}', {args}).then(r=>done({{ok:r}}), e=>done({{err:e}}));")


def item_by_title(t):
    return next((x for x in state()["items"].values() if x["title"] == t), None)


def shot(name):
    d.save_screenshot(os.path.join(SC, name))


try:
    print("=== Démarrage, reprise, migration ===")
    W().until(lambda _: state()["ready"])
    try:
        W().until(lambda _: item_by_title("ancien entretien") and (item_by_title("Appel interrompu") or {}).get("state") == "ready")
    except Exception:
        print("ÉTAT :", json.dumps({k: (v["title"], v["state"], v.get("error")) for k, v in state()["items"].items()}, ensure_ascii=False))
        print("DOSSIER :", os.listdir(LIB))
        try:
            print("ITEM.JSON :", open(os.path.join(crash, "item.json")).read()[:600])
            jp = os.path.join(HOME, ".local", "share", "Verbatim", "journal.txt")
            print("JOURNAL :", open(jp).read()[-1500:] if os.path.exists(jp) else "absent")
        except Exception as e:
            print("diag", e)
        for f in os.listdir(LIB):
            if os.path.isdir(os.path.join(LIB, f)):
                print(" ", f, os.listdir(os.path.join(LIB, f)))
        print("RUST :", open(os.path.join(SC, "driver4.log")).read()[-1500:])
        raise
    check("transcription v3 migrée dans la bibliothèque", item_by_title("ancien entretien")["speakers"] == ["Manuel", "Claire"])
    rec = item_by_title("Appel interrompu")
    check("enregistrement interrompu récupéré et finalisé", rec["recovered"] and abs(rec["duration"] - 6.0) < 0.05, rec)
    check("WAV convertis en FLAC", os.path.exists(os.path.join(crash, "micro.flac")) and not os.path.exists(os.path.join(crash, "micro.wav")))
    time.sleep(1)
    if "Enregistre ou importe" not in text(".hero"):
        print("HTML :", js("return document.getElementById('app').innerHTML.slice(0, 1500)"))
        print("ERREURS :", js("return window.__errs || []"))
        shot("v4_debug.png")
    check("accueil affiché", "Enregistre ou importe" in text(".hero"))
    shot("v4_welcome.png")

    print("=== Sécurité de la page ===")
    r = aw("const done=arguments[arguments.length-1]; fetch('http://127.0.0.1:9914/v1/user/subscription').then(()=>done('joint'),()=>done('bloqué'));")
    check("la page ne joint pas le réseau", r == "bloqué", r)
    r = inv("plugin:dialog|open", "{options:{}}")
    check("plugin de fichiers fermé à la page", "err" in r, r)
    r = inv("item_get", "{id:'../../etc'}")
    check("identifiant hors bibliothèque refusé", "err" in r, r)
    r = inv("open_url", "{url:'https://evil.example/'}")
    check("liens limités à elevenlabs.io", "err" in r, r)

    print("=== Clé ===")
    q(".card input[type=password]")[0].send_keys(GOOD_KEY)
    next(b for b in q(".card button") if b.text == "Enregistrer").click()
    W().until(lambda _: state()["key"]["has_key"])
    check("clé enregistrée, masquée", state()["key"]["masked"] == "sk_…abcd")
    check("clé jamais renvoyée à la page", GOOD_KEY not in json.dumps(state()) and GOOD_KEY not in d.page_source)
    W().until(lambda _: state()["balance"] is not None)
    check("solde lu", state()["balance"]["left"] == 90000)

    print("=== Interface : réglages, explications, version ===")
    gear = js("const p=document.querySelector('.top button[aria-label=Réglages] svg path'); return p && p.getAttribute('d')")
    check("icône réglages en forme d'engrenage", gear and gear.count("L") > 20, gear)
    q(".top button[aria-label=Réglages]")[0].click()
    W().until(lambda _: q(".drawer"))
    vtxt = js("return document.querySelector('.drawer').textContent")
    check("section Version avec installation depuis un fichier", "Verbatim 4.1.0" in vtxt and "Installer depuis un fichier" in vtxt, vtxt[-300:])
    check("explications au survol dans les réglages", len(q(".drawer .tip[data-tip]")) >= 2)
    check("vocabulaire : +20 % sur toute la transcription", "20 %" in js("return [...document.querySelectorAll('.drawer .tip')].map(t => t.dataset.tip).join(' ')"))
    r = inv("update_install_file", "{path:'/tmp/photo.png'}")
    check("mise à jour : un fichier quelconque est refusé", "err" in r and "Verbatim" in json.dumps(r, ensure_ascii=False), r)
    # Agenda : adresse secrète.
    cal_in = next(i for i in q(".drawer input[type=password]") if "ical" in (i.get_attribute("placeholder") or ""))
    cal_in.send_keys(CAL_URL)
    next(b for b in q(".drawer button") if b.text == "Connecter").click()
    W().until(lambda _: (state().get("calState") or {}).get("connected"))
    check("agenda connecté, compte reconnu", state()["calState"]["account"] == "moi@example.org", state()["calState"])
    check("adresse secrète jamais renvoyée à la page", "private-0123456789abcdef" not in d.page_source and "private-0123456789abcdef" not in json.dumps(state()))
    check("adresse chiffrée sur le disque", os.path.exists(os.path.join(HOME, ".local", "share", "Verbatim", "agenda.dpapi")))
    next(b for b in q(".drawer button") if b.text == "Fermer").click()
    W().until(lambda _: q(".event"))
    ev_txt = text(".event")
    check("rendez-vous en cours affiché à l'accueil", "Point budget avec Claire" in ev_txt and "Claire Martin" in ev_txt and "en cours" in ev_txt.lower(), ev_txt)
    check("bouton « Rejoindre et enregistrer »", any("Rejoindre et enregistrer" in b.text for b in q(".event button")))
    r = inv("open_url", "{url:'https://meet.google.com/abc-defg-hij'}")
    r2 = inv("open_url", "{url:'https://meet.google.com.evil.example/x'}")
    check("liens de visio autorisés, imitations refusées", "err" in r2 and not ("err" in r and "refusée" in json.dumps(r, ensure_ascii=False)), (r, r2))
    shot("v41_agenda.png")

    print("=== Enregistrement deux sources ===")
    inv("settings_set", "{patch:{mode:'separate'}}")
    q(".bigrec")[0].click()
    W().until(lambda _: q(".recbar") and len(q(".recbar .meter")) == 2)
    check("barre d'enregistrement avec deux vumètres", True)
    time.sleep(4)
    devs = js("return [...document.querySelectorAll('.recbar .meter .dev')].map(e => e.textContent)")
    check("noms des sources affichés", any("Micro simulé" in x for x in devs) and any("son du PC" in x for x in devs), devs)
    q(".recbar .rectitle")[0].send_keys("Appel avec Claire")
    pause = next(b for b in q(".recbar button") if b.text == "Pause")
    pause.click()
    time.sleep(2)
    check("pause affichée", "paused" in q(".recbar")[0].get_attribute("class"))
    next(b for b in q(".recbar button") if b.text == "Reprendre").click()
    time.sleep(7)
    shot("v4_recording.png")
    next(b for b in q(".recbar button") if "Arrêter" in b.text).click()
    W().until(lambda _: not q(".recbar"))
    W(60).until(lambda _: (item_by_title("Appel avec Claire") or {}).get("has_transcript"))
    it = item_by_title("Appel avec Claire")
    folder = os.path.join(LIB, it["id"])
    check("durée hors pause ≈ 11 s", 10.0 < it["duration"] < 12.5, it["duration"])
    files = os.listdir(folder)
    check("pistes et écoute en FLAC", all(f in files for f in ["micro.flac", "pc.flac", "ecoute.flac", "envelope.json", "transcription.txt"]), files)
    calls = [c for c in UP["calls"] if c["file"].startswith("envoi-")]
    names = sorted(set(c["file"] for c in calls))
    check("pistes séparées envoyées (micro et PC)", names == ["envoi-micro.flac", "envoi-pc.flac"], names)
    check("diarisation demandée sur chaque piste", all(c["form"].get("diarize") == ["true"] for c in calls))
    check("fichiers FLAC envoyés", all(c["head"] == b"fLaC" for c in calls))
    check("relance après 429 sur la piste PC", UP["attempts"].get("envoi-pc.flac") == 2, UP["attempts"])
    tdirs = os.listdir(os.path.join(folder, "transcriptions"))
    tfiles = os.listdir(os.path.join(folder, "transcriptions", tdirs[0]))
    check("réponses brutes conservées", "brut-micro.json" in tfiles and "brut-pc.json" in tfiles, tfiles)
    js("window.Verbatim.select(arguments[0])", it["id"])
    W().until(lambda _: len(q(".spk")) == 3)
    chips = [e.text for e in q(".spk")]
    check("deux personnes au micro + une côté PC, nommée d'après le rendez-vous", any("Micro 1" in c for c in chips) and any("Micro 2" in c for c in chips) and any("Claire Martin" in c for c in chips), chips)
    whos = [e.text.lower() for e in q(".turn .who")]
    check("tours dans l'ordre du temps", whos[:3] == ["micro 1", "claire martin", "micro 2"], whos)
    check("enregistrement lié au rendez-vous en cours", it.get("event") == "Point budget avec Claire", it.get("event"))
    me_chip = [e.text for e in q(".evrow .pchip")]
    check("ta propre adresse affichée avec ton prénom", any("(toi)" in c and "Moi" in c for c in me_chip), me_chip)
    plan = inv("estimate", "{id:'%s', mode:'auto'}" % it["id"])
    check("mode automatique : une seule piste mixée (coût ×1)", plan.get("ok", {}).get("mode") == "mixed" and plan["ok"]["parts"] == [None], plan)
    # Nom d'un participant corrigé : annuaire + voix renommée.
    pc = next(e for e in q(".evrow .pchip") if "Claire Martin" in e.text)
    pc.click()
    pin = q(".evrow .pchip-inp")[0]
    pin.send_keys(Keys.CONTROL, "a")
    pin.send_keys("Claire", Keys.ENTER)
    W().until(lambda _: any(e.text.startswith("Claire") and "Martin" not in e.text for e in q(".spk")))
    check("participant renommé : voix et annuaire mis à jour", (state()["settings"].get("people") or {}).get("claire@example.org") == "Claire", state()["settings"].get("people"))
    # Saisie du titre pendant que la conversation est mise à jour (ancien bug).
    tf = q(".head .title")[0]
    tf.click()
    tf.send_keys(Keys.END, " brouillon")
    inv("transcribe_cancel", "{id:'%s'}" % it["id"])
    time.sleep(1)
    tf.send_keys(" final", Keys.ENTER)
    W().until(lambda _: (state()["items"].get(it["id"]) or {}).get("title", "").endswith("brouillon final"))
    check("titre modifiable sans être écrasé par les mises à jour", True)
    inv("item_rename", "{id:'%s', title:'Appel avec Claire'}" % it["id"])
    time.sleep(0.5)
    frise = js("const c=document.querySelector('.frise canvas'); const g=c.getContext('2d').getImageData(0,0,c.width,c.height).data; let n=0; for(let i=3;i<g.length;i+=4) if(g[i]>0) n++; return n;")
    check("frise dessinée", frise > 2000, frise)
    src = js("const a=document.querySelector('audio'); return a && a.getAttribute('src');")
    check("lecteur branché sur l'écoute", src and "ecoute.flac" in src, src)
    shot("v4_detail.png")

    print("=== Correction des locuteurs ===")
    q(".spk")[0].click()
    inp = q(".pop input[type=text]")[0]
    inp.clear()
    inp.send_keys("Manuel", Keys.ENTER)
    W().until(lambda _: any("Manuel" in e.text for e in q(".spk")))
    # Micro 2 est en fait Claire : on renomme.
    next(e for e in q(".spk") if "Micro 2" in e.text).click()
    inp = q(".pop input[type=text]")[0]
    inp.clear()
    inp.send_keys("Claire", Keys.ENTER)
    W().until(lambda _: any("Claire" in e.text for e in q(".spk")))
    # La voix côté PC est en fait la même personne que Micro 2 renommée (test de fusion).
    next(e for e in q(".spk") if e.text.startswith("Claire") and e.find_elements(By.CSS_SELECTOR, ".srcicon") and "PC" in (e.find_elements(By.CSS_SELECTOR, ".srcicon")[0].get_attribute("title") or "")).click()
    next(b for b in q(".pop .item") if "Fusionner avec Claire" in b.text).click()
    W().until(lambda _: len(q(".spk")) == 2)
    check("fusion de deux locuteurs", True)
    tj = json.load(open(os.path.join(folder, "transcriptions", tdirs[0], "transcript.json")))
    check("corrections enregistrées sur le disque", sorted(s["name"] for s in tj["speakers"]) == ["Claire", "Manuel"], tj["speakers"])
    txt = open(os.path.join(folder, "transcription.txt"), encoding="utf-8-sig").read()
    check("texte à jour avec les noms", "] Manuel\n" in txt and "] Claire\n" in txt, txt[:300])
    next(b for b in q("button") if b.text == "Annuler la correction").click()
    W().until(lambda _: len(q(".spk")) == 3)
    check("annulation de la fusion", True)
    # Réattribution d'un paragraphe à une nouvelle personne.
    js("document.querySelectorAll('.para .reassign')[0].click()")
    next(b for b in q(".pop .item") if "nouvelle personne" in b.text).click()
    W().until(lambda _: any("Personne" in e.text for e in q(".spk")))
    chips = [e.text for e in q(".spk")]
    check("réattribution à une nouvelle personne (l'ancien nom sans passage disparaît)", len(chips) == 3 and not any("Manuel" in c for c in chips), chips)
    next(b for b in q("button") if b.text == "Réinitialiser").click()
    W().until(lambda _: any("Micro 1" in e.text for e in q(".spk")))
    check("réinitialisation depuis les réponses d'origine", len(q(".spk")) == 3)
    # Mise à jour glissée dans la fenêtre : proposition, puis annulation.
    js("window.Verbatim.importPaths(arguments[0])", [os.path.join(SRC, "Verbatim-4.2.zip")])
    W().until(lambda _: q(".updbar"))
    check("zip de Verbatim glissé : installation proposée", "Verbatim-4.2.zip" in text(".updbar"), text(".updbar"))
    next(b for b in q(".updbar button") if b.text == "Annuler").click()
    W().until(lambda _: not q(".updbar"))

    print("=== Import ===")
    js("window.Verbatim.importPaths(arguments[0])", [os.path.join(SRC, "Mémo téléphone.wav"), os.path.join(SRC, "note vocale.opus"), os.path.join(SRC, "liste de courses.txt")])
    W(60).until(lambda _: (item_by_title("Mémo téléphone") or {}).get("has_transcript") and (item_by_title("note vocale") or {}).get("has_transcript"))
    check("fichier texte refusé à l'import", item_by_title("liste de courses") is None)
    memo = item_by_title("Mémo téléphone")
    mf = os.listdir(os.path.join(LIB, memo["id"]))
    check("original copié dans la bibliothèque", "Mémo téléphone.wav" in mf, mf)
    check("durée lue", abs(memo["duration"] - 4.0) < 0.05, memo["duration"])
    sent = {c["file"]: c for c in UP["calls"]}
    check("WAV converti en FLAC 16 kHz pour l'envoi", sent.get("envoi-mix.flac", {}).get("head") == b"fLaC", list(sent))
    check("format illisible envoyé tel quel", "note vocale.opus" in sent, list(sent))
    check("import transcrit avec locuteurs", item_by_title("note vocale")["speakers"] == ["Locuteur 1", "Locuteur 2"])

    print("=== Recherche, retranscription, corbeille ===")
    box = q(".search input")[0]
    box.send_keys("reçu")
    W().until(lambda _: len(q(".entry")) == 2)
    found = sorted(js("return [...document.querySelectorAll('.entry .t')].map(e => e.textContent)"))
    check("recherche plein texte, insensible aux accents", found == ["Mémo téléphone", "note vocale"], found)
    box.clear()
    box.send_keys(" ")
    js("const i=document.querySelector('.search input'); i.value=''; i.dispatchEvent(new Event('input',{bubbles:true}))")
    W().until(lambda _: len(q(".entry")) == 5)
    js("window.Verbatim.select(arguments[0])", memo["id"])
    W().until(lambda _: any(b.text == "Retranscrire…" for b in q("button")))
    next(b for b in q("button") if b.text == "Retranscrire…").click()
    W().until(lambda _: any(b.text == "Retranscrire" for b in q("button")))
    cost = text(".cost")
    check("coût estimé affiché", "€" in cost and "crédits" in cost, cost)
    from selenium.webdriver.common.action_chains import ActionChains
    tips = q(".card .tip")
    if tips:
        ActionChains(d).move_to_element(tips[0]).perform()
        time.sleep(0.5)
    shot("v41_transcribe.png")
    n0 = len(UP["calls"])
    next(b for b in q("button") if b.text == "Retranscrire").click()
    W(40).until(lambda _: len(UP["calls"]) > n0 and not (item_by_title("Mémo téléphone") or {}).get("job"))
    time.sleep(1)
    check("deuxième version conservée", len(os.listdir(os.path.join(LIB, memo["id"], "transcriptions"))) == 2)
    next(b for b in q(".sub button") if b.text == "Supprimer").click()
    next(b for b in q(".sub button") if b.text == "Mettre à la corbeille").click()
    W().until(lambda _: item_by_title("Mémo téléphone") is None)
    check("conversation déplacée dans la corbeille", os.path.isdir(os.path.join(LIB, ".corbeille", memo["id"])))
    check("aucune clé dans le DOM", GOOD_KEY not in d.page_source)
    shot("v4_final.png")
except Exception:
    print("ERREURS JS :", js("return window.__errs"))
    print("TOAST :", text(".toast"))
    shot("v4_fail.png")
    raise
finally:
    try:
        logs = d.get_log("browser") if hasattr(d, "get_log") else []
    except Exception:
        logs = []
    try:
        d.quit()
    except Exception:
        pass
    drv.terminate()
    xvfb.terminate()

print(f"\n{sum(OK)}/{len(OK)} vérifications passées")
shutil.rmtree(TMP, ignore_errors=True)
sys.exit(0 if all(OK) else 1)
