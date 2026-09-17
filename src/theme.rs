//! Marcel's colour palettes, and the one active at a time.

use std::sync::atomic::{AtomicU8, Ordering};

use gpui::{App, Hsla, rgb};
use gpui_component::{Colorize, Theme, ThemeColor, ThemeMode, ThemeTokens, scroll::ScrollbarShow};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum Palette {
    #[default]
    Nord,
    GruvboxDark,
    TokyoNight,
    CatppuccinMocha,
    Dracula,
    OneDark,
    SolarizedDark,
    EverforestDark,
    RosePine,
    KanagawaWave,
    SystemDark,
    SystemLight,
}

/// The sixteen colours a dark palette is built from; `dark_colors` names them.
type Swatches = [u32; 16];

/// One row per palette: the label shown in Settings, the names accepted from
/// `MARCEL_THEME`, and the swatches — `None` for the two that use
/// gpui-component's built-in schemes.
const PALETTES: [(Palette, &str, &[&str], Option<Swatches>); 12] = [
    (
        Palette::Nord,
        "Nord",
        &["nord", "nord-dark"],
        Some([
            0x2e3440, 0x3b4252, 0x434c5e, 0x434c5e, 0x4c566a, 0xd8dee9, 0xaab2c0, 0x88c0d0,
            0x8fbcbb, 0x5e81ac, 0x81a1c1, 0xbf616a, 0xd08770, 0xebcb8b, 0xa3be8c, 0xb48ead,
        ]),
    ),
    (
        Palette::GruvboxDark,
        "Gruvbox Dark",
        &["gruvbox", "gruvbox-dark"],
        Some([
            0x1d2021, 0x282828, 0x3c3836, 0x504945, 0x665c54, 0xebdbb2, 0xa89984, 0x8ec07c,
            0xb8bb26, 0x83a598, 0x83a598, 0xfb4934, 0xfe8019, 0xfabd2f, 0xb8bb26, 0xd3869b,
        ]),
    ),
    (
        Palette::TokyoNight,
        "Tokyo Night",
        &["tokyo-night", "tokyonight"],
        Some([
            0x1a1b26, 0x24283b, 0x292e42, 0x3b4261, 0x414868, 0xc0caf5, 0xa9b1d6, 0x7dcfff,
            0x9ece6a, 0x7aa2f7, 0x7aa2f7, 0xf7768e, 0xff9e64, 0xe0af68, 0x9ece6a, 0xbb9af7,
        ]),
    ),
    (
        Palette::CatppuccinMocha,
        "Catppuccin Mocha",
        &["catppuccin", "catppuccin-mocha", "mocha"],
        Some([
            0x11111b, 0x1e1e2e, 0x313244, 0x45475a, 0x585b70, 0xcdd6f4, 0xa6adc8, 0x94e2d5,
            0xa6e3a1, 0x89b4fa, 0x89b4fa, 0xf38ba8, 0xfab387, 0xf9e2af, 0xa6e3a1, 0xcba6f7,
        ]),
    ),
    (
        Palette::Dracula,
        "Dracula",
        &["dracula"],
        Some([
            0x21222c, 0x282a36, 0x343746, 0x44475a, 0x6272a4, 0xf8f8f2, 0xbfbfbf, 0x8be9fd,
            0x50fa7b, 0xbd93f9, 0xbd93f9, 0xff5555, 0xffb86c, 0xf1fa8c, 0x50fa7b, 0xff79c6,
        ]),
    ),
    (
        Palette::OneDark,
        "One Dark",
        &["one-dark", "onedark"],
        Some([
            0x21252b, 0x282c34, 0x2c323c, 0x3e4451, 0x4b5263, 0xabb2bf, 0x7f848e, 0x56b6c2,
            0x98c379, 0x61afef, 0x61afef, 0xe06c75, 0xd19a66, 0xe5c07b, 0x98c379, 0xc678dd,
        ]),
    ),
    (
        Palette::SolarizedDark,
        "Solarized Dark",
        &["solarized", "solarized-dark"],
        Some([
            0x002b36, 0x073642, 0x164954, 0x285762, 0x586e75, 0x93a1a1, 0x839496, 0x2aa198,
            0x859900, 0x268bd2, 0x268bd2, 0xdc322f, 0xcb4b16, 0xb58900, 0x859900, 0xd33682,
        ]),
    ),
    (
        Palette::EverforestDark,
        "Everforest Dark",
        &["everforest", "everforest-dark"],
        Some([
            0x1e2326, 0x272e33, 0x2e383c, 0x374145, 0x4f5b58, 0xd3c6aa, 0x859289, 0x83c092,
            0xa7c080, 0x7fbbb3, 0x7fbbb3, 0xe67e80, 0xe69875, 0xdbbc7f, 0xa7c080, 0xd699b6,
        ]),
    ),
    (
        Palette::RosePine,
        "Rosé Pine",
        &["rose-pine", "rosé-pine", "rosepine"],
        Some([
            0x191724, 0x1f1d2e, 0x26233a, 0x403d52, 0x524f67, 0xe0def4, 0x908caa, 0x9ccfd8,
            0xc4a7e7, 0x31748f, 0x31748f, 0xeb6f92, 0xea9a97, 0xf6c177, 0x9ccfd8, 0xc4a7e7,
        ]),
    ),
    (
        Palette::KanagawaWave,
        "Kanagawa Wave",
        &["kanagawa", "kanagawa-wave"],
        Some([
            0x16161d, 0x1f1f28, 0x2a2a37, 0x363646, 0x54546d, 0xdcd7ba, 0x727169, 0x6a9589,
            0x98bb6c, 0x7e9cd8, 0x7e9cd8, 0xe46876, 0xffa066, 0xe6c384, 0x98bb6c, 0x957fb8,
        ]),
    ),
    (
        Palette::SystemDark,
        "System Dark",
        &["dark", "default-dark", "system-dark"],
        None,
    ),
    (
        Palette::SystemLight,
        "System Light",
        &["light", "default-light", "system-light"],
        None,
    ),
];

impl Palette {
    pub const ALL: [Self; 12] = {
        let mut all = [Self::Nord; 12];
        let mut index = 0;
        while index < 12 {
            all[index] = PALETTES[index].0;
            index += 1;
        }
        all
    };

    fn row(
        self,
    ) -> &'static (
        Palette,
        &'static str,
        &'static [&'static str],
        Option<Swatches>,
    ) {
        &PALETTES[self as usize]
    }

    pub fn from_name(name: &str) -> Option<Self> {
        let normalized = name.trim().to_ascii_lowercase().replace([' ', '_'], "-");
        PALETTES
            .iter()
            .find(|(_, _, names, _)| names.contains(&normalized.as_str()))
            .map(|(palette, ..)| *palette)
    }

    pub fn label(self) -> &'static str {
        self.row().1
    }

    fn from_environment() -> Self {
        std::env::var("MARCEL_THEME")
            .ok()
            .as_deref()
            .and_then(Self::from_name)
            .unwrap_or_default()
    }
}

static ACTIVE_PALETTE: AtomicU8 = AtomicU8::new(Palette::Nord as u8);

pub fn init(cx: &mut App) {
    apply(Palette::from_environment(), cx);
}

pub fn active() -> Palette {
    Palette::ALL[ACTIVE_PALETTE.load(Ordering::Relaxed) as usize]
}

pub fn apply(palette: Palette, cx: &mut App) {
    let typography = {
        let theme = Theme::global(cx);
        (
            theme.font_family.clone(),
            theme.font_size,
            theme.mono_font_family.clone(),
            theme.mono_font_size,
            theme.radius,
            theme.radius_lg,
        )
    };
    let mode = if palette == Palette::SystemLight {
        ThemeMode::Light
    } else {
        ThemeMode::Dark
    };
    Theme::change(mode, None, cx);

    let theme = Theme::global_mut(cx);
    if let Some(colors) = colors_for(palette) {
        theme.colors = colors;
        theme.tokens = ThemeTokens::from(colors);
    }
    theme.scrollbar_show = ScrollbarShow::Always;
    theme.font_family = typography.0;
    theme.font_size = typography.1;
    theme.mono_font_family = typography.2;
    theme.mono_font_size = typography.3;
    theme.radius = typography.4;
    theme.radius_lg = typography.5;

    ACTIVE_PALETTE.store(palette as u8, Ordering::Relaxed);
    cx.refresh_windows();
}

fn colors_for(palette: Palette) -> Option<ThemeColor> {
    palette.row().3.map(dark_colors)
}

fn dark_colors(swatches: Swatches) -> ThemeColor {
    // In table order: the three surfaces (window shell, browser, raised), the
    // hover tint, the border, two foregrounds, three accent states, and the
    // six named hues.
    let [
        shell,
        surface,
        raised,
        hover,
        border,
        foreground,
        muted_foreground,
        accent,
        accent_hover,
        accent_active,
        blue,
        red,
        orange,
        yellow,
        green,
        purple,
    ] = swatches.map(|hex| Hsla::from(rgb(hex)));
    ThemeColor {
        accent: raised,
        accent_foreground: foreground,
        accordion: surface,
        accordion_hover: hover,
        background: surface,
        border,
        button: raised,
        button_active: border,
        button_foreground: foreground,
        button_hover: hover,
        button_danger: red,
        button_danger_active: red.darken(0.12),
        button_danger_foreground: foreground,
        button_danger_hover: red.lighten(0.08),
        button_info: blue,
        button_info_active: accent_active,
        button_info_foreground: foreground,
        button_info_hover: accent,
        button_primary: accent,
        button_primary_active: accent_active,
        button_primary_foreground: shell,
        button_primary_hover: accent_hover,
        button_secondary: raised,
        button_secondary_active: border,
        button_secondary_foreground: foreground,
        button_secondary_hover: hover,
        button_success: green,
        button_success_active: green.darken(0.12),
        button_success_foreground: shell,
        button_success_hover: green.lighten(0.08),
        button_warning: yellow,
        button_warning_active: orange,
        button_warning_foreground: shell,
        button_warning_hover: yellow.lighten(0.08),
        group_box: surface,
        group_box_foreground: foreground,
        caret: accent,
        chart_1: accent,
        chart_2: green,
        chart_3: yellow,
        chart_4: purple,
        chart_5: red,
        danger: red,
        danger_active: red.darken(0.12),
        danger_foreground: foreground,
        danger_hover: red.lighten(0.08),
        description_list_label: surface,
        description_list_label_foreground: foreground,
        drag_border: accent,
        drop_target: accent.opacity(0.2),
        foreground,
        info: blue,
        info_active: accent_active,
        info_foreground: foreground,
        info_hover: accent,
        input: border,
        link: accent,
        link_active: accent_active,
        link_hover: accent_hover,
        list: surface,
        list_active: accent.opacity(0.18),
        list_active_border: accent,
        list_even: surface,
        list_head: raised,
        list_hover: hover,
        muted: raised,
        muted_foreground,
        popover: raised,
        popover_foreground: foreground,
        primary: accent,
        primary_active: accent_active,
        primary_foreground: shell,
        primary_hover: accent_hover,
        progress_bar: accent,
        ring: accent,
        scrollbar: shell,
        scrollbar_thumb: border,
        scrollbar_thumb_hover: accent_active,
        secondary: raised,
        secondary_active: border,
        secondary_foreground: foreground,
        secondary_hover: hover,
        selection: accent.opacity(0.26),
        sidebar: shell,
        sidebar_accent: raised,
        sidebar_accent_foreground: foreground,
        sidebar_border: border,
        sidebar_foreground: foreground,
        sidebar_primary: accent,
        sidebar_primary_foreground: shell,
        skeleton: raised,
        slider_bar: border,
        slider_thumb: foreground,
        success: green,
        success_foreground: shell,
        success_hover: green.lighten(0.08),
        success_active: green.darken(0.12),
        chart_bullish: green,
        chart_bearish: red,
        switch: border,
        switch_thumb: foreground,
        tab: shell,
        tab_active: raised,
        tab_active_foreground: foreground,
        tab_bar: shell,
        tab_bar_segmented: raised,
        tab_foreground: foreground,
        table: surface,
        table_active: accent.opacity(0.18),
        table_active_border: accent,
        table_even: surface,
        table_head: raised,
        table_head_foreground: foreground,
        table_foot: surface,
        table_foot_foreground: foreground,
        table_hover: hover,
        table_row_border: raised,
        title_bar: shell,
        title_bar_border: border,
        status_bar: shell,
        status_bar_border: border,
        tiles: surface,
        warning: yellow,
        warning_active: orange,
        warning_hover: yellow.lighten(0.08),
        warning_foreground: shell,
        overlay: shell.opacity(0.72),
        window_border: border,
        red,
        red_light: red.lighten(0.15),
        green,
        green_light: green.lighten(0.15),
        blue,
        blue_light: blue.lighten(0.15),
        yellow,
        yellow_light: yellow.lighten(0.15),
        magenta: purple,
        magenta_light: purple.lighten(0.15),
        cyan: accent,
        cyan_light: accent.lighten(0.15),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_supported_palette_names() {
        assert_eq!(Palette::from_name("nord"), Some(Palette::Nord));
        assert_eq!(
            Palette::from_name("GRUVBOX-DARK"),
            Some(Palette::GruvboxDark)
        );
        assert_eq!(
            Palette::from_name("catppuccin"),
            Some(Palette::CatppuccinMocha)
        );
        assert_eq!(
            Palette::from_name("default-dark"),
            Some(Palette::SystemDark)
        );
        assert_eq!(Palette::from_name("light"), Some(Palette::SystemLight));
        assert_eq!(Palette::from_name("unknown"), None);
    }

    #[test]
    fn palette_labels_round_trip_and_are_unique() {
        let mut labels = Palette::ALL
            .iter()
            .map(|palette| palette.label())
            .collect::<Vec<_>>();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), Palette::ALL.len());

        for palette in Palette::ALL {
            assert_eq!(Palette::from_name(palette.label()), Some(palette));
        }
    }

    #[test]
    fn every_custom_palette_has_distinct_shell_and_browser_surfaces() {
        for palette in Palette::ALL {
            if let Some(colors) = colors_for(palette) {
                assert_ne!(colors.sidebar, colors.background, "{}", palette.label());
                assert_ne!(colors.list_hover, colors.background, "{}", palette.label());
                assert_ne!(colors.list_active, colors.popover, "{}", palette.label());
            }
        }
    }

    #[test]
    fn custom_palette_tokens_match_the_declared_component_colors() {
        let colors = colors_for(Palette::Nord).unwrap();
        let tokens = ThemeTokens::from(colors);

        assert_eq!(tokens.background.color, colors.background);
        assert_eq!(tokens.primary.color, colors.primary);
        assert_eq!(tokens.switch.color, colors.switch);
        assert_eq!(tokens.switch_thumb.color, colors.switch_thumb);
    }
}
