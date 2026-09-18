//! The preview pane for sound and video: cover or poster, the facts, and
//! for audio a player with a waveform to scrub and bars that move.
//!
//! The player lives in the preview and does its work on its own thread;
//! this file only reads its numbers. While something plays, a foreground
//! task asks for a repaint twenty times a second and stops a moment after
//! the sound does, so the bars can fall.
//!
//! The spectrum and the waveform are each one `canvas` painting a quad per
//! bar rather than a row of `div`s. Twenty times a second they would
//! otherwise be close to three hundred layout nodes rebuilt and measured
//! per frame; a quad is the cheapest thing the renderer draws and skips
//! layout altogether.

use std::{cell::Cell, rc::Rc, sync::Arc, time::Duration};

use gpui::prelude::*;
use gpui::{
    AnyElement, Bounds, Context, FontWeight, Hsla, MouseButton, MouseDownEvent, ObjectFit, Pixels,
    RenderImage, Window, canvas, div, fill, img, point, px, relative, size,
};
use gpui_component::{
    ActiveTheme as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
};

use crate::preview::{
    audio::AudioInfo,
    media::{MediaInfo, describe_channels, format_duration},
    player::{Player, Shared},
};

use super::Marcel;

const COVER_EDGE: f32 = 220.0;
const SPECTRUM_HEIGHT: f32 = 72.0;
const SPECTRUM_BAR_WIDTH: f32 = 4.0;
const SPECTRUM_BAR_GAP: f32 = 2.0;
const WAVEFORM_HEIGHT: f32 = 48.0;
const WAVEFORM_BAR_GAP: f32 = 1.0;
/// Bars drawn while the waveform has not been measured, so the scrubber
/// has a shape to press on.
const PLACEHOLDER_BARS: usize = 120;
const PLACEHOLDER_PEAK: f32 = 0.35;
const TRANSPORT_BUTTON_EDGE: f32 = 36.0;
const REPAINT_INTERVAL: Duration = Duration::from_millis(50);
/// Repaints kept going after the sound stops, so the bars settle.
const SETTLE_TICKS: u32 = 24;

impl Marcel {
    /// Keep the pane repainting while the player runs. The task ends on
    /// its own once nothing has played for a second, or when the preview
    /// moves on and the ticket no longer matches.
    pub(super) fn start_audio_repaints(&mut self, shared: Arc<Shared>, cx: &mut Context<Self>) {
        if self.preview.audio_repaints.is_some() {
            return;
        }
        let ticket = self.preview.ticket;
        self.preview.audio_repaints = Some(cx.spawn(async move |this, cx| {
            let mut quiet = 0;
            loop {
                cx.background_executor().timer(REPAINT_INTERVAL).await;
                let alive = this.update(cx, |this, cx| {
                    if this.preview.ticket != ticket {
                        return false;
                    }
                    cx.notify();
                    true
                });
                if !matches!(alive, Ok(true)) {
                    break;
                }
                if shared.playing() {
                    quiet = 0;
                } else {
                    quiet += 1;
                    if quiet > SETTLE_TICKS {
                        break;
                    }
                }
            }
            let _ = this.update(cx, |this, _| this.preview.audio_repaints = None);
        }));
    }

    fn toggle_playback(&mut self, player: &Player, cx: &mut Context<Self>) {
        player.toggle();
        self.start_audio_repaints(player.shared().clone(), cx);
        cx.notify();
    }

    fn seek_to_fraction(&mut self, player: &Player, fraction: f32, cx: &mut Context<Self>) {
        let Some(duration) = self.audio_duration() else {
            return;
        };
        player.seek(duration.mul_f32(fraction.clamp(0.0, 1.0)));
        self.start_audio_repaints(player.shared().clone(), cx);
        cx.notify();
    }

    fn audio_duration(&self) -> Option<Duration> {
        match &self.preview.state {
            crate::preview::PreviewState::Ready(crate::preview::Preview::Audio {
                info, ..
            }) => info.duration,
            _ => None,
        }
    }

    pub(super) fn render_audio_preview(
        &mut self,
        info: &AudioInfo,
        waveform: &Arc<[f32]>,
        cover: Option<&Arc<RenderImage>>,
        player: &Arc<Player>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors;
        let radius = cx.theme().radius;
        let shared = player.shared();
        let playing = shared.playing();
        let position = shared.position();
        let fraction = info
            .duration
            .filter(|duration| !duration.is_zero())
            .map(|duration| (position.as_secs_f32() / duration.as_secs_f32()).clamp(0.0, 1.0))
            .unwrap_or(0.0);

        let artwork = match cover {
            Some(cover) => div()
                .size(px(COVER_EDGE))
                .rounded(radius)
                .overflow_hidden()
                .child(
                    img(cover.clone())
                        .id(("audio-cover", self.preview.ticket))
                        .size_full()
                        .object_fit(ObjectFit::Cover),
                )
                .into_any_element(),
            None => div()
                .flex()
                .size(px(COVER_EDGE))
                .items_center()
                .justify_center()
                .rounded(radius)
                .bg(colors.list_hover)
                .font_family(cx.theme().mono_font_family.clone())
                .text_color(colors.muted_foreground)
                .child(div().text_3xl().child("♪"))
                .into_any_element(),
        };

        let title = info.title.clone().unwrap_or_else(|| {
            self.primary_entry().map(|entry| entry.name.clone()).unwrap_or_default()
        });
        let by_line = match (&info.artist, &info.album) {
            (Some(artist), Some(album)) => Some(format!("{artist} · {album}")),
            (Some(artist), None) => Some(artist.clone()),
            (None, Some(album)) => Some(album.clone()),
            (None, None) => None,
        };

        // The bars: one quad each, the height the band's level.
        let spectrum = shared.spectrum();
        let (lit, unlit) = (colors.primary, colors.border);
        let bars = canvas(
            |_, _, _| {},
            move |bounds, (), window, _| paint_spectrum(window, bounds, &spectrum, lit, unlit),
        )
        .w_full()
        .h(px(SPECTRUM_HEIGHT));

        // The waveform doubles as the scrubber: a press anywhere on it seeks
        // to that fraction of the file. Its painted bounds are what turn
        // the pointer into a fraction. gpui-component's `Slider` draws a
        // track and a thumb over its own state entity, and what is wanted
        // here is a slider whose track is the waveform, so it is painted.
        let scrub_bounds = self.preview.scrub_bounds.clone();
        let peaks = waveform.clone();
        let player_for_seek = player.clone();
        let waveform_canvas = canvas(
            {
                let scrub_bounds = scrub_bounds.clone();
                move |bounds, _, _| scrub_bounds.set(Some(bounds))
            },
            move |bounds, (), window, _| {
                paint_waveform(window, bounds, &peaks, fraction, lit, unlit)
            },
        )
        .size_full();
        let waveform_row = div()
            .id("audio-waveform")
            .w_full()
            .h(px(WAVEFORM_HEIGHT))
            .cursor_pointer()
            .child(waveform_canvas)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    if let Some(bounds) = scrub_bounds.get() {
                        let width = f32::from(bounds.size.width).max(1.0);
                        let fraction = f32::from(event.position.x - bounds.origin.x) / width;
                        this.seek_to_fraction(&player_for_seek, fraction, cx);
                    }
                    cx.stop_propagation();
                }),
            );

        let player_for_toggle = player.clone();
        let button = Button::new("audio-toggle")
            .primary()
            .with_size(px(TRANSPORT_BUTTON_EDGE))
            .rounded(px(TRANSPORT_BUTTON_EDGE / 2.0))
            .font_family(cx.theme().mono_font_family.clone())
            .line_height(relative(1.0))
            .child(if playing { "▮▮" } else { "▶" })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_playback(&player_for_toggle, cx);
            }));
        let clock = format!(
            "{} / {}",
            format_duration(position),
            info.duration.map(format_duration).unwrap_or_else(|| "–:––".to_string())
        );
        let transport = h_flex()
            .w_full()
            .items_center()
            .gap_3()
            .child(button)
            .child(div().text_sm().text_color(colors.muted_foreground).child(clock))
            .child(div().flex_1())
            .child(div().text_xs().text_color(colors.muted_foreground).child(stream_line(info)));

        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .p_4()
            .gap_3()
            .child(artwork)
            .child(
                v_flex()
                    .w_full()
                    .items_center()
                    .gap_1()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_center()
                            .whitespace_normal()
                            .child(title),
                    )
                    .when_some(by_line, |this, line| {
                        this.child(
                            div()
                                .text_sm()
                                .text_color(colors.muted_foreground)
                                .text_center()
                                .whitespace_normal()
                                .child(line),
                        )
                    }),
            )
            .child(bars)
            .child(waveform_row)
            .child(transport)
            .when_some(shared.error(), |this, error| {
                this.child(div().text_xs().text_color(colors.danger).child(error))
            })
            .into_any_element()
    }

    pub(super) fn render_video_preview(
        &mut self,
        info: &MediaInfo,
        poster: &Result<Arc<RenderImage>, String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors;
        // Marcel does not play video; the poster is a button that hands the
        // file to whatever does, the same way Enter would.
        let entry = self.primary_entry().cloned();
        let play = div()
            .id("video-play")
            .absolute()
            .flex()
            .size(px(56.0))
            .items_center()
            .justify_center()
            .rounded_full()
            .bg(colors.background.opacity(0.8))
            .text_color(colors.foreground)
            .text_xl()
            .font_family(cx.theme().mono_font_family.clone())
            .line_height(relative(1.0))
            .cursor_pointer()
            .hover(|this| this.bg(colors.primary).text_color(colors.button_primary_foreground))
            .child("▶")
            .on_click(cx.listener(move |this, _, window, cx| {
                if let Some(entry) = entry.clone() {
                    this.open_entry(entry, window, cx);
                }
            }));
        let frame = match poster {
            Ok(poster) => div()
                .relative()
                .flex_1()
                .min_h_0()
                .w_full()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    img(poster.clone())
                        .id(("video-poster", self.preview.ticket))
                        .max_w_full()
                        .max_h_full()
                        .object_fit(ObjectFit::Contain),
                )
                .child(play)
                .into_any_element(),
            Err(error) => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(colors.muted_foreground)
                .whitespace_normal()
                .child(format!("No frame could be taken from this video\n{error}"))
                .into_any_element(),
        };
        let mut facts = Vec::new();
        if let Some(duration) = info.duration {
            facts.push(format_duration(duration));
        }
        if let (Some(width), Some(height)) = (info.width, info.height) {
            facts.push(format!("{width}×{height}"));
        }
        let codecs = [info.codec.as_deref(), info.audio_codec.as_deref()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(", ");
        if !codecs.is_empty() {
            facts.push(codecs);
        }
        v_flex()
            .size_full()
            .p_3()
            .gap_2()
            .child(frame)
            .child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(colors.muted_foreground)
                    .text_center()
                    .child(facts.join(" · ")),
            )
            .into_any_element()
    }
}

/// "flac · 44100 Hz · stereo", whichever parts are known.
fn stream_line(info: &AudioInfo) -> String {
    let mut parts = vec![info.codec.clone()];
    if info.sample_rate > 0 {
        parts.push(format!("{} Hz", info.sample_rate));
    }
    if info.channels > 0 {
        parts.push(describe_channels(info.channels));
    }
    parts.join(" · ")
}

/// The spectrum's bars, standing on the bottom edge and centred across
/// `bounds`; a bar that is all but silent takes the quiet colour.
fn paint_spectrum(
    window: &mut Window,
    bounds: Bounds<Pixels>,
    levels: &[f32],
    lit: Hsla,
    unlit: Hsla,
) {
    if levels.is_empty() {
        return;
    }
    let span =
        levels.len() as f32 * SPECTRUM_BAR_WIDTH + (levels.len() - 1) as f32 * SPECTRUM_BAR_GAP;
    let left = f32::from(bounds.origin.x) + (f32::from(bounds.size.width) - span) / 2.0;
    let bottom = f32::from(bounds.origin.y) + f32::from(bounds.size.height);
    let range = f32::from(bounds.size.height) - 2.0;
    for (index, level) in levels.iter().enumerate() {
        let height = range * level + 2.0;
        let x = left + index as f32 * (SPECTRUM_BAR_WIDTH + SPECTRUM_BAR_GAP);
        let bar = Bounds::new(
            point(px(x), px(bottom - height)),
            size(px(SPECTRUM_BAR_WIDTH), px(height)),
        );
        let color = if *level > 0.02 { lit } else { unlit };
        window.paint_quad(fill(bar, color).corner_radii(px(1.0)));
    }
}

/// The waveform: a bar per peak sharing the width, centred on the middle
/// line, with the part before the playhead in `played`. No peaks yet means
/// an even row of placeholder bars, so the scrubber still has a shape.
fn paint_waveform(
    window: &mut Window,
    bounds: Bounds<Pixels>,
    peaks: &[f32],
    fraction: f32,
    played: Hsla,
    rest: Hsla,
) {
    let placeholder = [PLACEHOLDER_PEAK; PLACEHOLDER_BARS];
    let peaks = if peaks.is_empty() { &placeholder[..] } else { peaks };
    let stride = f32::from(bounds.size.width) / peaks.len() as f32;
    let bar_width = (stride - WAVEFORM_BAR_GAP).max(1.0);
    let left = f32::from(bounds.origin.x);
    let middle = f32::from(bounds.origin.y) + f32::from(bounds.size.height) / 2.0;
    let range = f32::from(bounds.size.height) - 4.0;
    let played_bars = (fraction * peaks.len() as f32).round() as usize;
    for (index, peak) in peaks.iter().enumerate() {
        let height = range * peak.max(0.04);
        let bar = Bounds::new(
            point(px(left + index as f32 * stride), px(middle - height / 2.0)),
            size(px(bar_width), px(height)),
        );
        let color = if index < played_bars { played } else { rest };
        window.paint_quad(fill(bar, color).corner_radii(px(1.0)));
    }
}

/// Where the scrubber was last painted, for turning a press into a seek.
pub(super) type ScrubBounds = Rc<Cell<Option<Bounds<Pixels>>>>;
