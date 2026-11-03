# Compiler Verbatim

Projet Tauri 2. `src/core.rs` porte la logique (testée seule), `src/vault.rs`
le chiffrement DPAPI, `src/lib.rs` les commandes Tauri, `ui/` l’interface.

## Sous Windows (voie officielle)

Prérequis : Rust (rustup, cible MSVC) et les Build Tools de Visual Studio.

    cargo build --release

L’exécutable sort dans `target\release\Verbatim.exe`. La cible MSVC intègre le
chargeur WebView2 : aucune DLL à joindre.

## Depuis Linux (comment les versions publiées sont produites)

Cible `x86_64-pc-windows-gnu`, avec la bibliothèque standard recompilée depuis
les sources (paquets Ubuntu `rustc-1.91 cargo-1.91 rust-1.91-src mingw-w64`) :

1. Copier `library/` des sources Rust, réajouter dans `library/std/Cargo.toml`
   la dépendance `[target.'cfg(windows)'.dependencies.windows-targets]`
   (`path = "../windows_targets"`) retirée par Debian.
2. Compiler `library/rtstartup/rsbegin.rs` et `rsend.rs` en objets pour la
   cible, et passer par un lieur qui les substitue aux noms nus.
3. `RUSTC_BOOTSTRAP=1 __CARGO_TESTS_ONLY_SRC_ROOT=<copie>/library
   cargo build --release -Zbuild-std=std,panic_abort --target x86_64-pc-windows-gnu`

Avec cette cible, joindre `WebView2Loader.dll` (fourni par le crate
`webview2-com-sys`, dossier `x64/`) à côté de l’exe.

Le script `tests/build-windows.sh build` reprend ces étapes. La variable
`VERBATIM_UPDATE_REPO=Rnaock/verbatim` active la mise à jour automatique.

## Publier une version

La branche `mises-a-jour` contient seulement `Verbatim.exe` et `latest.json`
(`version`, `sha256`, `size`, `notes`). L’application lit ce fichier au
lancement, télécharge l’exe, vérifie l’empreinte SHA-256 puis s’installe.

## Tests

    cargo test --lib          # audio, enregistreur, transcription, agenda, mises à jour…
    python3 tests/e2e.py      # app réelle sous Xvfb + WebDriver, fausse API et faux agenda

Le banc e2e demande un build de développement (`cargo build`), Xvfb,
`webkit2gtk-driver`, `tauri-driver` et Selenium. La variable
`VERBATIM_API_BASE` (prise en compte en build de développement uniquement)
redirige l’app vers la fausse API.
