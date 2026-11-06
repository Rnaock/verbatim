# Verbatim 4.3

Enregistre et transcrit n’importe quelle conversation : ton micro, le son du
PC (visio, appel, vidéo), ou les deux à la fois. Importe aussi les fichiers de
ton téléphone ou d’autres logiciels, vidéos comprises. Transcription par
ElevenLabs Scribe v2, avec séparation des personnes.

## Installation

1. **Avant d’extraire le zip** : clic droit sur `Verbatim-4.3.zip` →
   Propriétés → coche **Débloquer** → OK.
2. Extrais le dossier `Verbatim` où tu veux le garder. Garde les deux fichiers
   ensemble : `Verbatim.exe` a besoin de `WebView2Loader.dll` à côté de lui.
3. Clic droit sur `Verbatim.exe` → **Afficher d’autres options** → **Envoyer
   vers** → **Bureau (créer un raccourci)**.

Au premier lancement, Windows peut afficher « Windows a protégé votre
ordinateur » : **Informations complémentaires** puis **Exécuter quand même**.
Ça n’arrive qu’une fois.

**Depuis Verbatim 4.0** : ferme Verbatim, puis remplace l’ancien
`Verbatim.exe` par le nouveau, dans le même dossier (garde aussi
`WebView2Loader.dll` à côté). Ton raccourci continue de marcher.
Conversations, clé et réglages restent en place. À partir de la 4.1, les
versions suivantes s’installent depuis Verbatim lui-même.

**Depuis Verbatim 3** : remplace simplement l’ancien `Verbatim.exe` (ou garde
les deux dossiers, ils partagent les mêmes données). Ta clé et tes réglages
sont repris. Chaque ancienne transcription devient une conversation de la
bibliothèque, avec les noms de locuteurs que tu avais donnés.

## Mises à jour

**Qui peut publier une mise à jour ?** Seuls les comptes qui ont le droit
d’écrire dans le dépôt : le tien, et Claude via l’application GitHub que tu
as autorisée, seulement dans les sessions que tu lances. Un dépôt public se
lit par tous, mais personne d’autre ne peut y écrire. Une proposition de
modification (« pull request ») venue de l’extérieur n’entre dans le dépôt
que si tu l’acceptes. Verbatim ne télécharge que depuis ce dépôt (adresse
inscrite dans le programme) et vérifie l’empreinte du fichier. La vraie
protection est donc ton compte GitHub : active la validation en deux étapes
(GitHub → Settings → Password and authentication).

Au lancement, Verbatim regarde s’il existe une version plus récente sur
github.com/Rnaock/verbatim et l’annonce dans un bandeau : **Mettre à jour**
télécharge, vérifie l’empreinte du fichier, remplace le programme et
redémarre. Jamais pendant un enregistrement ou une transcription.

Seul `Verbatim.exe` change. Tes conversations (`Documents\Verbatim`), ta clé,
ton agenda et tes réglages (`%LOCALAPPDATA%\Verbatim`) ne sont jamais touchés.
L’ancienne version est gardée sous `Verbatim.old.exe` jusqu’au lancement
suivant. Réglages → **Version** permet aussi d’installer depuis un fichier.

## Accueil

Le logo **Verbatim.** en haut à gauche (ou « ‹ Accueil » au-dessus d’une
conversation) ramène à l’accueil : enregistrer, importer, et à droite
**l’agenda du jour**. Les rendez-vous passés sont estompés, un trait rouge
marque l’heure actuelle, et chaque rendez-vous déjà enregistré mène à sa
conversation.

Le bouton **⋯** en haut à droite donne les accès rapides : thème (auto,
clair, sombre), **Signaler un problème**, **Proposer une idée**, journal.

## Agenda Google

Réglages → **Agenda** : colle l’**adresse secrète au format iCal** de ton
agenda (Google Agenda → ⚙ Paramètres → ton agenda dans la colonne de gauche →
« Intégrer l’agenda »). Lecture seule, chiffrée sur ce PC comme ta clé.

Ensuite :

- Quand un rendez-vous est en cours ou commence dans les 10 minutes, l’accueil
  l’affiche avec ses participants : **Rejoindre et enregistrer** ouvre la visio
  (Meet, Zoom, Teams) et lance l’enregistrement d’un seul clic. Ailleurs dans
  l’app, un rappel discret reste en haut.
- L’enregistrement prend le titre du rendez-vous et ses participants. S’il y a
  une seule autre personne et une seule voix côté PC, elle est nommée
  directement. Sinon, les participants sont proposés en un clic quand tu
  renommes une voix.
- Clique sur le nom d’un participant pour le corriger (« Jean » au lieu de
  « jdupont ») : Verbatim s’en souvient pour toutes les conversations
  suivantes. Un nom en pointillés est deviné depuis l’adresse e-mail.
- Une conversation peut être liée après coup : **Lier à un rendez-vous…** sous
  son titre. La recherche trouve aussi les conversations par participant.

Si plusieurs rendez-vous se chevauchent, rien n’est lié automatiquement :
choisis-le sur l’accueil.

## Enregistrer

Le gros bouton rouge **Enregistrer** démarre tout de suite. La petite flèche à
côté choisit les sources :

- **Micro** : ce que tu dis (et ce qui se dit dans la pièce).
- **Son du PC** : tout ce que le PC joue (Teams, Zoom, Meet, Discord, une
  vidéo…), sauf Verbatim lui-même. Pas besoin de Voicemeeter ni de câble
  virtuel, et tu ne t’entends pas dans le casque.

Les deux sont cochées par défaut : c’est le bon réglage pour une visio. Pour
une réunion autour d’une table, garde seulement **Micro**.

Pendant l’enregistrement, la barre rouge montre la durée, un vumètre par
source, le périphérique utilisé et le rendez-vous lié (× pour le délier). Tu
peux faire **Pause**, puis **Arrêter**. Juste après l’arrêt, le nom de la
conversation est sélectionné : tape le nouveau nom, puis Entrée, pendant que
Verbatim finalise l’audio.

Ce qui est garanti :

- **Changement de casque ou de micro** : Verbatim suit le périphérique par
  défaut de Windows. Tu branches, débranches, passes en Bluetooth :
  l’enregistrement continue, avec un court silence pendant la bascule.
- **Rien ne se perd** : l’audio est écrit sur le disque toutes les deux
  secondes. Après une coupure de courant ou un plantage, la conversation est
  récupérée au démarrage suivant.
- **Échec de transcription** : l’audio reste dans la bibliothèque, tu relances
  quand tu veux, avec d’autres réglages si besoin. Une annulation coupe aussi
  les envois en cours, pour ne pas payer pour rien.
- **Micro coupé** : si le micro ne capte strictement rien pendant 6 s, la barre
  l’affiche en orange (« le micro est-il coupé ? »).
- Le PC ne se met pas en veille pendant un enregistrement.

Si tu as une clé API et que **Transcrire automatiquement à l’arrêt** est coché,
la transcription part dès l’arrêt.

## Importer

Glisse des fichiers n’importe où dans la fenêtre, ou **Transcrire un
fichier**. Formats : m4a, mp3, wav, flac, ogg, opus, aac, wma, amr, aiff, et les
vidéos (mp4, mov, mkv, avi, webm, 3gp…), dont seul le son est gardé. L’original est copié
dans la bibliothèque, il n’est jamais modifié.

## Transcrire

Panneau **Transcrire** d’une conversation :

- **Langue**, **Nombre de personnes** si tu le connais (ça aide la
  séparation), **Texte nettoyé** (sans « euh » ni faux départs). Survole le
  petit ⓘ pour l’explication de chaque option.
- **Vocabulaire spécifique** (noms propres, sigles) : mémorisé et appliqué à
  chaque transcription. Il ajoute **20 % au prix de toute la transcription**,
  quel que soit le nombre de termes. Laisse-le vide pour ne pas payer ce
  supplément.
- Pour un enregistrement à deux sources : **Automatique** (coût ×1, conseillé)
  ou **Pistes séparées** (coût ×2).
- Le coût estimé s’affiche avant de lancer, en euros et en crédits
  (ElevenLabs compte environ 330 crédits par minute d’audio).
- À la fin de la transcription, une ligne indique **ce qu’elle a réellement
  coûté** (mesuré sur ton solde avant et après), **ce qu’il reste**, et le
  temps de transcription que ça représente. Un écart de plus de 10 % avec
  l’estimation est signalé. Une recharge pendant la transcription est
  reconnue et n’est pas comptée comme un écart. Si le solde ne suffit plus
  pour une conversation de même durée, un bandeau orange te prévient.
- **Plus assez de crédits** : Verbatim vérifie le solde avant d’envoyer quoi
  que ce soit. S’il manque des crédits, ou si ElevenLabs refuse en cours de
  route, rien n’est perdu : l’audio reste là, un bouton **Recharger sur
  ElevenLabs** apparaît, puis **Relancer**.

**Retranscrire…** garde la version précédente : rien n’est écrasé.

### Comment Verbatim sépare les voix

Une source ne correspond jamais forcément à une seule personne : plusieurs
personnes peuvent parler autour de ton micro, et plusieurs interlocuteurs
arrivent par le son du PC. Verbatim combine donc deux indices :

1. **La source** (micro ou PC) est une frontière sûre quand elle est
   exploitable.
2. **La reconnaissance des voix** d’ElevenLabs subdivise chaque source en
   personnes.

Les deux pistes restent toujours séparées sur ton disque. Ce qui change, c’est
ce qui part chez ElevenLabs :

- **Automatique** : un seul fichier, où micro et PC sont mélangés, donc
  facturé une fois. Au retour, Verbatim regarde pour chaque mot sur quelle
  piste il y avait du son. Avec un casque, le micro n’entend pas
  l’interlocuteur : l’attribution « toi / lui » est quasi certaine, même si
  ElevenLabs confond les voix. Avec des haut-parleurs, la source la plus forte
  l’emporte. Si une source est restée muette, seule l’autre est envoyée.
- **Pistes séparées** : chaque piste est transcrite à part puis réunie dans
  l’ordre du temps. Un peu meilleur quand vous parlez en même temps, pour un
  coût double. Tu peux comparer sur un même appel avec **Retranscrire…**.

## Corriger les locuteurs

- Clique sur le nom d’une personne pour la **renommer**. Les noms déjà
  utilisés sont proposés. Ton prénom (dans les réglages) est donné
  automatiquement à la voix du micro quand elle est seule.
- **C’est la même personne que…** fusionne deux locuteurs.
- Sur un paragraphe : **Ce passage est de…** le réattribue à une personne
  existante ou à **Une nouvelle personne**, seul ou avec **la suite du tour**.
  **Écouter ce passage** pour vérifier à l’oreille.
- **Annuler la correction** revient en arrière, **Réinitialiser** retrouve
  l’attribution d’origine. Le texte lui-même n’est jamais modifié.

## Écouter, retrouver, exporter

- La **frise** montre qui parle quand (micro au-dessus, PC en dessous). Un
  clic dessus, ou sur l’heure d’un tour de parole, déplace la lecture.
- Lecteur : ±10 s et vitesse de lecture.
- La recherche en haut cherche dans les titres et dans le texte de toutes les
  transcriptions, sans tenir compte des accents.
- Export **Texte (.txt)**, **Markdown (.md)**, **Sous-titres (.srt)**, ou
  **Copier**.
- **Supprimer** met la conversation dans `.corbeille` : rien n’est effacé
  définitivement.

## La bibliothèque

Par défaut `Documents\Verbatim` (modifiable dans **Réglages**). Un dossier par
conversation, lisible sans Verbatim :

```
2026-11-02 14h05 Point projet\
  micro.flac, pc.flac       pistes originales, sans perte
  ecoute.flac               mélange équilibré pour la lecture
  transcription.txt         dernière version, lisible
  transcriptions\…          chaque version : texte, locuteurs, réponses brutes
  note vocale.m4a           (fichiers importés) l’original intact
```

## Diagnostic

Dans le dossier de Verbatim, double-clique sur **Diagnostic audio.cmd**
(pendant 30 s, mets si possible une vidéo en fond). Le rapport s’ouvre
ensuite dans le Bloc-notes. Il liste tes micros et sorties, teste chaque capture et
fait un vrai enregistrement de 10 s (supprimé ensuite). Rien n’est envoyé sur
internet.

`Verbatim.exe --selftest rapport.txt` vérifie le chiffrement de la clé, les
dossiers et la chaîne audio, sans enregistrer.

Réglages → **Version** → **Ouvrir le journal** montre `journal.txt`. Il
garde la trace des démarrages, des périphériques, des enregistrements
(durées, niveaux, écho), des transcriptions (mode, coût estimé et réel,
nouveaux essais, erreurs), de tes corrections de voix (en nombre seulement)
et des plantages. Il ne contient ni clé, ni titre, ni nom, ni texte de
conversation : il peut être partagé pour améliorer Verbatim.

## Signaler un problème, proposer une idée

**⋯** → **Signaler un problème** : Verbatim prend une capture de sa fenêtre,
la copie dans le presse-papiers et prépare un texte avec la version et la fin
du journal (que tu peux lire avant). **Ouvrir sur GitHub** ouvre une page
pré-remplie sur le dépôt de Verbatim : colle la capture avec Ctrl+V, puis
valide. **Proposer une idée** fonctionne pareil, sans capture.

C’est publié sur GitHub, **visible par tous** : ne colle pas une capture qui
montre une conversation privée. Les captures restent aussi dans
`%LOCALAPPDATA%\Verbatim\captures` (les 10 dernières).

Pourquoi c’est sûr : GitHub publie chaque message sous le compte de la
personne connectée. Quelqu’un qui installe Verbatim et propose une idée la
publie sous **son** compte, pas sous le tien, et rien dans l’application ne
permet de changer ça. Tes messages portent la mention « Owner » (propriétaire
du dépôt), apposée par GitHub. Les agents qui travaillent sur Verbatim ne
suivent que les demandes du compte `Rnaock`, vérifié auprès de GitHub et
jamais d’après le texte du message. Ils traitent les autres comme de simples
suggestions (règle écrite dans `CLAUDE.md`).

## Sécurité

| Point | Traitement |
|---|---|
| Clé API | Chiffrée par DPAPI dans `%LOCALAPPDATA%\Verbatim\key.dpapi`, déchiffrable par ta session Windows seulement |
| Exposition de la clé | Elle reste dans la partie Rust de l’app. L’interface ne la reçoit jamais après sa saisie |
| Réseau | Aucun port ouvert. Seule la partie Rust contacte ElevenLabs (api.elevenlabs.io) |
| Interface | Politique CSP stricte : aucun site joignable, aucun accès direct au disque ni au shell |
| Liens | Seules les pages d’elevenlabs.io et les liens de visio (Meet, Zoom, Teams) s’ouvrent, dans ton navigateur |
| Agenda | Adresse secrète chiffrée par DPAPI (`agenda.dpapi`), jamais montrée à l’interface. Lecture seule |
| Mises à jour | Téléchargées uniquement depuis GitHub, empreinte SHA-256 vérifiée, et le fichier doit se présenter comme Verbatim avant d’être installé |
| Audio | Reste sur ton disque. Il part chez ElevenLabs uniquement quand tu transcris |

Limite structurelle : un programme malveillant lancé sous ton compte, session
ouverte, peut demander le déchiffrement de la clé comme l’app le fait. Utilise
une clé ElevenLabs dédiée limitée à *Speech to Text* (et *User → Read* pour le
solde).

## Limites connues

- **Consentement** : préviens les personnes que tu enregistres.
- La capture « tout le son du PC sauf Verbatim » demande Windows 11. Sur
  Windows 10, Verbatim capture la sortie par défaut à la place, et le
  signale.
- Deux personnes qui parlent exactement en même temps restent difficiles à
  séparer. Les corrections manuelles sont faites pour ça.
- La transcription en direct n’est pas prévue : elle se fait à l’arrêt ou à
  la demande.
- Plafonds ElevenLabs : 10 h et 5 Go par fichier envoyé. Un enregistrement
  s’arrête de lui-même au bout de 20 h.
