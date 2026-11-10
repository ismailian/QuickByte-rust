//! Windows' app mode, for `AppTheme::System` (the lookup in `Theme.cs`).

use windows_registry::CURRENT_USER;

const PERSONALIZE_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize";

/// `HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize\AppsUseLightTheme`;
/// true (light) when it cannot be read.
pub fn apps_use_light_theme() -> bool {
    CURRENT_USER
        .open(PERSONALIZE_KEY)
        .and_then(|key| key.get_u32("AppsUseLightTheme"))
        .map(|value| value != 0)
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    #[test]
    fn reading_the_app_mode_never_fails() {
        // Either answer is right on some machine; what matters is that it answers.
        let _ = super::apps_use_light_theme();
    }
}
