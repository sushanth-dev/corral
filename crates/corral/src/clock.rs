//! The status bar's right-hand clock.
//!
//! Formatting a wall-clock time in the local timezone is the one thing
//! the standard library does not offer, and a date crate would be a new
//! dependency for a single format string. The platform's `date` already
//! knows the timezone, so the format is delegated to it, the same way
//! the socket path delegates the user id to `id` (see
//! `crate::socket_path`).

use std::time::{Duration, Instant};

/// How long a formatted string is reused. Tmux refreshes its status line
/// every `status-interval` (15s by default), which bounds how stale the
/// displayed minute can be; matching it keeps the bar as current as the
/// one it imitates.
const REFRESH: Duration = Duration::from_secs(15);

/// Tmux's default `status-right` format, so the bar reads the same.
const DATE_FORMAT: &str = "+%H:%M %d-%b-%y";

/// The bar's clock, re-formatted at most once per `REFRESH`.
pub struct Clock {
    text: String,
    next: Instant,
}

impl Clock {
    pub fn new() -> Self {
        let mut clock = Self {
            text: String::new(),
            next: Instant::now(),
        };
        clock.refresh();
        clock
    }

    /// The current text, re-formatted when the refresh window has
    /// passed. A failed re-format leaves the last good text in place
    /// rather than blanking the bar.
    pub fn text(&mut self) -> &str {
        if Instant::now() >= self.next {
            self.refresh();
        }
        &self.text
    }

    fn refresh(&mut self) {
        if let Some(text) = format_now() {
            self.text = text;
        }
        self.next = Instant::now() + REFRESH;
    }
}

/// The local time in `DATE_FORMAT`, or `None` when the platform has no
/// usable `date`.
fn format_now() -> Option<String> {
    let out = std::process::Command::new("date")
        .arg(DATE_FORMAT)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clock_formats_a_time_and_a_date() {
        let text = format_now().expect("the platform's `date` must format the bar's clock");
        assert!(text.contains(':'), "no time in {text:?}");
        // `%d-%b-%y` carries two separators; a month abbreviation in a
        // non-English locale may be wider than three characters, so only
        // the separators are asserted.
        assert_eq!(text.matches('-').count(), 2, "no date in {text:?}");
    }

    #[test]
    fn the_clock_text_is_cached_between_refreshes() {
        let mut clock = Clock::new();
        let first = clock.text().to_string();
        assert!(!first.is_empty(), "the first read must be filled in");
        assert_eq!(
            clock.text(),
            first,
            "a second read inside the window must be the same text"
        );
    }
}
