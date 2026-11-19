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

/// Fonts to fall back to for characters the UI and mono fonts lack: the
/// status marks (✔ ✘ ⏳ ⊘ ⏹) aren't in Noto Sans or most monospace fonts, and
/// gpui only falls back to fonts it is told about. macOS falls back itself.
pub fn fallbacks() -> Option<gpui_kit::FontFallbacks> {
    static FALLBACKS: std::sync::OnceLock<Option<gpui_kit::FontFallbacks>> = std::sync::OnceLock::new();
    FALLBACKS
        .get_or_init(|| {
            if cfg!(target_os = "macos") {
                return None;
            }
            // A font with every mark, as fontconfig would pick it.
            let symbols = fontconfig(":charset=2714 2718 23f3 2298 23f9")?;
            Some(gpui_kit::FontFallbacks::from_fonts(vec![symbols.to_string()]))
        })
        .clone()
}

/// The family fontconfig picks for a pattern: a generic alias (what the
/// desktop uses) or a character set.
fn fontconfig(alias: &str) -> Option<SharedString> {
    let out = std::process::Command::new("fc-match")
        .args(["-f", "%{family[0]}", alias])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let family = String::from_utf8(out.stdout).ok()?.trim().to_owned();
    (!family.is_empty()).then(|| family.into())
}
