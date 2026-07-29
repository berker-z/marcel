use std::sync::atomic::{AtomicU8, Ordering};

use gpui::{App, Hsla, rgb};
use gpui_component::{Colorize, Theme, ThemeColor, ThemeMode, scroll::ScrollbarShow};

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

impl Palette {
    pub const ALL: [Self; 12] = [
        Self::Nord,
        Self::GruvboxDark,
        Self::TokyoNight,
        Self::CatppuccinMocha,
        Self::Dracula,
        Self::OneDark,
        Self::SolarizedDark,
        Self::EverforestDark,
        Self::RosePine,
        Self::KanagawaWave,
        Self::SystemDark,
        Self::SystemLight,
    ];

    pub fn from_name(name: &str) -> Option<Self> {
        let normalized = name.trim().to_ascii_lowercase().replace([' ', '_'], "-");
        match normalized.as_str() {
            "nord" | "nord-dark" => Some(Self::Nord),
            "gruvbox" | "gruvbox-dark" => Some(Self::GruvboxDark),
            "tokyo-night" | "tokyonight" => Some(Self::TokyoNight),
            "catppuccin" | "catppuccin-mocha" | "mocha" => Some(Self::CatppuccinMocha),
            "dracula" => Some(Self::Dracula),
            "one-dark" | "onedark" => Some(Self::OneDark),
            "solarized" | "solarized-dark" => Some(Self::SolarizedDark),
            "everforest" | "everforest-dark" => Some(Self::EverforestDark),
            "rose-pine" | "rosé-pine" | "rosepine" => Some(Self::RosePine),
            "kanagawa" | "kanagawa-wave" => Some(Self::KanagawaWave),
            "dark" | "default-dark" | "system-dark" => Some(Self::SystemDark),
            "light" | "default-light" | "system-light" => Some(Self::SystemLight),
            _ => None,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Nord => "Nord",
            Self::GruvboxDark => "Gruvbox Dark",
            Self::TokyoNight => "Tokyo Night",
            Self::CatppuccinMocha => "Catppuccin Mocha",
            Self::Dracula => "Dracula",
            Self::OneDark => "One Dark",
            Self::SolarizedDark => "Solarized Dark",
            Self::EverforestDark => "Everforest Dark",
            Self::RosePine => "Rosé Pine",
            Self::KanagawaWave => "Kanagawa Wave",
            Self::SystemDark => "System Dark",
            Self::SystemLight => "System Light",
        }
    }

    fn from_environment() -> Self {
        std::env::var("MARCEL_THEME")
            .ok()
            .as_deref()
            .and_then(Self::from_name)
            .unwrap_or_default()
    }

    const fn from_discriminant(value: u8) -> Self {
        match value {
            1 => Self::GruvboxDark,
            2 => Self::TokyoNight,
            3 => Self::CatppuccinMocha,
            4 => Self::Dracula,
            5 => Self::OneDark,
            6 => Self::SolarizedDark,
            7 => Self::EverforestDark,
            8 => Self::RosePine,
            9 => Self::KanagawaWave,
            10 => Self::SystemDark,
            11 => Self::SystemLight,
            _ => Self::Nord,
        }
    }
}

static ACTIVE_PALETTE: AtomicU8 = AtomicU8::new(Palette::Nord as u8);

pub fn init(cx: &mut App) {
    apply(Palette::from_environment(), cx);
}

pub fn active() -> Palette {
    Palette::from_discriminant(ACTIVE_PALETTE.load(Ordering::Relaxed))
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

#[derive(Clone, Copy)]
struct DarkScheme {
    shell: u32,
    surface: u32,
    raised: u32,
    hover: u32,
    border: u32,
    foreground: u32,
    muted_foreground: u32,
    accent: u32,
    accent_hover: u32,
    accent_active: u32,
    blue: u32,
    red: u32,
    orange: u32,
    yellow: u32,
    green: u32,
    purple: u32,
}

fn colors_for(palette: Palette) -> Option<ThemeColor> {
    let scheme = match palette {
        Palette::Nord => DarkScheme {
            shell: 0x2e3440,
            surface: 0x3b4252,
            raised: 0x434c5e,
            hover: 0x434c5e,
            border: 0x4c566a,
            foreground: 0xd8dee9,
            muted_foreground: 0xaab2c0,
            accent: 0x88c0d0,
            accent_hover: 0x8fbcbb,
            accent_active: 0x5e81ac,
            blue: 0x81a1c1,
            red: 0xbf616a,
            orange: 0xd08770,
            yellow: 0xebcb8b,
            green: 0xa3be8c,
            purple: 0xb48ead,
        },
        Palette::GruvboxDark => DarkScheme {
            shell: 0x1d2021,
            surface: 0x282828,
            raised: 0x3c3836,
            hover: 0x504945,
            border: 0x665c54,
            foreground: 0xebdbb2,
            muted_foreground: 0xa89984,
            accent: 0x8ec07c,
            accent_hover: 0xb8bb26,
            accent_active: 0x83a598,
            blue: 0x83a598,
            red: 0xfb4934,
            orange: 0xfe8019,
            yellow: 0xfabd2f,
            green: 0xb8bb26,
            purple: 0xd3869b,
        },
        Palette::TokyoNight => DarkScheme {
            shell: 0x1a1b26,
            surface: 0x24283b,
            raised: 0x292e42,
            hover: 0x3b4261,
            border: 0x414868,
            foreground: 0xc0caf5,
            muted_foreground: 0xa9b1d6,
            accent: 0x7dcfff,
            accent_hover: 0x9ece6a,
            accent_active: 0x7aa2f7,
            blue: 0x7aa2f7,
            red: 0xf7768e,
            orange: 0xff9e64,
            yellow: 0xe0af68,
            green: 0x9ece6a,
            purple: 0xbb9af7,
        },
        Palette::CatppuccinMocha => DarkScheme {
            shell: 0x11111b,
            surface: 0x1e1e2e,
            raised: 0x313244,
            hover: 0x45475a,
            border: 0x585b70,
            foreground: 0xcdd6f4,
            muted_foreground: 0xa6adc8,
            accent: 0x94e2d5,
            accent_hover: 0xa6e3a1,
            accent_active: 0x89b4fa,
            blue: 0x89b4fa,
            red: 0xf38ba8,
            orange: 0xfab387,
            yellow: 0xf9e2af,
            green: 0xa6e3a1,
            purple: 0xcba6f7,
        },
        Palette::Dracula => DarkScheme {
            shell: 0x21222c,
            surface: 0x282a36,
            raised: 0x343746,
            hover: 0x44475a,
            border: 0x6272a4,
            foreground: 0xf8f8f2,
            muted_foreground: 0xbfbfbf,
            accent: 0x8be9fd,
            accent_hover: 0x50fa7b,
            accent_active: 0xbd93f9,
            blue: 0xbd93f9,
            red: 0xff5555,
            orange: 0xffb86c,
            yellow: 0xf1fa8c,
            green: 0x50fa7b,
            purple: 0xff79c6,
        },
        Palette::OneDark => DarkScheme {
            shell: 0x21252b,
            surface: 0x282c34,
            raised: 0x2c323c,
            hover: 0x3e4451,
            border: 0x4b5263,
            foreground: 0xabb2bf,
            muted_foreground: 0x7f848e,
            accent: 0x56b6c2,
            accent_hover: 0x98c379,
            accent_active: 0x61afef,
            blue: 0x61afef,
            red: 0xe06c75,
            orange: 0xd19a66,
            yellow: 0xe5c07b,
            green: 0x98c379,
            purple: 0xc678dd,
        },
        Palette::SolarizedDark => DarkScheme {
            shell: 0x002b36,
            surface: 0x073642,
            raised: 0x164954,
            hover: 0x285762,
            border: 0x586e75,
            foreground: 0x93a1a1,
            muted_foreground: 0x839496,
            accent: 0x2aa198,
            accent_hover: 0x859900,
            accent_active: 0x268bd2,
            blue: 0x268bd2,
            red: 0xdc322f,
            orange: 0xcb4b16,
            yellow: 0xb58900,
            green: 0x859900,
            purple: 0xd33682,
        },
        Palette::EverforestDark => DarkScheme {
            shell: 0x1e2326,
            surface: 0x272e33,
            raised: 0x2e383c,
            hover: 0x374145,
            border: 0x4f5b58,
            foreground: 0xd3c6aa,
            muted_foreground: 0x859289,
            accent: 0x83c092,
            accent_hover: 0xa7c080,
            accent_active: 0x7fbbb3,
            blue: 0x7fbbb3,
            red: 0xe67e80,
            orange: 0xe69875,
            yellow: 0xdbbc7f,
            green: 0xa7c080,
            purple: 0xd699b6,
        },
        Palette::RosePine => DarkScheme {
            shell: 0x191724,
            surface: 0x1f1d2e,
            raised: 0x26233a,
            hover: 0x403d52,
            border: 0x524f67,
            foreground: 0xe0def4,
            muted_foreground: 0x908caa,
            accent: 0x9ccfd8,
            accent_hover: 0xc4a7e7,
            accent_active: 0x31748f,
            blue: 0x31748f,
            red: 0xeb6f92,
            orange: 0xea9a97,
            yellow: 0xf6c177,
            green: 0x9ccfd8,
            purple: 0xc4a7e7,
        },
        Palette::KanagawaWave => DarkScheme {
            shell: 0x16161d,
            surface: 0x1f1f28,
            raised: 0x2a2a37,
            hover: 0x363646,
            border: 0x54546d,
            foreground: 0xdcd7ba,
            muted_foreground: 0x727169,
            accent: 0x6a9589,
            accent_hover: 0x98bb6c,
            accent_active: 0x7e9cd8,
            blue: 0x7e9cd8,
            red: 0xe46876,
            orange: 0xffa066,
            yellow: 0xe6c384,
            green: 0x98bb6c,
            purple: 0x957fb8,
        },
        Palette::SystemDark | Palette::SystemLight => return None,
    };

    Some(dark_colors(scheme))
}

fn dark_colors(scheme: DarkScheme) -> ThemeColor {
    let shell = color(scheme.shell);
    let surface = color(scheme.surface);
    let raised = color(scheme.raised);
    let hover = color(scheme.hover);
    let border = color(scheme.border);
    let foreground = color(scheme.foreground);
    let muted_foreground = color(scheme.muted_foreground);
    let accent = color(scheme.accent);
    let accent_hover = color(scheme.accent_hover);
    let accent_active = color(scheme.accent_active);
    let blue = color(scheme.blue);
    let red = color(scheme.red);
    let orange = color(scheme.orange);
    let yellow = color(scheme.yellow);
    let green = color(scheme.green);
    let purple = color(scheme.purple);

    ThemeColor {
        accent: raised,
        accent_foreground: foreground,
        accordion: surface,
        accordion_hover: hover,
        background: surface,
        border,
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
        bullish: green,
        bearish: red,
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
        table_hover: hover,
        table_row_border: raised,
        title_bar: shell,
        title_bar_border: border,
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

fn color(hex: u32) -> Hsla {
    rgb(hex).into()
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
}
