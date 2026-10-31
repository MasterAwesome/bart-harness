use std::{
    io::{self, Write as _},
    time::{Duration, Instant},
};

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use ratatui::style::Color;

const TIMEOUT: Duration = Duration::from_millis(300);
const MAX_REPLY_BYTES: usize = 1024;

type Rgb = (u8, u8, u8);

const SLOTS: [u8; 12] = [1, 2, 3, 4, 5, 6, 9, 10, 11, 12, 13, 14];

const READABLE: f64 = 4.5;

#[derive(Clone, Copy)]
pub(super) struct Theme {
    pub(super) brand: Color,
    pub(super) success: Color,
    pub(super) error: Color,
    pub(super) code: Color,
    pub(super) keyword: Color,
    pub(super) literal: Color,
    pub(super) band: Option<Color>,
    pub(super) muted: Option<Color>,
    pub(super) added: Option<Color>,
    pub(super) removed: Option<Color>,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            brand: Color::Magenta,
            success: Color::Green,
            error: Color::Red,
            code: Color::Cyan,
            keyword: Color::Blue,
            literal: Color::Yellow,
            band: None,
            muted: None,
            added: None,
            removed: None,
        }
    }
}

#[derive(Default)]
struct Palette {
    foreground: Option<Rgb>,
    background: Option<Rgb>,
    slots: [Option<Rgb>; 16],
}

impl Palette {
    fn theme(&self) -> Theme {
        let Some(background) = self.background else {
            return Theme::default();
        };
        let color = |rgb: Rgb| Color::Rgb(rgb.0, rgb.1, rgb.2);
        let tint = tint(background);
        let band = (self
            .foreground
            .is_some_and(|foreground| contrast(foreground, tint) >= READABLE))
        .then_some(tint);
        let readable = |rgb: Rgb| {
            contrast(rgb, background) >= READABLE
                && band.is_none_or(|band| contrast(rgb, band) >= READABLE)
        };
        let pick = |slot: usize, named: Color, bright: Color| {
            let Some(normal) = self.slots[slot] else {
                return named;
            };
            if readable(normal) {
                return named;
            }
            if self.slots[slot + 8].is_some_and(readable) {
                return bright;
            }
            let Some(foreground) = self.foreground else {
                return named;
            };
            (1..=5)
                .map(|tenths| mix(normal, foreground, tenths))
                .find(|&rgb| readable(rgb))
                .map_or(Color::Reset, color)
        };
        let muted = self.foreground.and_then(|foreground| {
            (1..=5)
                .rev()
                .map(|tenths| mix(foreground, background, tenths))
                .find(|&rgb| readable(rgb))
                .map(color)
        });
        let wash = |slot: usize| {
            let (accent, foreground) = (self.slots[slot]?, self.foreground?);
            (1..=3)
                .rev()
                .map(|tenths| mix(background, accent, tenths))
                .find(|&rgb| contrast(foreground, rgb) >= READABLE)
                .map(color)
        };
        Theme {
            muted,
            added: wash(2),
            removed: wash(1),
            brand: pick(5, Color::Magenta, Color::LightMagenta),
            success: pick(2, Color::Green, Color::LightGreen),
            error: pick(1, Color::Red, Color::LightRed),
            code: pick(6, Color::Cyan, Color::LightCyan),
            keyword: pick(4, Color::Blue, Color::LightBlue),
            literal: pick(3, Color::Yellow, Color::LightYellow),
            band: band.map(color),
        }
    }
}

fn mix(from: Rgb, to: Rgb, tenths: u32) -> Rgb {
    let channel = |a: u8, b: u8| {
        let mixed = (u32::from(a) * (10 - tenths) + u32::from(b) * tenths + 5) / 10;
        u8::try_from(mixed).unwrap_or(u8::MAX)
    };
    (
        channel(from.0, to.0),
        channel(from.1, to.1),
        channel(from.2, to.2),
    )
}

fn contrast(a: Rgb, b: Rgb) -> f64 {
    let luminance = |(r, g, b): Rgb| {
        let linear = |channel: u8| {
            let value = f64::from(channel) / 255.0;
            if value <= 0.040_45 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
    };
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

fn tint((r, g, b): Rgb) -> Rgb {
    let luma = 299 * u32::from(r) + 587 * u32::from(g) + 114 * u32::from(b);
    let (toward, percent) = if luma < 128 * 1000 { (255, 10) } else { (0, 5) };
    let mix = |channel: u8| {
        let mixed = (u32::from(channel) * (100 - percent) + toward * percent + 50) / 100;
        u8::try_from(mixed).unwrap_or(u8::MAX)
    };
    (mix(r), mix(g), mix(b))
}

pub(super) fn theme(truecolor: bool) -> (Theme, Vec<Event>) {
    let mut pending = Vec::new();
    let theme = if truecolor {
        query(&mut pending).theme()
    } else {
        Theme::default()
    };
    (theme, pending)
}

fn query(pending: &mut Vec<Event>) -> Palette {
    let mut request = b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\".to_vec();
    for slot in SLOTS {
        request.extend(format!("\x1b]4;{slot};?\x1b\\").bytes());
    }
    let mut out = io::stdout();
    if out.write_all(&request).and_then(|()| out.flush()).is_err() {
        return Palette::default();
    }
    let deadline = Instant::now() + TIMEOUT;
    let mut reply = Palette::default();
    let mut candidate = Vec::new();
    let mut bytes = Vec::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        if !event::poll(left).unwrap_or(false) {
            break;
        }
        let Ok(event) = event::read() else { break };
        let byte = match &event {
            Event::Key(key)
                if key.modifiers == KeyModifiers::ALT && key.code == KeyCode::Char(']') =>
            {
                Some(b"\x1b]".as_slice())
            }
            Event::Key(key)
                if key.modifiers == KeyModifiers::ALT && key.code == KeyCode::Char('\\') =>
            {
                Some(b"\x1b\\".as_slice())
            }
            Event::Key(key)
                if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('g') =>
            {
                Some(b"\x07".as_slice())
            }
            _ => None,
        };
        if byte == Some(b"\x1b]".as_slice()) {
            pending.append(&mut candidate);
            bytes.clear();
        }
        if candidate.is_empty() && byte != Some(b"\x1b]".as_slice()) {
            pending.push(event);
            continue;
        }
        if let Some(byte) = byte {
            bytes.extend_from_slice(byte);
        } else if let Event::Key(key) = &event
            && let KeyCode::Char(ch) = key.code
            && (key.modifiers.is_empty()
                || (key.modifiers == KeyModifiers::SHIFT && ch.is_ascii_uppercase()))
            && ch.is_ascii()
        {
            bytes.push(u8::try_from(ch).expect("checked ASCII"));
        } else {
            pending.append(&mut candidate);
            pending.push(event);
            bytes.clear();
            continue;
        }
        candidate.push(event);
        if bytes.ends_with(b"\x07") || bytes.ends_with(b"\x1b\\") {
            let palette = parse(&bytes);
            if palette.foreground.is_some()
                || palette.background.is_some()
                || palette.slots.iter().any(Option::is_some)
            {
                reply.foreground = palette.foreground.or(reply.foreground);
                reply.background = palette.background.or(reply.background);
                for (slot, color) in reply.slots.iter_mut().zip(palette.slots) {
                    *slot = color.or(*slot);
                }
                candidate.clear();
            } else {
                pending.append(&mut candidate);
            }
            bytes.clear();
        } else if bytes.len() >= MAX_REPLY_BYTES {
            pending.append(&mut candidate);
            bytes.clear();
        }
    }
    pending.append(&mut candidate);
    reply
}

fn parse(reply: &[u8]) -> Palette {
    let Ok(text) = std::str::from_utf8(reply) else {
        return Palette::default();
    };
    let mut palette = Palette {
        foreground: color(text, "\x1b]10;"),
        background: color(text, "\x1b]11;"),
        ..Palette::default()
    };
    for slot in SLOTS {
        palette.slots[usize::from(slot)] = color(text, &format!("\x1b]4;{slot};"));
    }
    palette
}

fn color(text: &str, prefix: &str) -> Option<Rgb> {
    let start = text.find(prefix)? + prefix.len();
    let value = text[start..].strip_prefix("rgb:")?;
    let end = value.find(['\x07', '\x1b'])?;
    let mut components = value[..end].split('/').map(|hex| {
        let digits = u32::try_from(hex.len())
            .ok()
            .filter(|&n| (1..=4).contains(&n))?;
        if !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        let value = u32::from_str_radix(hex, 16).ok()?;
        let max = 16u32.pow(digits) - 1;
        u8::try_from((value * 255 + max / 2) / max).ok()
    });
    let rgb = (
        components.next()??,
        components.next()??,
        components.next()??,
    );
    components.next().is_none().then_some(rgb)
}
