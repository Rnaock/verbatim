# À faire (validé par Manuel)

Liste des évolutions décidées, à reprendre à la prochaine mise à jour.

## 1. Arrêt quasi instantané et fichiers plus légers (validé le 13/11/2026)

Constat sur de vrais appels : environ 150 Mo par heure, dont plus de la moitié
pour `ecoute.flac`. À l'arrêt, deux attentes : « Finalisation de
l'enregistrement » (relecture de tout l'audio, FLAC 24 kHz, analyse, mélange)
puis « Préparation de l'audio » (FLAC 16 kHz pour ElevenLabs).

- Encoder `micro.flac` et `pc.flac` **pendant l'enregistrement** (FLAC
  incrémental déjà disponible dans `audio::FlacWriter`), en gardant les WAV
  de secours jusqu'à la fin, pour ne rien perdre en cas de plantage.
- Calculer **pendant l'enregistrement** les enveloppes (frise) et les
  statistiques d'activité (bruit, niveau, part de parole).
- Produire `ecoute.flac` en **16 kHz mono** (qualité suffisante pour
  réécouter) au fil de l'eau. Les gains de mélange ne sont connus qu'à la fin :
  appliquer une normalisation glissante, ou garder la normalisation finale
  mais en 16 kHz seulement.
- En mode Automatique, **réutiliser ce fichier d'écoute comme envoi**
  (`envoi-mix.flac`) : plus de préparation.
- Objectif : arrêt en moins de 2 s pour un appel d'une heure, environ
  105 Mo par heure au lieu de 150.
- Ne pas casser : récupération après plantage (`repair_wav`), conversations
  existantes en 24 kHz (lecture inchangée), retranscription en pistes
  séparées (repart de `micro.flac` et `pc.flac`).

## 2. Paragraphes trop hachés en pistes séparées (à vérifier)

Appel du 11/11 en pistes séparées : 896 paragraphes pour 63 min, contre 198
pour 47 min en mode Automatique. Hypothèse : `segment()` coupe un paragraphe
dès que l'autre piste dit « oui » ou « hm » pendant un blanc. Piste : ignorer
les interventions de 1 à 2 mots de moins de 1,5 s comme point de coupure, ou
les rattacher en incise. Vérifier sur la structure de cette transcription
(longueurs des paragraphes), sans en lire le contenu.

## 3. Plus tard

- Signer les versions avec une clé gardée sur le PC de Manuel (une fois
  Claude Code installé dessus), et faire vérifier la signature par Verbatim.
- Intégration Notion : voir `INTEGRATION-AGENDA-NOTION.md`, en attente des
  réponses de l'assistant agenda/Notion de Manuel.
