use gpui::{App, Hsla, rgb};
use gpui_component::{Colorize, Theme, ThemeColor, ThemeMode, scroll::ScrollbarShow};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Palette {
    #[default]
    Nord,
    Dark,
    Light,
}

impl Palette {
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "nord" | "nord-dark" => Some(Self::Nord),
            "dark" | "default-dark" => Some(Self::Dark),
            "light" | "default-light" => Some(Self::Light),
            _ => None,
        }
    }

    fn from_environment() -> Self {
        std::env::var("MARCEL_THEME")
            .ok()
            .as_deref()
            .and_then(Self::from_name)
            .unwrap_or_default()
    }
}

pub fn init(cx: &mut App) {
    apply(Palette::from_environment(), cx);
}

pub fn apply(palette: Palette, cx: &mut App) {
    let mode = match palette {
        Palette::Nord | Palette::Dark => ThemeMode::Dark,
        Palette::Light => ThemeMode::Light,
    };
    Theme::change(mode, None, cx);
    Theme::global_mut(cx).scrollbar_show = ScrollbarShow::Always;

    if palette == Palette::Nord {
        Theme::global_mut(cx).colors = nord();
    }
}

fn nord() -> ThemeColor {
    let nord0 = color(0x2e3440);
    let nord1 = color(0x3b4252);
    let nord2 = color(0x434c5e);
    let nord3 = color(0x4c566a);
    let nord4 = color(0xd8dee9);
    let nord5 = color(0xe5e9f0);
    let nord6 = color(0xeceff4);
    let frost0 = color(0x8fbcbb);
    let frost1 = color(0x88c0d0);
    let frost2 = color(0x81a1c1);
    let frost3 = color(0x5e81ac);
    let red = color(0xbf616a);
    let orange = color(0xd08770);
    let yellow = color(0xebcb8b);
    let green = color(0xa3be8c);
    let purple = color(0xb48ead);

    ThemeColor {
        accent: nord2,
        accent_foreground: nord6,
        accordion: nord1,
        accordion_hover: nord2,
        background: nord0,
        border: nord3,
        group_box: nord1,
        group_box_foreground: nord4,
        caret: frost1,
        chart_1: frost1,
        chart_2: green,
        chart_3: yellow,
        chart_4: purple,
        chart_5: red,
        danger: red,
        danger_active: red.darken(0.12),
        danger_foreground: nord6,
        danger_hover: red.lighten(0.08),
        description_list_label: nord1,
        description_list_label_foreground: nord4,
        drag_border: frost1,
        drop_target: frost1.opacity(0.2),
        foreground: nord4,
        info: frost2,
        info_active: frost3,
        info_foreground: nord6,
        info_hover: frost1,
        input: nord3,
        link: frost1,
        link_active: frost3,
        link_hover: frost0,
        list: nord0,
        list_active: frost1.opacity(0.18),
        list_active_border: frost1,
        list_even: nord0,
        list_head: nord1,
        list_hover: nord1,
        muted: nord1,
        muted_foreground: color(0xaab2c0),
        popover: nord1,
        popover_foreground: nord4,
        primary: frost1,
        primary_active: frost3,
        primary_foreground: nord0,
        primary_hover: frost0,
        progress_bar: frost1,
        ring: frost1,
        scrollbar: nord0,
        scrollbar_thumb: nord3,
        scrollbar_thumb_hover: frost3,
        secondary: nord2,
        secondary_active: nord3,
        secondary_foreground: nord5,
        secondary_hover: nord3,
        selection: frost1.opacity(0.26),
        sidebar: nord1,
        sidebar_accent: nord2,
        sidebar_accent_foreground: nord6,
        sidebar_border: nord3,
        sidebar_foreground: nord4,
        sidebar_primary: frost1,
        sidebar_primary_foreground: nord0,
        skeleton: nord2,
        slider_bar: nord3,
        slider_thumb: frost1,
        success: green,
        success_foreground: nord0,
        success_hover: green.lighten(0.08),
        success_active: green.darken(0.12),
        bullish: green,
        bearish: red,
        switch: nord3,
        switch_thumb: nord5,
        tab: nord1,
        tab_active: nord2,
        tab_active_foreground: nord6,
        tab_bar: nord1,
        tab_bar_segmented: nord2,
        tab_foreground: nord4,
        table: nord0,
        table_active: frost1.opacity(0.18),
        table_active_border: frost1,
        table_even: nord0,
        table_head: nord1,
        table_head_foreground: nord5,
        table_hover: nord1,
        table_row_border: nord2,
        title_bar: nord1,
        title_bar_border: nord3,
        tiles: nord0,
        warning: yellow,
        warning_active: orange,
        warning_hover: yellow.lighten(0.08),
        warning_foreground: nord0,
        overlay: nord0.opacity(0.72),
        window_border: nord3,
        red,
        red_light: red.lighten(0.15),
        green,
        green_light: green.lighten(0.15),
        blue: frost3,
        blue_light: frost2,
        yellow,
        yellow_light: yellow.lighten(0.15),
        magenta: purple,
        magenta_light: purple.lighten(0.15),
        cyan: frost0,
        cyan_light: frost1,
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
        assert_eq!(Palette::from_name("DEFAULT-DARK"), Some(Palette::Dark));
        assert_eq!(Palette::from_name("light"), Some(Palette::Light));
        assert_eq!(Palette::from_name("unknown"), None);
    }
}
