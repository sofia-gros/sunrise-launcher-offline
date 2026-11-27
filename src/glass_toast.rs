//! What the launcher just did, said for 4.2 s (the mockup's `say`): one card along the window's
//! bottom, at every width, over every page and dialog, its path or command in mono. It fades in,
//! holds its time while the pointer is on it, and a click lets it go.
//!
//! Any handler can say something, even one with only `&mut App`: the line waits here, wakes the
//! launcher's drain (`wake.rs`), which draws it, then lets it go.

use super::*;

use std::path::Path;
use std::sync::Mutex;

/// How long a line stays (`setTimeout(…, 4200)`).
const SHOWN: Duration = Duration::from_millis(4200);

/// A line's pieces: the words, and a path or command (`true`) set in mono.
type Parts = Vec<(String, bool)>;

/// The line showing: its pieces, when its 4.2 s started (moved on by the time it was held
/// under the pointer), its number (a new one restarts them), and since when it's held.
struct Line {
    parts: Parts,
    since: Instant,
    n: u64,
    held: Option<Instant>,
}

static LINE: Mutex<Option<Line>> = Mutex::new(None);

fn say_parts(parts: Parts) {
    let mut line = LINE.lock().unwrap_or_else(|e| e.into_inner());
    let n = line.as_ref().map_or(1, |l| l.n + 1);
    *line = Some(Line {
        parts,
        since: Instant::now(),
        n,
        held: None,
    });
    crate::wake::wake();
}

/// The pointer is on the toast (its time stops) or left it (its time goes on).
fn hold(on: bool) {
    let mut line = LINE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(l) = line.as_mut() {
        match (on, l.held) {
            (true, None) => l.held = Some(Instant::now()),
            (false, Some(from)) => {
                l.since += from.elapsed();
                l.held = None;
            }
            _ => {}
        }
    }
}

/// Lets the line go now: a click on the toast.
fn dismiss() {
    *LINE.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Says `text` now, in place of whatever was said before.
pub(super) fn say(text: impl Into<String>) {
    say_parts(vec![(text.into(), false)]);
}

/// Says `en` translated, its one `{hole}` filled with `value` in mono, wherever the language
/// puts it.
pub(super) fn say_with(en: &str, value: &str) {
    say_parts(parts_with(tr(en), value));
}

/// `said`'s one `{hole}` filled with `value` in mono.
fn parts_with(said: &str, value: &str) -> Parts {
    match (said.find('{'), said.find('}')) {
        (Some(a), Some(b)) if a < b => [
            (said[..a].to_string(), false),
            (value.to_string(), true),
            (said[b + 1..].to_string(), false),
        ]
        .into_iter()
        .filter(|(p, _)| !p.trim().is_empty())
        .collect(),
        _ => vec![(said.to_string(), false)],
    }
}

/// The line still showing, and its number.
fn current_parts() -> Option<(Parts, u64)> {
    let mut line = LINE.lock().unwrap_or_else(|e| e.into_inner());
    if line
        .as_ref()
        .is_some_and(|l| l.held.is_none() && l.since.elapsed() >= SHOWN)
    {
        *line = None;
    }
    line.as_ref().map(|l| (l.parts.clone(), l.n))
}

fn current() -> Option<(String, u64)> {
    current_parts().map(|(parts, n)| {
        let text: Vec<_> = parts.iter().map(|(p, _)| p.trim()).collect();
        (text.join(" "), n)
    })
}

/// `/Users/me/x` as `~/x`, as the mockup writes folders.
pub(super) fn tilde(path: &Path) -> String {
    let shown = path.display().to_string();
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && shown.starts_with(&home) => {
            format!("~{}", &shown[home.len()..])
        }
        _ => shown,
    }
}

/// Opens `path` in the system's file manager (or its app), and says so.
pub(super) fn open(cx: &mut App, path: &Path) {
    cx.open_with_system(path);
    say_with("Opened {path}", &tilde(path));
}

impl Launcher {
    /// From the drain: whether the line changed (a new one, or the last one gone).
    pub(super) fn toast_tick(&mut self) -> bool {
        let now = current().map(|l| l.1);
        let changed = now != self.toast_seen;
        self.toast_seen = now;
        changed
    }

    /// The line along the window's bottom while it is said: a glass card over every page and
    /// dialog, its path or command in mono.
    pub(super) fn toast_float(&self) -> Option<Div> {
        toast_card()
    }
}

/// The card for the line still showing, none once it is gone.
fn toast_card() -> Option<Div> {
    {
        let (parts, n) = current_parts()?;
        let p = crate::paint::palette();
        let mut line = div()
            .flex()
            .flex_wrap()
            .items_baseline()
            .gap_x(px(6.))
            .min_w_0();
        for (part, mono) in parts {
            let part = part.trim().to_string();
            line = line.child(if mono {
                div().font_family(crate::theme::MONO).child(part)
            } else {
                div().child(part)
            });
        }
        Some(
            div()
                .absolute()
                .left(px(16.))
                .right(px(16.))
                .bottom(px(16.))
                .flex()
                .justify_center()
                .child(
                    div()
                        .id("toast")
                        .occlude()
                        .cursor_pointer()
                        .on_hover(|on, window, _| {
                            hold(*on);
                            window.refresh();
                        })
                        .on_click(|_, window, _| {
                            dismiss();
                            window.refresh();
                        })
                        .max_w(px(560.))
                        .px(px(16.))
                        .py(px(12.))
                        .rounded(crate::tokens::r(crate::tokens::R_PANEL))
                        .border_1()
                        .border_color(gpui::rgb(p.line))
                        .bg(gpui::rgb(p.raised))
                        .text_color(gpui::rgb(p.text))
                        .text_size(px(13.))
                        .shadow_lg()
                        .child(line)
                        .with_animation(
                            ElementId::named_usize("toast", n as usize),
                            crate::motion::eased(sunrise_launcher_theme::QUICK),
                            |d, p| d.opacity(p),
                        ),
                ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_card_shows_while_a_line_is_said_and_goes_when_it_expires_or_is_clicked() {
        // One test: the line is a process-wide static.
        say("Copied it");
        assert!(toast_card().is_some());
        // Held under the pointer, its time does not run out.
        hold(true);
        LINE.lock().unwrap().as_mut().unwrap().since -= SHOWN * 2;
        assert!(toast_card().is_some());
        hold(false);
        // Released, the 4.2 s are long past.
        LINE.lock().unwrap().as_mut().unwrap().since -= SHOWN * 2;
        assert!(toast_card().is_none());
        say("Copied again");
        assert!(toast_card().is_some());
        dismiss();
        assert!(toast_card().is_none());
    }

    #[test]
    fn the_hole_is_set_in_mono_between_the_words() {
        assert_eq!(
            parts_with("Copied {text}", "AB12-CD"),
            vec![
                ("Copied ".to_string(), false),
                ("AB12-CD".to_string(), true)
            ]
        );
        assert_eq!(
            parts_with("Opened {path} now", "~/x"),
            vec![
                ("Opened ".to_string(), false),
                ("~/x".to_string(), true),
                (" now".to_string(), false)
            ]
        );
    }
}
