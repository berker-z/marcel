use std::borrow::Cow;

use gpui::{App, SharedString};
use gpui_component::Theme;

pub const DEFAULT_FONT_FAMILY: &str = "Marcel Iosevka";
pub const FONT_FAMILY_ENV: &str = "MARCEL_FONT_FAMILY";

static REGULAR_FONT: &[u8] = include_bytes!("../assets/fonts/MarcelIosevka-Regular.ttf");
static SEMIBOLD_FONT: &[u8] = include_bytes!("../assets/fonts/MarcelIosevka-SemiBold.ttf");

pub fn init(cx: &mut App) {
    if let Err(error) = cx.text_system().add_fonts(vec![
        Cow::Borrowed(REGULAR_FONT),
        Cow::Borrowed(SEMIBOLD_FONT),
    ]) {
        eprintln!("failed to load Marcel's bundled font: {error}");
    }

    let available = cx.text_system().all_font_names();
    let requested = std::env::var(FONT_FAMILY_ENV).ok();
    let unavailable_override = requested.as_deref().is_some_and(|requested| {
        let requested = requested.trim();
        !requested.is_empty()
            && !available
                .iter()
                .any(|family| family.eq_ignore_ascii_case(requested))
    });
    let family = select_font_family(requested.as_deref(), &available);
    if unavailable_override {
        eprintln!("{FONT_FAMILY_ENV} requested an unavailable font; using {DEFAULT_FONT_FAMILY}");
    }

    let family = SharedString::from(family);
    let theme = Theme::global_mut(cx);
    theme.font_family = family.clone();
    theme.mono_font_family = family;
}

fn select_font_family(requested: Option<&str>, available: &[String]) -> String {
    let requested = requested.map(str::trim).filter(|value| !value.is_empty());
    requested
        .and_then(|requested| {
            available
                .iter()
                .find(|family| family.eq_ignore_ascii_case(requested))
        })
        .cloned()
        .unwrap_or_else(|| DEFAULT_FONT_FAMILY.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_font_is_the_unconditional_default() {
        assert_eq!(
            select_font_family(None, &["DejaVu Sans".to_string()]),
            DEFAULT_FONT_FAMILY
        );
    }

    #[test]
    fn explicit_available_font_overrides_the_bundle() {
        assert_eq!(
            select_font_family(
                Some("ibm plex mono"),
                &["IBM Plex Mono".to_string(), DEFAULT_FONT_FAMILY.to_string()]
            ),
            "IBM Plex Mono"
        );
    }

    #[test]
    fn empty_or_unknown_overrides_fall_back_to_the_bundle() {
        let available = [DEFAULT_FONT_FAMILY.to_string()];
        assert_eq!(
            select_font_family(Some("  "), &available),
            DEFAULT_FONT_FAMILY
        );
        assert_eq!(
            select_font_family(Some("Missing Mono"), &available),
            DEFAULT_FONT_FAMILY
        );
    }
}
