// Hide the console window on Windows release builds.
#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]

mod api;
mod jellyfin;
mod ssh;
mod util;
#[cfg(feature = "gui")]
mod gui;

/// Port the app window uses (fixed so the page's saved view settings persist between launches).
const APP_PORT: u16 = 47297;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let flag = |f: &str| args.iter().any(|a| a == f);
    let port: u16 = args.iter().position(|a| a == "--port").and_then(|i| args.get(i + 1)).and_then(|p| p.parse().ok()).unwrap_or(APP_PORT);
    let app = api::App::new();

    #[cfg(feature = "gui")]
    if !flag("--server-only") {
        return gui::run(app, port);
    }

    // Headless: just the local server (used for development and tests).
    let _ = flag;
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    if let Err(e) = rt.block_on(api::serve(app, port, |_| {})) {
        eprintln!("Dropfin couldn't start: {e}");
        std::process::exit(1);
    }
}
