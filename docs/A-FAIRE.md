# À faire (validé par Manuel)

Liste des évolutions décidées, à reprendre à la prochaine mise à jour.

## Fait en 4.4

- Arrêt immédiat : FLAC, enveloppes et piste d'écoute 16 kHz produits pendant
  l'enregistrement (`recorder::Mixer`), WAV gardés comme filet de sécurité ;
  la piste d'écoute sert d'envoi en mode Automatique.
- Paragraphes en pistes séparées : une relance courte de l'autre piste ne
  coupe plus le paragraphe (896 → environ 590 paragraphes sur l'appel de
  référence, mesuré sur les seuls horodatages).
- Accueil du premier lancement (prénom, clé, agenda, langue).
- Issue #1 (enregistrement oublié) : rappel après 2 min de silence, fin
  silencieuse non envoyée à la transcription.
- Faux « écart de -100 % » quand ElevenLabs débite après coup.

## Fait en 4.4.1

- Icône plus lisible : tuile bleue vive, barres blanches, trois barres sous
  32 px (`tests/make-icons.py`).

## Fait en 4.5

- Mise à jour jamais installée sans passer par une fenêtre : nouveautés,
  avertissement « projet amateur », marche arrière, « Ignorer cette version ».
- Version précédente gardée (`Verbatim-precedent.exe`) et bouton « Revenir à
  Verbatim x.y » dans Réglages → Version.
- Arrêt brutal détecté au lancement suivant (témoin `session.txt`) : rapport
  proposé par mail pré-rempli, ou copie.
- Signalement « Par mail » pour qui n'a pas de compte GitHub.

## Fait en 4.5.1

- Agenda : relu toutes les 2 minutes au plus (5 avant), au retour sur la
  fenêtre si la copie a plus de 20 s, et sur « Actualiser ». L'heure de
  lecture est affichée, un échec de lecture garde les rendez-vous connus.
- Enregistrement « fantôme » : un relevé de niveaux arrivé après l'arrêt
  remettait l'interface en mode enregistrement (plus de « Supprimer »).

## À étudier

- Rapports de plantage : les plantages natifs (pilote audio, WebView2) ne
  laissent pas de trace dans le journal, seulement l'arrêt brutal. Un envoi
  automatique demanderait un serveur de réception et le consentement.

- Enregistrement oublié : proposer de couper aussi le fichier conservé (pour
  l'instant seul l'envoi est raccourci), et arrêt automatique optionnel après
  un long silence.
- Deux voix détectées par piste en pistes séparées alors qu'il n'y a qu'une
  personne (vu dans le journal : « 4 voix » puis fusion à la main) : proposer
  la fusion automatique quand une voix parle très peu.
- Gain du mélange d'écoute en direct : il s'ajuste au fil de l'appel (6 dB/s
  au plus) ; vérifier à l'oreille sur de vrais appels.

## Plus tard

- Signer les versions avec une clé gardée sur le PC de Manuel (une fois
  Claude Code installé dessus), et faire vérifier la signature par Verbatim.
- Intégration Notion : voir `INTEGRATION-AGENDA-NOTION.md`, en attente des
  réponses de l'assistant agenda/Notion de Manuel.
