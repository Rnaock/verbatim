//! Mises à jour de Verbatim : depuis les « releases » GitHub du projet, ou
//! depuis un fichier (Verbatim.exe ou zip livré).
//!
//! Les données ne bougent jamais : clé et réglages dans %LOCALAPPDATA%\Verbatim,
//! conversations dans la bibliothèque. Seul l'exécutable est remplacé :
//! l'ancien est renommé en `Verbatim.old.exe` (Windows autorise le renommage
//! d'un programme en cours d'exécution), le nouveau prend sa place, puis il est
//! lancé et attend la fin de l'ancien processus.

use crate::core::{ApiError, Res, VERSION};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Dépôt GitHub public des versions (« propriétaire/depot »), fixé à la compilation.
pub const REPO: Option<&str> = option_env!("VERBATIM_UPDATE_REPO");
const EXE: &str = "Verbatim.exe";
const OLD: &str = "Verbatim.old.exe";
/// Hôtes autorisés pour le téléchargement (GitHub et son stockage).
const HOSTS: &[&str] = &["api.github.com", "github.com", "objects.githubusercontent.com", "release-assets.githubusercontent.com", "raw.githubusercontent.com"];
/// Branche du dépôt qui porte la dernière version : `latest.json` + `Verbatim.exe`.
const BRANCH: &str = "mises-a-jour";

#[derive(Debug, Clone, Serialize)]
pub struct UpdateInfo {
    pub version: String,
    pub notes: String,
    pub url: String,
    pub sha256: Option<String>,
    pub size: u64,
}

/// « 4.10.2 » → (4, 10, 2). Les suffixes éventuels sont ignorés.
pub fn parse_version(v: &str) -> Option<(u32, u32, u32)> {
    let v = v.trim().trim_start_matches(['v', 'V']);
    let mut it = v.split(|c: char| !c.is_ascii_digit()).filter(|s| !s.is_empty()).map(|s| s.parse::<u32>().ok());
    Some((it.next()??, it.next().flatten().unwrap_or(0), it.next().flatten().unwrap_or(0)))
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

fn host_ok(url: &reqwest::Url) -> bool {
    url.scheme() == "https" && url.host_str().map(|h| HOSTS.contains(&h)).unwrap_or(false)
}

/// Dernière version publiée, si elle est plus récente que celle-ci : d'abord
/// la branche de mises à jour du dépôt, sinon ses « releases » GitHub.
pub async fn check(http: &reqwest::Client) -> Res<Option<UpdateInfo>> {
    let Some(repo) = REPO else { return Ok(None) };
    match check_branch(http, repo).await {
        Ok(Some(found)) => return Ok(Some(found).filter(|f| is_newer(&f.version, VERSION))),
        Ok(None) => {}
        Err(e) if e.status != 404 => return Err(e),
        Err(_) => {}
    }
    check_release(http, repo).await
}

/// `latest.json` : {"version": "4.2.0", "sha256": "…", "size": 123, "notes": "…"}.
pub fn parse_manifest(repo: &str, v: &serde_json::Value) -> Option<UpdateInfo> {
    let version = v.get("version")?.as_str()?.trim().trim_start_matches(['v', 'V']).to_string();
    parse_version(&version)?;
    let sha = v.get("sha256")?.as_str()?.trim().to_ascii_lowercase();
    if sha.len() != 64 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return None; // pas d'installation sans empreinte
    }
    Some(UpdateInfo {
        version,
        notes: v.get("notes").and_then(|n| n.as_str()).unwrap_or_default().chars().take(4000).collect(),
        url: format!("https://raw.githubusercontent.com/{repo}/{BRANCH}/{EXE}"),
        sha256: Some(sha),
        size: v.get("size").and_then(|s| s.as_u64()).unwrap_or(0),
    })
}

async fn check_branch(http: &reqwest::Client, repo: &str) -> Res<Option<UpdateInfo>> {
    let url = format!("https://raw.githubusercontent.com/{repo}/{BRANCH}/latest.json");
    let r = http.get(&url).timeout(Duration::from_secs(20)).send().await.map_err(|e| ApiError::new(0, e.to_string()))?;
    if r.status().as_u16() == 404 {
        return Err(ApiError::new(404, "pas de branche de mises à jour"));
    }
    if !r.status().is_success() {
        return Err(ApiError::new(r.status().as_u16() as i32, "GitHub ne répond pas comme prévu"));
    }
    let body = r.bytes().await.map_err(|e| ApiError::new(0, e.to_string()))?;
    let v: serde_json::Value = serde_json::from_slice(&body).map_err(|_| ApiError::new(0, "description de version illisible"))?;
    Ok(parse_manifest(repo, &v))
}

async fn check_release(http: &reqwest::Client, repo: &str) -> Res<Option<UpdateInfo>> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let r = http
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| ApiError::new(0, e.to_string()))?;
    if r.status().as_u16() == 404 {
        return Ok(None); // aucune version publiée
    }
    if !r.status().is_success() {
        return Err(ApiError::new(r.status().as_u16() as i32, "GitHub ne répond pas comme prévu"));
    }
    let body = r.bytes().await.map_err(|e| ApiError::new(0, e.to_string()))?;
    let v: serde_json::Value = serde_json::from_slice(&body).map_err(|e| ApiError::new(0, e.to_string()))?;
    let tag = v.get("tag_name").and_then(|t| t.as_str()).unwrap_or_default();
    if !is_newer(tag, VERSION) {
        return Ok(None);
    }
    let asset = v
        .get("assets")
        .and_then(|a| a.as_array())
        .and_then(|a| a.iter().find(|x| x.get("name").and_then(|n| n.as_str()) == Some(EXE)))
        .ok_or_else(|| ApiError::new(404, "la version publiée ne contient pas Verbatim.exe"))?;
    let dl = asset.get("browser_download_url").and_then(|u| u.as_str()).unwrap_or_default().to_string();
    let parsed = reqwest::Url::parse(&dl).map_err(|_| ApiError::new(400, "adresse de téléchargement invalide"))?;
    if !host_ok(&parsed) {
        return Err(ApiError::new(400, "adresse de téléchargement refusée"));
    }
    let sha256 = asset
        .get("digest")
        .and_then(|d| d.as_str())
        .and_then(|d| d.strip_prefix("sha256:"))
        .map(|d| d.to_ascii_lowercase());
    Ok(Some(UpdateInfo {
        version: tag.trim_start_matches(['v', 'V']).to_string(),
        notes: v.get("body").and_then(|b| b.as_str()).unwrap_or_default().chars().take(4000).collect(),
        url: dl,
        sha256,
        size: asset.get("size").and_then(|s| s.as_u64()).unwrap_or(0),
    }))
}

fn staging(data_dir: &Path) -> std::io::Result<PathBuf> {
    let d = data_dir.join("mise-a-jour");
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d)?;
    Ok(d)
}

/// Télécharge l'exécutable publié et vérifie son empreinte.
pub async fn download<P: FnMut(f32)>(http: &reqwest::Client, data_dir: &Path, info: &UpdateInfo, mut progress: P) -> Res<PathBuf> {
    use futures_util::StreamExt;
    let url = reqwest::Url::parse(&info.url).map_err(|_| ApiError::new(400, "adresse invalide"))?;
    if !host_ok(&url) {
        return Err(ApiError::new(400, "adresse de téléchargement refusée"));
    }
    let r = http.get(url).timeout(Duration::from_secs(600)).send().await.map_err(|e| ApiError::new(0, e.to_string()))?;
    if !host_ok(r.url()) {
        return Err(ApiError::new(400, "redirection refusée"));
    }
    if !r.status().is_success() {
        return Err(ApiError::new(r.status().as_u16() as i32, "téléchargement refusé par GitHub"));
    }
    let total = r.content_length().unwrap_or(info.size).max(1);
    let dir = staging(data_dir).map_err(|e| ApiError::new(500, e.to_string()))?;
    let path = dir.join(EXE);
    let mut f = std::fs::File::create(&path).map_err(|e| ApiError::new(500, e.to_string()))?;
    let mut hasher = Sha256::new();
    let mut got = 0u64;
    let mut stream = r.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| ApiError::new(0, e.to_string()))?;
        hasher.update(&chunk);
        f.write_all(&chunk).map_err(|e| ApiError::new(500, e.to_string()))?;
        got += chunk.len() as u64;
        progress((got as f32 / total as f32).min(1.0));
    }
    f.sync_all().map_err(|e| ApiError::new(500, e.to_string()))?;
    let digest = hex(&hasher.finalize());
    if let Some(expected) = &info.sha256 {
        if &digest != expected {
            let _ = std::fs::remove_file(&path);
            return Err(ApiError::new(400, "fichier téléchargé corrompu (empreinte différente) : rien n'a été modifié"));
        }
    }
    Ok(path)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Prépare un fichier choisi par l'utilisateur : un Verbatim.exe, ou un zip qui
/// en contient un. Renvoie le chemin de l'exécutable à installer.
pub fn stage_file(data_dir: &Path, src: &Path) -> Result<PathBuf, String> {
    let ext = src.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let dir = staging(data_dir).map_err(|e| e.to_string())?;
    let out = dir.join(EXE);
    match ext.as_str() {
        "exe" => {
            std::fs::copy(src, &out).map_err(|e| format!("copie impossible : {e}"))?;
        }
        "zip" => {
            let f = std::fs::File::open(src).map_err(|e| e.to_string())?;
            let mut z = zip::ZipArchive::new(f).map_err(|_| "ce zip est illisible".to_string())?;
            let idx = (0..z.len())
                .find(|&i| z.by_index(i).map(|e| e.name().rsplit('/').next() == Some(EXE)).unwrap_or(false))
                .ok_or("ce zip ne contient pas Verbatim.exe")?;
            let mut e = z.by_index(idx).map_err(|e| e.to_string())?;
            if e.size() > 200_000_000 {
                return Err("fichier anormalement gros".into());
            }
            let mut buf = Vec::with_capacity(e.size() as usize);
            e.read_to_end(&mut buf).map_err(|e| e.to_string())?;
            std::fs::write(&out, buf).map_err(|e| e.to_string())?;
        }
        _ => return Err("choisis un fichier Verbatim.exe ou le zip d'une version de Verbatim".into()),
    }
    Ok(out)
}

/// Lance le candidat avec `--version-file` : il doit répondre « Verbatim x.y.z ».
/// Évite d'installer un autre programme par erreur, ou un fichier tronqué.
pub fn probe(candidate: &Path) -> Result<String, String> {
    let report = candidate.with_extension("version.txt");
    let _ = std::fs::remove_file(&report);
    let mut child = std::process::Command::new(candidate)
        .arg("--version-file")
        .arg(&report)
        .spawn()
        .map_err(|e| format!("ce fichier ne se lance pas : {e}"))?;
    let start = std::time::Instant::now();
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        if start.elapsed() > Duration::from_secs(20) {
            let _ = child.kill();
            return Err("ce fichier ne répond pas comme Verbatim".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let text = std::fs::read_to_string(&report).map_err(|_| "ce fichier n'est pas une version de Verbatim".to_string())?;
    let _ = std::fs::remove_file(&report);
    let v = text.trim().strip_prefix("Verbatim ").ok_or("ce fichier n'est pas une version de Verbatim")?;
    parse_version(v).ok_or("version illisible")?;
    Ok(v.to_string())
}

/// Remplace l'exécutable en cours par `candidate`. En cas d'échec, l'ancien est
/// remis en place. Renvoie le chemin du programme à relancer.
pub fn swap(candidate: &Path) -> Result<PathBuf, String> {
    let current = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = current.parent().ok_or("dossier de Verbatim introuvable")?.to_path_buf();
    let old = dir.join(OLD);
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&current, &old).map_err(|e| format!("impossible de remplacer Verbatim (dossier protégé ?) : {e}"))?;
    if let Err(e) = std::fs::copy(candidate, &current) {
        let _ = std::fs::rename(&old, &current);
        return Err(format!("copie de la nouvelle version impossible : {e}"));
    }
    Ok(current)
}

/// Relance Verbatim : le nouveau processus attend la fin de celui-ci.
pub fn relaunch(exe: &Path) -> Result<(), String> {
    std::process::Command::new(exe)
        .arg("--wait-pid")
        .arg(std::process::id().to_string())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("relance impossible : {e}"))
}

/// Au démarrage : ménage de l'ancienne version et du dossier de préparation.
pub fn cleanup(data_dir: &Path) {
    if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
        for _ in 0..5 {
            let old = dir.join(OLD);
            if !old.exists() || std::fs::remove_file(&old).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(400));
        }
    }
    let _ = std::fs::remove_dir_all(data_dir.join("mise-a-jour"));
}

/// Attend la fin d'un processus (l'ancienne version), au plus `limit`.
pub fn wait_for_pid(pid: u32, limit: Duration) {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};
        if let Ok(h) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
            let _ = WaitForSingleObject(h, limit.as_millis() as u32);
            let _ = CloseHandle(h);
        }
    }
    #[cfg(not(windows))]
    {
        let start = std::time::Instant::now();
        while std::path::Path::new(&format!("/proc/{pid}")).exists() && start.elapsed() < limit {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(parse_version("v4.1.0"), Some((4, 1, 0)));
        assert_eq!(parse_version("4.10"), Some((4, 10, 0)));
        assert!(is_newer("v4.2.0", "4.1.9"));
        assert!(is_newer("4.10.0", "4.9.3"));
        assert!(!is_newer("v4.1.0", "4.1.0"));
        assert!(!is_newer("n'importe quoi", "4.1.0"));
    }

    #[test]
    fn description_de_version() {
        let ok = serde_json::json!({"version": "v4.2.0", "sha256": "AB".repeat(32), "size": 10, "notes": "Agenda"});
        let u = parse_manifest("Rnaock/verbatim", &ok).unwrap();
        assert_eq!(u.version, "4.2.0");
        assert_eq!(u.url, "https://raw.githubusercontent.com/Rnaock/verbatim/mises-a-jour/Verbatim.exe");
        assert!(host_ok(&reqwest::Url::parse(&u.url).unwrap()));
        assert_eq!(u.sha256.as_deref(), Some("ab".repeat(32).as_str()));
        assert!(parse_manifest("x/y", &serde_json::json!({"version": "4.2.0"})).is_none(), "sans empreinte : refusé");
        assert!(parse_manifest("x/y", &serde_json::json!({"version": "4.2.0", "sha256": "zz"})).is_none());
    }

    #[test]
    fn hotes_autorises() {
        assert!(host_ok(&reqwest::Url::parse("https://github.com/x/verbatim/releases/download/v4.2.0/Verbatim.exe").unwrap()));
        assert!(!host_ok(&reqwest::Url::parse("http://github.com/x").unwrap()));
        assert!(!host_ok(&reqwest::Url::parse("https://github.com.evil.example/x").unwrap()));
    }

    #[test]
    fn fichier_zip_ou_exe() {
        let base = std::env::temp_dir().join(format!("vb-upd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let zp = base.join("Verbatim-4.2.zip");
        {
            let mut z = zip::ZipWriter::new(std::fs::File::create(&zp).unwrap());
            z.start_file("Verbatim/LISEZMOI.md", zip::write::SimpleFileOptions::default()).unwrap();
            z.write_all(b"doc").unwrap();
            z.start_file("Verbatim/Verbatim.exe", zip::write::SimpleFileOptions::default()).unwrap();
            z.write_all(b"MZ-binaire").unwrap();
            z.finish().unwrap();
        }
        let data = base.join("data");
        let out = stage_file(&data, &zp).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), b"MZ-binaire");
        assert!(stage_file(&data, &base.join("photo.png")).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }
}
