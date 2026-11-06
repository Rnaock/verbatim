// Pas de console noire derrière la fenêtre, sauf en build de développement.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // Vérification par la mise à jour : « es-tu bien Verbatim, et lequel ? »
    if let Some(i) = args.iter().position(|a| a == "--version-file") {
        if let Some(p) = args.get(i + 1) {
            let _ = std::fs::write(p, format!("Verbatim {}", verbatim_lib::core::VERSION));
        }
        return;
    }
    if let Some(i) = args.iter().position(|a| a == "--capture-test") {
        let out = args.get(i + 1).cloned().unwrap_or_else(|| "capture-test.png".into());
        std::process::exit(if verbatim_lib::cli_capture_test(&out) { 0 } else { 1 });
    }
    if let Some(i) = args.iter().position(|a| a == "--install-update") {
        let file = args.get(i + 1).cloned().unwrap_or_default();
        let report = args.get(i + 2).cloned().unwrap_or_else(|| "verbatim-mise-a-jour.txt".into());
        std::process::exit(if verbatim_lib::cli_install(&file, &report) { 0 } else { 1 });
    }
    // Relance après mise à jour : attendre que l'ancienne version soit fermée.
    if let Some(pid) = args.iter().position(|a| a == "--wait-pid").and_then(|i| args.get(i + 1)).and_then(|p| p.parse::<u32>().ok()) {
        verbatim_lib::update::wait_for_pid(pid, std::time::Duration::from_secs(15));
    }
    for (flag, audio) in [("--selftest", false), ("--audio-test", true)] {
        if let Some(i) = args.iter().position(|a| a == flag) {
            let report = args.get(i + 1).cloned().unwrap_or_else(|| "verbatim-rapport.txt".into());
            std::process::exit(if verbatim_lib::selftest(&report, audio) { 0 } else { 1 });
        }
    }
    verbatim_lib::run();
}
