//! Rainbow activity indicators: Rainbow Classic Compact, Sonic Speed Up and
//! Rainbow Starlight.
//!
//! Every renderer draws from the active theme's loader spectrum
//! ([`theme::loader_spectrum`]) and fades toward the surface it paints over,
//! read from the buffer itself, so an indicator blends into whatever canvas,
//! custom text-area colour or transparency policy is in force. A surface with
//! no resolvable RGB (a terminal-default background) is never painted for
//! ambience: fades resolve toward the theme canvas and empty cells keep the
//! terminal's background.
//!
//! Each renderer is a pure function of its area and an instant in epoch
//! milliseconds, so tests can pin exact frames.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;

use super::theme::{self, LoaderSpectrum};

type Rgb = (u8, u8, u8);

const WHITE: Rgb = (255, 255, 255);

/// Upper half block: foreground paints the top half, background the bottom.
const UPPER_HALF: &str = "\u{2580}";
/// Lower half block.
const LOWER_HALF: &str = "\u{2584}";

/// Shared animation clock: milliseconds since the epoch.
fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn seconds(now_ms: i64) -> f64 {
    now_ms as f64 / 1_000.0
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// What a renderer paints over at one cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Surface {
    /// Colour that fades resolve toward.
    rgb: Rgb,
    /// Whether ambience (haze, empty cells) may paint the background.
    paintable: bool,
}

fn surface_at(buf: &Buffer, x: u16, y: u16, fallback: Rgb) -> Surface {
    theme::resolve_rgb(buf[(x, y)].bg).map_or(
        Surface {
            rgb: fallback,
            paintable: false,
        },
        |rgb| Surface {
            rgb,
            paintable: true,
        },
    )
}

/// Fade target for a surface the terminal owns: the theme canvas.
fn fallback_surface() -> Rgb {
    theme::resolve_rgb(theme::base()).unwrap_or((0, 0, 0))
}

/// Highlight for hot spots (streak heads, star flares): the theme's text
/// colour on a dark surface. A light surface gets none, because a pale
/// highlight would vanish into it.
fn glow_for(surface: Rgb) -> Option<Rgb> {
    (luma(surface) < 0.5).then(|| theme::resolve_rgb(theme::text()).unwrap_or(WHITE))
}

fn luma((r, g, b): Rgb) -> f32 {
    (0.2126 * f32::from(r) + 0.7152 * f32::from(g) + 0.0722 * f32::from(b)) / 255.0
}

/// Channel-wise blend `from` → `to` at `amount` (clamped to `0.0..=1.0`).
fn mix(from: Rgb, to: Rgb, amount: f32) -> Rgb {
    let amount = amount.clamp(0.0, 1.0);
    let channel =
        |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * amount).round() as u8;
    (
        channel(from.0, to.0),
        channel(from.1, to.1),
        channel(from.2, to.2),
    )
}

const fn rgb_color((r, g, b): Rgb) -> Color {
    Color::Rgb(r, g, b)
}

/// Overwrite one cell. `bg: None` keeps the background already there.
fn put(buf: &mut Buffer, x: u16, y: u16, symbol: &str, fg: Option<Rgb>, bg: Option<Rgb>) {
    let cell = &mut buf[(x, y)];
    let kept_bg = cell.bg;
    cell.reset();
    cell.set_symbol(symbol);
    cell.set_bg(bg.map_or(kept_bg, rgb_color));
    if let Some(fg) = fg {
        cell.set_fg(rgb_color(fg));
    }
}

/// Avalanche hash (lowbias32) for deterministic per-cell randomness.
const fn hash32(seed: u32) -> u32 {
    let mut x = seed;
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb_352d);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846c_a68b);
    x ^= x >> 16;
    x
}

/// Deterministic value in `0.0..=1.0` for `seed`.
fn unit(seed: u32) -> f64 {
    f64::from(hash32(seed)) / f64::from(u32::MAX)
}

fn smoothstep(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Fractional part of `value` as a spectrum position.
fn turns(value: f64) -> f32 {
    value.rem_euclid(1.0) as f32
}

/// Sample the spectrum at an `f64` position without losing precision.
fn sample(spectrum: &LoaderSpectrum, position: f64) -> Rgb {
    spectrum.sample_rgb(turns(position))
}

// ---------------------------------------------------------------------------
// Rainbow Classic Compact: an interlaced ribbon with a passing glint
// ---------------------------------------------------------------------------

/// Columns per repeat of the ribbon gradient: the classic strip's period.
const RIBBON_PERIOD_COLUMNS: f64 = theme::RAINBOW_PALETTE_LEN as f64;
/// Milliseconds per column of travel: the classic strip's pace.
const RIBBON_MS_PER_COLUMN: f64 = 60.0;
/// Travel wraps on whole ribbon periods, keeping the phase small and exact.
const RIBBON_WRAP_MS: i64 = 60 * 14 * 1_000;
/// A glint sweeps across the ribbon once per period.
const GLINT_PERIOD_MS: i64 = 3_600;
/// How long one sweep takes.
const GLINT_SWEEP_MS: i64 = 1_100;
/// Columns the glint starts and ends beyond the ribbon's edges.
const GLINT_MARGIN: f64 = 6.0;
/// Gaussian radius of the glint, in columns.
const GLINT_RADIUS: f64 = 2.8;
/// Peak blend toward white at the glint's centre.
const GLINT_STRENGTH: f64 = 0.6;

/// Rainbow Classic Compact: the classic strip's interlaced, complementary
/// half-block ribbon in one row, flowing smoothly at the classic pace, with a
/// slanted glint sweeping across it every few seconds.
pub fn render_classic_compact(frame: &mut Frame, area: Rect) {
    render_classic_compact_at(frame.buffer_mut(), area, now_ms());
}

pub(crate) fn render_classic_compact_at(buf: &mut Buffer, area: Rect, now_ms: i64) {
    let area = area.intersection(buf.area);
    if area.is_empty() {
        return;
    }
    let spectrum = theme::loader_spectrum();
    let travel = now_ms.rem_euclid(RIBBON_WRAP_MS) as f64 / RIBBON_MS_PER_COLUMN;
    let glint = glint_center(now_ms, area.width);
    for row in 0..area.height {
        let slant = 2.0 * f64::from(row);
        for column in 0..area.width {
            let position = (f64::from(column) + slant + travel) / RIBBON_PERIOD_COLUMNS;
            let mut top = sample(&spectrum, position);
            let mut bottom = sample(&spectrum, position + 0.5);
            if let Some(center) = glint {
                let x = f64::from(column) + 0.5;
                top = mix(top, WHITE, glint_strength(x, center - slant));
                bottom = mix(bottom, WHITE, glint_strength(x, center - slant - 1.0));
            }
            put(
                buf,
                area.x + column,
                area.y + row,
                UPPER_HALF,
                Some(top),
                Some(bottom),
            );
        }
    }
}

/// Column the glint is centred on while a sweep is in progress.
fn glint_center(now_ms: i64, width: u16) -> Option<f64> {
    let elapsed = now_ms.rem_euclid(GLINT_PERIOD_MS);
    (elapsed < GLINT_SWEEP_MS).then(|| {
        let progress = smoothstep(elapsed as f64 / GLINT_SWEEP_MS as f64);
        -GLINT_MARGIN + (f64::from(width) + 2.0 * GLINT_MARGIN) * progress
    })
}

/// Blend toward white at column centre `x` for a glint centred on `center`.
fn glint_strength(x: f64, center: f64) -> f32 {
    let distance = (x - center) / GLINT_RADIUS;
    (GLINT_STRENGTH * (-distance * distance).exp()) as f32
}

// ---------------------------------------------------------------------------
// Sonic Speed Up: parallax rainbow speed streaks with boost surges
// ---------------------------------------------------------------------------

/// Seconds per boost surge.
const BOOST_PERIOD_SECS: f64 = 2.6;
/// Extra speed at the peak of a surge (peak speed is `1 + BOOST_GAIN`).
const BOOST_GAIN: f64 = 1.6;
/// Cruise speed in columns per second of the far (top) and near (bottom)
/// half-row lanes; the near lane runs faster for parallax.
const LANE_SPEEDS: [f64; 2] = [34.0, 52.0];
/// Columns a head starts left of the row, so streaks enter from off-screen.
const STREAK_ENTRY: f64 = 2.0;
/// Longest possible tail; the track leaves room for a tail to fully exit
/// before its streak re-enters.
const STREAK_TAIL_MAX: f64 = 30.0;
/// Roughly one streak per this many columns in each lane.
const STREAK_SPACING: u16 = 28;
/// Spectrum span a tail sweeps from head to tip, for a rainbow trail.
const STREAK_HUE_SPAN: f64 = 0.45;
/// Columns behind the head that run hot.
const STREAK_HOT_COLUMNS: f64 = 1.6;
/// Intensity below which a lane pixel is left dark.
const VISIBLE_FLOOR: f32 = 0.04;
/// The road dust scrolls left at this fraction of the far lane's speed.
const DUST_SPEED: f64 = 0.38;
/// One column in this many carries a dust speck.
const DUST_ODDS: u32 = 11;
/// Spectrum drift per second, so the whole palette slowly rotates.
const HUE_DRIFT_PER_SEC: f64 = 0.05;

/// Surge level in `0.0..=1.0`: mostly cruising, briefly peaking mid-period.
fn boost_level(seconds: f64) -> f64 {
    (std::f64::consts::PI * seconds / BOOST_PERIOD_SECS)
        .sin()
        .powi(4)
}

/// Distance travelled at unit cruise speed: the closed-form integral of
/// `1 + BOOST_GAIN * boost_level`, so streak positions stay continuous as the
/// speed surges.
fn boost_distance(seconds: f64) -> f64 {
    let x = std::f64::consts::PI * seconds / BOOST_PERIOD_SECS;
    let surge = 3.0 * x / 8.0 - (2.0 * x).sin() / 4.0 + (4.0 * x).sin() / 32.0;
    seconds + BOOST_GAIN * (BOOST_PERIOD_SECS / std::f64::consts::PI) * surge
}

/// One speed streak in a lane at an instant.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Streak {
    /// Column position of the head (fractional; may sit off-screen).
    head: f64,
    /// Tail length in columns; grows with speed like motion blur.
    length: f64,
    /// Spectrum position of the head colour.
    hue: f64,
}

/// Streaks of `lane` (half-rows counted from the top) at an instant.
fn lane_streaks(lane: u16, width: u16, seconds: f64) -> Vec<Streak> {
    let count = (width.saturating_add(24) / STREAK_SPACING).max(1);
    let track = f64::from(width) + STREAK_ENTRY + STREAK_TAIL_MAX;
    let lane_speed = LANE_SPEEDS[usize::from(lane % 2)] * (1.0 + 0.12 * f64::from(lane / 2));
    let distance = boost_distance(seconds);
    let speed = 1.0 + BOOST_GAIN * boost_level(seconds);
    let drift = (seconds * HUE_DRIFT_PER_SEC).rem_euclid(1.0);
    (0..count)
        .map(|index| {
            let seed = u32::from(lane) * 977 + u32::from(index) * 131 + 7;
            let offset = (f64::from(index) + unit(seed) * 0.6) / f64::from(count) * track;
            let pace = 0.88 + 0.3 * unit(seed + 1);
            let base_length = 6.0 + 7.0 * unit(seed + 2);
            Streak {
                head: (offset + lane_speed * pace * distance).rem_euclid(track) - STREAK_ENTRY,
                length: base_length * (0.7 + 0.45 * speed),
                hue: unit(seed + 3) + drift,
            }
        })
        .collect()
}

/// Brightness of a streak at `behind` columns behind its head: an
/// anti-aliased leading edge across the head cell, then a quadratic fade.
fn streak_intensity(behind: f64, length: f64) -> f32 {
    if behind < -0.5 {
        0.0
    } else if behind < 0.5 {
        (behind + 0.5) as f32
    } else {
        let tail = (behind - 0.5) / length;
        if tail >= 1.0 {
            0.0
        } else {
            ((1.0 - tail) * (1.0 - tail)) as f32
        }
    }
}

/// Sonic Speed Up: rainbow comets race right through two parallax half-row
/// lanes, each trailing a fading rainbow tail behind a white-hot head. Every
/// couple of seconds the field surges (boost) and the tails stretch like
/// motion blur, while road dust scrolls the other way.
pub fn render_sonic_speed_up(frame: &mut Frame, area: Rect) {
    render_sonic_speed_up_at(frame.buffer_mut(), area, now_ms());
}

pub(crate) fn render_sonic_speed_up_at(buf: &mut Buffer, area: Rect, now_ms: i64) {
    let area = area.intersection(buf.area);
    if area.is_empty() {
        return;
    }
    let spectrum = theme::loader_spectrum();
    let fallback = fallback_surface();
    let t = seconds(now_ms);
    let surge = boost_level(t);
    let drift = (t * HUE_DRIFT_PER_SEC).rem_euclid(1.0);
    let dust_travel = DUST_SPEED * LANE_SPEEDS[0] * boost_distance(t);
    for row in 0..area.height {
        let top_lane = lane_streaks(2 * row, area.width, t);
        let bottom_lane = lane_streaks(2 * row + 1, area.width, t);
        for column in 0..area.width {
            let (x, y) = (area.x + column, area.y + row);
            let surface = surface_at(buf, x, y, fallback);
            let glow = glow_for(surface.rgb);
            let center = f64::from(column) + 0.5;
            let top = lane_pixel(&top_lane, center, &spectrum, glow, surge, surface.rgb);
            let bottom = lane_pixel(&bottom_lane, center, &spectrum, glow, surge, surface.rgb);
            match (top, bottom) {
                (Some(top), Some(bottom)) => put(buf, x, y, UPPER_HALF, Some(top), Some(bottom)),
                (Some(top), None) => put(buf, x, y, UPPER_HALF, Some(top), None),
                (None, Some(bottom)) => put(buf, x, y, LOWER_HALF, Some(bottom), None),
                (None, None) => {
                    let speck = (center + dust_travel).floor() as i64;
                    let seed = (speck as u32)
                        .wrapping_mul(7_919)
                        .wrapping_add(u32::from(row));
                    if hash32(seed) % DUST_ODDS == 0 {
                        let color = sample(&spectrum, unit(speck as u32) + drift);
                        put(buf, x, y, "·", Some(mix(surface.rgb, color, 0.4)), None);
                    } else {
                        put(buf, x, y, " ", None, None);
                    }
                }
            }
        }
    }
}

/// Colour of one lane pixel at column centre `center`, or `None` when no
/// streak lights it.
fn lane_pixel(
    streaks: &[Streak],
    center: f64,
    spectrum: &LoaderSpectrum,
    glow: Option<Rgb>,
    surge: f64,
    surface: Rgb,
) -> Option<Rgb> {
    let (intensity, streak, behind) = streaks
        .iter()
        .map(|streak| {
            let behind = streak.head - center;
            (streak_intensity(behind, streak.length), streak, behind)
        })
        .max_by(|a, b| a.0.total_cmp(&b.0))?;
    if intensity < VISIBLE_FLOOR {
        return None;
    }
    let along = (behind.max(0.0) / streak.length).min(1.0);
    let mut color = sample(spectrum, streak.hue + STREAK_HUE_SPAN * along);
    if let Some(glow) = glow
        && behind < STREAK_HOT_COLUMNS
    {
        let heat = (1.0 - behind.max(0.0) / STREAK_HOT_COLUMNS) * (0.45 + 0.35 * surge);
        color = mix(color, glow, heat as f32);
    }
    Some(mix(surface, color, intensity))
}

// ---------------------------------------------------------------------------
// Rainbow Starlight: twinkling stars over a drifting nebula
// ---------------------------------------------------------------------------

/// Fraction of cells that host a star.
const STAR_DENSITY: f64 = 0.34;
/// Shortest twinkle period, in seconds.
const STAR_PERIOD_MIN_SECS: f64 = 1.5;
/// Spread of twinkle periods above the minimum, in seconds.
const STAR_PERIOD_SPREAD_SECS: f64 = 2.3;
/// Resting brightness of a star between flares.
const STAR_REST: f64 = 0.22;
/// Brightness from which a star flares to an open star.
const STAR_OPEN: f64 = 0.46;
/// Brightness from which a star flares to a full star.
const STAR_FULL: f64 = 0.8;
/// Spectrum drift per second for star colours.
const STAR_HUE_DRIFT_PER_SEC: f64 = 0.03;
/// Least nebula tint over the surface.
const HAZE_FLOOR: f32 = 0.05;
/// Extra nebula tint where the clouds gather.
const HAZE_SWELL: f32 = 0.10;
/// A shooting star crosses once per period...
const METEOR_PERIOD_MS: i64 = 5_200;
/// ...taking this long to cross.
const METEOR_FLIGHT_MS: i64 = 1_000;
/// Tail length of a shooting star, in columns.
const METEOR_TAIL: f64 = 11.0;

/// Twinkle brightness of a star in `STAR_REST..=1.0`: a long rest with one
/// brief flare per period.
fn twinkle(seed: u32, seconds: f64) -> f64 {
    let period = STAR_PERIOD_MIN_SECS + STAR_PERIOD_SPREAD_SECS * unit(seed + 1);
    let phase = (seconds / period + unit(seed + 2)).rem_euclid(1.0);
    STAR_REST + (1.0 - STAR_REST) * (std::f64::consts::PI * phase).sin().powi(6)
}

const fn star_glyph(brightness: f64) -> &'static str {
    if brightness < STAR_OPEN {
        "·"
    } else if brightness < STAR_FULL {
        "✧"
    } else {
        "✦"
    }
}

/// Rainbow Starlight: a calm field of rainbow stars that rest as dots and
/// flare into stars on their own rhythms, over a softly drifting nebula of
/// theme colours, crossed by a shooting star every few seconds.
pub fn render_starlight(frame: &mut Frame, area: Rect) {
    render_starlight_at(frame.buffer_mut(), area, now_ms());
}

pub(crate) fn render_starlight_at(buf: &mut Buffer, area: Rect, now_ms: i64) {
    let area = area.intersection(buf.area);
    if area.is_empty() {
        return;
    }
    let spectrum = theme::loader_spectrum();
    let fallback = fallback_surface();
    let t = seconds(now_ms);
    let haze_drift = t * 0.025;
    let star_drift = t * STAR_HUE_DRIFT_PER_SEC;
    for row in 0..area.height {
        for column in 0..area.width {
            let (x, y) = (area.x + column, area.y + row);
            let surface = surface_at(buf, x, y, fallback);
            let col = f64::from(column);
            let cloud = 0.5
                + 0.5
                    * (col * 0.21 + t * 0.55 + f64::from(row) * 1.3).sin()
                    * (col * 0.083 - t * 0.31).sin();
            let haze_color = sample(
                &spectrum,
                col / f64::from(area.width.max(1)) * 0.6 + haze_drift,
            );
            let sky = mix(
                surface.rgb,
                haze_color,
                HAZE_FLOOR + HAZE_SWELL * cloud as f32,
            );
            let painted_sky = surface.paintable.then_some(sky);
            let seed = u32::from(column)
                .wrapping_mul(7_349)
                .wrapping_add(u32::from(row).wrapping_mul(3_163))
                .wrapping_add(11);
            if unit(seed) < STAR_DENSITY {
                let brightness = twinkle(seed, t);
                let mut color = mix(
                    sky,
                    sample(&spectrum, unit(seed + 3) + star_drift),
                    (0.3 + 0.7 * brightness) as f32,
                );
                if let Some(glow) = glow_for(surface.rgb)
                    && brightness > STAR_FULL
                {
                    let flare = (brightness - STAR_FULL) / (1.0 - STAR_FULL) * 0.55;
                    color = mix(color, glow, flare as f32);
                }
                put(buf, x, y, star_glyph(brightness), Some(color), painted_sky);
            } else {
                put(buf, x, y, " ", None, painted_sky);
            }
        }
    }
    draw_meteor(buf, area, now_ms, &spectrum, fallback);
}

/// Overlay this period's shooting star, if one is in flight.
fn draw_meteor(
    buf: &mut Buffer,
    area: Rect,
    now_ms: i64,
    spectrum: &LoaderSpectrum,
    fallback: Rgb,
) {
    let elapsed = now_ms.rem_euclid(METEOR_PERIOD_MS);
    if elapsed >= METEOR_FLIGHT_MS {
        return;
    }
    let cycle = now_ms.div_euclid(METEOR_PERIOD_MS) as u32;
    let row = (hash32(cycle) % u32::from(area.height)) as u16;
    let leftward = hash32(cycle.wrapping_add(99)) & 1 == 1;
    let progress = elapsed as f64 / METEOR_FLIGHT_MS as f64;
    let eased = 0.65 * progress + 0.35 * smoothstep(progress);
    let head = -2.0 + (f64::from(area.width) + METEOR_TAIL + 4.0) * eased;
    let hue = unit(cycle.wrapping_mul(31).wrapping_add(5));
    let y = area.y + row;
    for column in 0..area.width {
        // Measure along the flight direction so a leftward meteor mirrors.
        let lane_column = if leftward {
            area.width - 1 - column
        } else {
            column
        };
        let behind = head - (f64::from(lane_column) + 0.5);
        if behind < -0.5 {
            continue;
        }
        let (glyph, intensity) = if behind < 0.5 {
            ("✦", behind + 0.5)
        } else {
            let tail = (behind - 0.5) / METEOR_TAIL;
            if tail >= 1.0 {
                continue;
            }
            (if tail < 0.25 { "━" } else { "─" }, (1.0 - tail).powf(1.6))
        };
        let x = area.x + column;
        let under = theme::resolve_rgb(buf[(x, y)].bg).unwrap_or(fallback);
        let mut color = sample(
            spectrum,
            hue + 0.5 * (behind.max(0.0) / METEOR_TAIL).min(1.0),
        );
        if let Some(glow) = glow_for(under)
            && behind < 1.5
        {
            color = mix(color, glow, (0.7 * (1.0 - behind.max(0.0) / 1.5)) as f32);
        }
        put(
            buf,
            x,
            y,
            glyph,
            Some(mix(under, color, intensity as f32)),
            None,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An instant clear of every sweep and flight window (it sits 2.4 s into
    /// a glint period and 1.2 s into a meteor period; both windows have closed).
    const QUIET_MS: i64 = 1_700_000_000_000 + 2_400 - (1_700_000_000_000 % 3_600);

    fn surface_buffer(width: u16, height: u16, surface: Color) -> Buffer {
        let mut buf = Buffer::empty(Rect::new(0, 0, width, height));
        for cell in &mut buf.content {
            cell.set_bg(surface);
        }
        buf
    }

    fn symbols(buf: &Buffer) -> Vec<String> {
        buf.content
            .iter()
            .map(|cell| cell.symbol().to_string())
            .collect()
    }

    /// True when `color` lies on (or within rounding of) `spectrum`.
    fn on_spectrum(spectrum: &LoaderSpectrum, color: Color) -> bool {
        (0..1_024).any(|step| {
            theme::perceptual_distance(color, spectrum.sample(step as f32 / 1_024.0))
                .is_some_and(|distance| distance < 0.012)
        })
    }

    fn first_instant_where(start: i64, predicate: impl Fn(i64) -> bool) -> i64 {
        (start..start + 20_000)
            .step_by(10)
            .find(|&instant| predicate(instant))
            .expect("instant within twenty seconds")
    }

    #[test]
    fn quiet_instant_has_no_glint_or_meteor() {
        assert!(glint_center(QUIET_MS, 80).is_none());
        assert!(QUIET_MS.rem_euclid(METEOR_PERIOD_MS) >= METEOR_FLIGHT_MS);
    }

    #[test]
    fn every_loader_survives_degenerate_areas() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 8, 2));
        for area in [
            Rect::new(0, 0, 0, 0),
            Rect::new(0, 0, 1, 0),
            Rect::new(0, 0, 0, 1),
            Rect::new(0, 0, 1, 1),
            Rect::new(6, 1, 10, 4),
            Rect::new(20, 20, 4, 1),
        ] {
            render_classic_compact_at(&mut buf, area, QUIET_MS);
            render_sonic_speed_up_at(&mut buf, area, QUIET_MS);
            render_starlight_at(&mut buf, area, QUIET_MS);
            render_starlight_at(&mut buf, area, QUIET_MS - QUIET_MS % METEOR_PERIOD_MS + 400);
        }
    }

    #[test]
    fn loaders_are_deterministic_for_an_instant() {
        let _pinned_theme = theme::pin_theme_state();
        type Render = fn(&mut Buffer, Rect, i64);
        let renders: [Render; 3] = [
            render_classic_compact_at,
            render_sonic_speed_up_at,
            render_starlight_at,
        ];
        for render in renders {
            let area = Rect::new(0, 0, 60, 1);
            let mut first = surface_buffer(60, 1, Color::Rgb(11, 13, 17));
            let mut second = first.clone();
            render(&mut first, area, QUIET_MS);
            render(&mut second, area, QUIET_MS);
            assert_eq!(first, second);
        }
    }

    #[test]
    fn classic_compact_interlaces_complementary_spectrum_colours() {
        let _pinned_theme = theme::pin_theme_state();
        theme::set_theme_by_name("emerald");
        let spectrum = theme::loader_spectrum();
        let mut buf = surface_buffer(40, 1, Color::Rgb(6, 17, 13));
        render_classic_compact_at(&mut buf, Rect::new(0, 0, 40, 1), QUIET_MS);
        for cell in &buf.content {
            assert_eq!(cell.symbol(), UPPER_HALF);
            assert!(on_spectrum(&spectrum, cell.fg), "top {:?}", cell.fg);
            assert!(on_spectrum(&spectrum, cell.bg), "bottom {:?}", cell.bg);
            assert_ne!(cell.fg, cell.bg, "halves sit half a loop apart");
        }
    }

    #[test]
    fn classic_compact_flows_left_one_column_per_classic_tick() {
        let _pinned_theme = theme::pin_theme_state();
        let area = Rect::new(0, 0, 30, 1);
        let mut now = surface_buffer(30, 1, Color::Rgb(11, 13, 17));
        let mut later = now.clone();
        render_classic_compact_at(&mut now, area, QUIET_MS);
        render_classic_compact_at(&mut later, area, QUIET_MS + 60);
        for x in 0..29u16 {
            for (moved, original) in [
                (later[(x, 0)].fg, now[(x + 1, 0)].fg),
                (later[(x, 0)].bg, now[(x + 1, 0)].bg),
            ] {
                let distance = theme::perceptual_distance(moved, original).expect("rgb cells");
                assert!(distance < 0.01, "column {x} moved {distance}");
            }
        }
    }

    #[test]
    fn glint_sweeps_left_to_right_and_rests_between_sweeps() {
        let period_start = 1_700_000_000_000 - 1_700_000_000_000 % GLINT_PERIOD_MS;
        let centres: Vec<f64> = (0..GLINT_SWEEP_MS)
            .step_by(100)
            .map(|offset| glint_center(period_start + offset, 80).expect("mid sweep"))
            .collect();
        assert!(
            centres.windows(2).all(|pair| pair[1] > pair[0]),
            "{centres:?}"
        );
        assert!(centres[0] < 0.0, "a sweep starts off the left edge");
        assert!(glint_center(period_start + GLINT_SWEEP_MS, 80).is_none());
        assert!(glint_center(period_start + GLINT_PERIOD_MS - 1, 80).is_none());

        let peak = glint_strength(10.0, 10.0);
        assert!(peak > 0.5);
        assert!(glint_strength(11.0, 10.0) < peak);
        assert!(glint_strength(10.0 + 3.0 * GLINT_RADIUS, 10.0) < 0.01);
    }

    #[test]
    fn classic_compact_glint_brightens_the_ribbon() {
        let _pinned_theme = theme::pin_theme_state();
        theme::set_theme_by_name("goth");
        let period_start = QUIET_MS - QUIET_MS.rem_euclid(GLINT_PERIOD_MS);
        let instant = period_start + GLINT_SWEEP_MS / 2;
        let centre = glint_center(instant, 40).expect("mid sweep");
        let column = centre.floor() as u16;
        let mut buf = surface_buffer(40, 1, Color::Rgb(11, 13, 17));
        render_classic_compact_at(&mut buf, Rect::new(0, 0, 40, 1), instant);
        let spectrum = theme::loader_spectrum();
        let travel = instant.rem_euclid(RIBBON_WRAP_MS) as f64 / RIBBON_MS_PER_COLUMN;
        let plain = sample(
            &spectrum,
            (f64::from(column) + travel) / RIBBON_PERIOD_COLUMNS,
        );
        let lit = theme::perceptual_lightness(buf[(column, 0)].fg).expect("rgb");
        let unlit = theme::perceptual_lightness(rgb_color(plain)).expect("rgb");
        assert!(lit > unlit + 0.05, "glint {lit} over ribbon {unlit}");
    }

    #[test]
    fn boost_surges_without_ever_stalling() {
        let speeds: Vec<f64> = (0..520)
            .map(|step| {
                let t = f64::from(step) * 0.01;
                (boost_distance(t + 0.001) - boost_distance(t)) / 0.001
            })
            .collect();
        assert!(speeds.iter().all(|&speed| speed > 0.95), "always moving");
        let peak = speeds.iter().copied().fold(f64::MIN, f64::max);
        assert!(peak > 1.0 + BOOST_GAIN * 0.95, "surge reaches {peak}");
        assert!((boost_distance(0.0)).abs() < 1e-9);
        assert!((boost_level(BOOST_PERIOD_SECS / 2.0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn streaks_light_the_head_and_fade_along_the_tail() {
        let length = 10.0;
        assert_eq!(
            streak_intensity(-0.6, length),
            0.0,
            "nothing ahead of the head"
        );
        assert!(streak_intensity(-0.25, length) < streak_intensity(0.25, length));
        assert!((streak_intensity(0.5, length) - 1.0).abs() < 1e-6);
        let tail: Vec<f32> = (1..=10)
            .map(|step| streak_intensity(0.5 + f64::from(step), length))
            .collect();
        assert!(tail.windows(2).all(|pair| pair[1] < pair[0]), "{tail:?}");
        assert_eq!(streak_intensity(length + 0.5, length), 0.0);
    }

    #[test]
    fn streak_heads_race_right_and_wrap_back_in() {
        let width = 80;
        let track = f64::from(width) + STREAK_ENTRY + STREAK_TAIL_MAX;
        for lane in 0..2 {
            let mut previous = lane_streaks(lane, width, 1_000.0);
            for step in 1..400 {
                let current = lane_streaks(lane, width, 1_000.0 + f64::from(step) * 0.008);
                for (before, after) in previous.iter().zip(&current) {
                    let advance = (after.head - before.head).rem_euclid(track);
                    assert!(
                        advance > 0.0 && advance < 2.0,
                        "lane {lane} advanced {advance}"
                    );
                    assert!(after.head >= -STREAK_ENTRY && after.head < track - STREAK_ENTRY);
                }
                previous = current;
            }
        }
        let far = lane_streaks(0, width, 0.0).len();
        assert_eq!(far, lane_streaks(1, width, 0.0).len());
        assert!(
            far >= 2,
            "an 80-column row carries several streaks per lane"
        );
    }

    #[test]
    fn sonic_draws_half_row_streaks_that_fade_into_the_surface() {
        let _pinned_theme = theme::pin_theme_state();
        theme::set_theme_by_name("goth");
        let surface = Color::Rgb(11, 13, 17);
        let mut buf = surface_buffer(80, 1, surface);
        render_sonic_speed_up_at(&mut buf, Rect::new(0, 0, 80, 1), QUIET_MS);
        let mut lit = 0;
        for cell in &buf.content {
            match cell.symbol() {
                UPPER_HALF | LOWER_HALF => {
                    lit += 1;
                    let fg = theme::perceptual_lightness(cell.fg).expect("rgb streak");
                    assert!(fg > theme::perceptual_lightness(surface).expect("rgb"));
                }
                "·" => {}
                " " => assert_eq!(cell.bg, surface, "empty cells keep the surface"),
                other => panic!("unexpected sonic glyph {other:?}"),
            }
        }
        assert!(lit >= 10, "streaks light {lit} cells");
    }

    #[test]
    fn sonic_streaks_move_between_frames() {
        let _pinned_theme = theme::pin_theme_state();
        let area = Rect::new(0, 0, 80, 1);
        let mut now = surface_buffer(80, 1, Color::Rgb(11, 13, 17));
        let mut later = now.clone();
        render_sonic_speed_up_at(&mut now, area, QUIET_MS);
        render_sonic_speed_up_at(&mut later, area, QUIET_MS + 50);
        assert_ne!(symbols(&now), symbols(&later));
    }

    #[test]
    fn starlight_twinkles_star_glyphs_over_a_faint_nebula() {
        let _pinned_theme = theme::pin_theme_state();
        theme::set_theme_by_name("saphire");
        let surface = Color::Rgb(5, 11, 23);
        let mut buf = surface_buffer(80, 1, surface);
        render_starlight_at(&mut buf, Rect::new(0, 0, 80, 1), QUIET_MS);
        let stars = symbols(&buf)
            .iter()
            .filter(|symbol| matches!(symbol.as_str(), "·" | "✧" | "✦"))
            .count();
        assert!(stars >= 12, "{stars} stars in 80 columns");
        for cell in &buf.content {
            assert!(
                matches!(cell.symbol(), " " | "·" | "✧" | "✦"),
                "unexpected starlight glyph {:?}",
                cell.symbol()
            );
            let haze = theme::perceptual_distance(cell.bg, surface).expect("rgb haze");
            assert!(haze < 0.18, "nebula stays faint: {haze}");
        }
        assert!(
            buf.content.iter().any(|cell| cell.bg != surface),
            "the nebula tints the surface"
        );
    }

    #[test]
    fn starlight_stars_flare_and_rest_over_time() {
        let _pinned_theme = theme::pin_theme_state();
        let area = Rect::new(0, 0, 80, 1);
        let frames: Vec<Vec<String>> = (0..4)
            .map(|step| {
                let mut buf = surface_buffer(80, 1, Color::Rgb(11, 13, 17));
                render_starlight_at(&mut buf, area, QUIET_MS + step * 300);
                symbols(&buf)
            })
            .collect();
        assert!(frames.windows(2).any(|pair| pair[0] != pair[1]));
        assert!(
            frames.iter().flatten().any(|symbol| symbol == "✦"),
            "some star reaches a full flare"
        );
        assert!(twinkle(42, 0.0) >= STAR_REST && twinkle(42, 0.0) <= 1.0);
    }

    #[test]
    fn starlight_leaves_terminal_default_backgrounds_unpainted() {
        let _pinned_theme = theme::pin_theme_state();
        theme::set_theme_by_name("truly-transparent");
        let mut buf = surface_buffer(60, 1, Color::Reset);
        render_starlight_at(&mut buf, Rect::new(0, 0, 60, 1), QUIET_MS);
        assert!(buf.content.iter().all(|cell| cell.bg == Color::Reset));
        assert!(
            buf.content
                .iter()
                .any(|cell| cell.symbol() != " " && matches!(cell.fg, Color::Rgb(..))),
            "stars still shine"
        );
    }

    #[test]
    fn shooting_star_crosses_during_its_flight_window() {
        let _pinned_theme = theme::pin_theme_state();
        let area = Rect::new(0, 0, 80, 1);
        let period_start = QUIET_MS - QUIET_MS.rem_euclid(METEOR_PERIOD_MS);
        let mut flying = surface_buffer(80, 1, Color::Rgb(11, 13, 17));
        render_starlight_at(&mut flying, area, period_start + METEOR_FLIGHT_MS / 2);
        let trail = symbols(&flying)
            .iter()
            .filter(|symbol| matches!(symbol.as_str(), "━" | "─"))
            .count();
        assert!(trail >= 4, "meteor trail of {trail} cells");

        let mut resting = surface_buffer(80, 1, Color::Rgb(11, 13, 17));
        render_starlight_at(&mut resting, area, QUIET_MS);
        assert!(
            symbols(&resting)
                .iter()
                .all(|symbol| !matches!(symbol.as_str(), "━" | "─"))
        );
    }

    #[test]
    fn loaders_follow_the_active_theme_spectrum() {
        let _pinned_theme = theme::pin_theme_state();
        let area = Rect::new(0, 0, 60, 1);
        let mut frames = Vec::new();
        for key in ["emerald", "ruby"] {
            theme::set_theme_by_name(key);
            let spectrum = theme::loader_spectrum();
            let mut ribbon = surface_buffer(60, 1, Color::Rgb(10, 10, 10));
            render_classic_compact_at(&mut ribbon, area, QUIET_MS);
            assert!(
                ribbon
                    .content
                    .iter()
                    .all(|cell| on_spectrum(&spectrum, cell.fg))
            );
            let mut sonic = surface_buffer(60, 1, Color::Rgb(10, 10, 10));
            render_sonic_speed_up_at(&mut sonic, area, QUIET_MS);
            frames.push((ribbon, sonic));
        }
        assert_ne!(frames[0].0, frames[1].0);
        assert_ne!(frames[0].1, frames[1].1);
    }

    #[test]
    fn light_surfaces_get_no_pale_highlight() {
        let _pinned_theme = theme::pin_theme_state();
        assert!(glow_for((255, 255, 255)).is_none());
        assert!(glow_for((11, 13, 17)).is_some());
    }

    #[test]
    fn helpers_wrap_and_blend_predictably() {
        assert_eq!(mix((0, 0, 0), (200, 100, 50), 0.5), (100, 50, 25));
        assert_eq!(mix((10, 20, 30), (200, 100, 50), -1.0), (10, 20, 30));
        assert_eq!(mix((10, 20, 30), (200, 100, 50), 2.0), (200, 100, 50));
        assert!((turns(-0.25) - 0.75).abs() < 1e-6);
        assert!((0..1_000).all(|seed| (0.0..=1.0).contains(&unit(seed))));
        let instant = first_instant_where(QUIET_MS, |instant| glint_center(instant, 40).is_some());
        assert_eq!(instant.rem_euclid(GLINT_PERIOD_MS), 0);
    }
}
