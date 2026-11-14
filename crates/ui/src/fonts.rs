//! Concrete font families for the platform. gpui needs real family names: the
//! generic aliases ("monospace", "sans-serif") do not resolve, and gpui-kit's
//! default UI font (".SystemUIFont") only exists on macOS.

use gpui_kit::SharedString;

pub fn sans() -> SharedString {
    if cfg!(target_os = "macos") {
        ".SystemUIFont".into()
    } else if cfg!(target_os = "windows") {
        "Segoe UI".into()
    } else {
        fontconfig("sans-serif").unwrap_or_else(|| "DejaVu Sans".into())
    }
}

pub fn mono() -> SharedString {
    static MONO: std::sync::OnceLock<SharedString> = std::sync::OnceLock::new();
    MONO.get_or_init(mono_uncached).clone()
}

fn mono_uncached() -> SharedString {
    if cfg!(target_os = "macos") {
        "Menlo".into()
    } else if cfg!(target_os = "windows") {
        "Consolas".into()
    } else {
        fontconfig("monospace").unwrap_or_else(|| "DejaVu Sans Mono".into())
    }
}

/// The family fontconfig picks for a generic alias (what the desktop uses).
fn fontconfig(alias: &str) -> Option<SharedString> {
    let out = std::process::Command::new("fc-match")
        .args(["-f", "%{family[0]}", alias])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let family = String::from_utf8(out.stdout).ok()?.trim().to_owned();
    (!family.is_empty()).then(|| family.into())
}
