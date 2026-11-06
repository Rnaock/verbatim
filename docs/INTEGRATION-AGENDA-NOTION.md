# Verbatim × Google Agenda × Notion : note d’intégration

Note destinée à l’assistant qui gère l’agenda Google et l’espace Notion de
Manuel. But : décider ensemble comment chaque transcription de Verbatim
rejoint la bonne page Notion, sans doublon et sans casser l’organisation
existante. Rien n’est encore branché côté Notion : tout est à décider.

## 1. Ce qu’est Verbatim

Application Windows, locale, qui enregistre une conversation (micro et son du
PC) ou importe un fichier, puis la transcrit via ElevenLabs avec séparation
des voix. Elle lit l’agenda Google en **lecture seule** via l’adresse iCal
secrète. Elle n’écrit nulle part ailleurs que sur le disque de Manuel.

Pour chaque conversation, un dossier dans `Documents\Verbatim` :

```
2026-11-06 14h00 Point budget\
  item.json            métadonnées (voir ci-dessous)
  transcription.txt    texte lisible de la version courante
  transcriptions\<horodatage>\transcript.json   locuteurs + paragraphes horodatés
  micro.flac, pc.flac, ecoute.flac               audio
```

Extrait de `item.json` quand un rendez-vous est lié :

```json
{
  "id": "2026-11-06 14h00 Point budget",
  "created": "2026-11-06T14:00:12+01:00",
  "duration": 2160.4,
  "event": {
    "uid": "abc123@google.com@1793955600",
    "title": "Point budget",
    "start": 1793955600, "end": 1793957400,
    "people": [
      { "email": "manuel@…", "name": "Manuel", "me": true },
      { "email": "jean@…", "name": "Jean", "guessed": false, "me": false }
    ],
    "meet": "https://meet.google.com/abc-defg-hij"
  }
}
```

`transcript.json` : `speakers` (id, nom, source micro/PC) et `segments` (début,
fin en secondes, locuteur, texte). Les noms de voix sont corrigés à la main
par Manuel dans Verbatim, et retenus dans un annuaire e-mail → prénom.

## 2. Identifier un rendez-vous : les pièges

- **Deux identifiants Google différents.** Le flux iCal donne l’`UID` iCal
  (ex. `abc123@google.com`), que l’API Google Calendar appelle `iCalUID`.
  L’`id` de l’API (`eventId`) est **un autre identifiant**. Une intégration
  Notion ↔ Google stocke souvent l’`eventId`, ou seulement un lien. Il faut
  savoir lequel est conservé côté Notion.
- **Rendez-vous récurrents.** Toutes les occurrences partagent le même `UID`.
  Verbatim distingue l’occurrence par son heure de début :
  `uid = "<UID iCal>@<début en secondes UNIX>"`. Dans l’API Google, une
  occurrence a l’`eventId` `<idDeLaSérie>_<AAAAMMJJTHHMMSSZ>`.
- **Occurrence déplacée.** Google garde l’`UID` et ajoute un `RECURRENCE-ID`
  (heure d’origine). Verbatim utilise aujourd’hui l’heure **réelle** de début.
  Si Notion se cale sur l’heure d’origine, il faudra s’aligner (facile à
  changer côté Verbatim).
- **Fuseaux.** Verbatim stocke des instants UTC (secondes UNIX). Les
  comparaisons doivent se faire en UTC, pas sur les heures affichées.
- **Pas de rendez-vous.** Appel improvisé ou fichier importé : `event` est
  absent. Il faut décider s’il faut quand même créer une page.
- **Plusieurs rendez-vous en même temps.** Verbatim ne lie rien
  automatiquement, et Manuel choisit à la main. L’identifiant peut aussi être
  ajouté ou changé après coup.

## 3. Rapprochement proposé (du plus sûr au plus souple)

1. **Exact** : la page Notion porte l’identifiant iCal **et** l’heure de début
   de l’occurrence. On cherche la page qui a les deux.
2. **Souple** : même jour, début à ±15 min, titre proche (ou au moins un
   participant en commun). À utiliser seulement si (1) est impossible, et à
   signaler.
3. **Création** : aucune page trouvée → Verbatim crée une page avec titre,
   date, participants et identifiants, pour qu’un rapprochement ultérieur
   reste exact.

Le plus robuste est que **la synchronisation agenda → Notion stocke elle-même
l’identifiant iCal et l’heure de début** sur chaque page de réunion. Verbatim
n’aurait alors jamais à deviner.

## 4. Partage des rôles proposé

| Champ de la page Notion | Écrit par |
|---|---|
| Titre, date, participants, lien de visio | la synchro agenda → Notion (source de vérité) |
| Identifiant iCal + début de l’occurrence | la synchro agenda (idéalement), sinon Verbatim à la création |
| Transcription (contenu de la page ou sous-page « Transcription ») | Verbatim, en ajout seulement |
| Propriétés « Transcrit le », « Durée », « Verbatim ID » | Verbatim |

Verbatim ne modifierait **jamais** le titre, la date, les participants ni le
reste du contenu de la page. Il n’ajoute qu’un bloc identifiable. S’il
retranscrit, il remplace uniquement ce bloc.

## 5. Côté technique, si Verbatim écrit dans Notion

- Intégration Notion « interne » (jeton secret), partagée **avec la seule base
  de réunions**. Le jeton serait chiffré sur le PC comme la clé ElevenLabs.
- API utilisée : interroger la base avec un filtre (identifiant, date), créer
  une page, ajouter des blocs. Limites : 100 blocs par requête, 2 000
  caractères par bloc de texte. Une transcription d’une heure demande donc
  plusieurs requêtes.
- Déclenchement : automatique à la fin de la transcription, ou bouton
  « Envoyer vers Notion ». Les transcriptions sont sensibles : Manuel
  préférera peut-être valider avant envoi.

## 6. Questions à trancher

1. Existe-t-il déjà une base Notion des réunions alimentée depuis Google
   Agenda ? Par quel moyen (Notion Calendar, automatisation, assistant), et
   quels identifiants stocke-t-elle (`eventId`, `iCalUID`, lien, rien) ?
2. Peut-on ajouter aux pages les propriétés « iCalUID » et « Début (UTC) » ?
3. Une page par occurrence de réunion récurrente, ou une page par série ?
4. La transcription va-t-elle dans le corps de la page, dans une sous-page,
   ou dans une base « Transcriptions » liée ?
5. Faut-il un résumé en tête (décisions, actions), et qui le produit ?
6. Envoi automatique ou validé par Manuel ?
7. Que faire des appels sans rendez-vous et des fichiers importés ?

Dès que ces points sont fixés, Verbatim peut s’aligner sur le format retenu.
