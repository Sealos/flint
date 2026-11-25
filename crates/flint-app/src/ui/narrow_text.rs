//! Text that has to fit a narrow pane: what to keep of a string when the room is short.

/// The width of one character of the sidebar's small text, in points.
const SMALL_CHAR_WIDTH: f32 = 6.5;

/// What a cluster row spends beside its second line's text: padding, the status dot and gaps.
const ROW_CHROME: f32 = 56.;

/// The fewest characters a cut address keeps: enough for the start of the host and the port.
const MIN_ADDRESS_CHARS: usize = 12;

/// How many characters of a cluster row's second line (its address) fit a sidebar `width` wide.
pub(super) fn sidebar_subtitle_chars(width: f32) -> usize {
    (((width - ROW_CHROME) / SMALL_CHAR_WIDTH).floor().max(0.) as usize).max(MIN_ADDRESS_CHARS)
}

/// `text` as is when it fits `chars` characters, else cut in the middle, so its end (a port, a
/// final label) stays in view where a cut at the end would show only what clusters on one host
/// share.
pub(super) fn fit_end_visible(text: &str, chars: usize) -> String {
    flint_core::display::middle_truncate(text, chars)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sidebar_width_decides_how_much_of_an_address_shows() {
        // The narrowest sidebar still keeps room for a host start and a port.
        assert_eq!(sidebar_subtitle_chars(140.), MIN_ADDRESS_CHARS);
        assert_eq!(sidebar_subtitle_chars(0.), MIN_ADDRESS_CHARS);
        assert_eq!(sidebar_subtitle_chars(200.), 22);
        assert!(sidebar_subtitle_chars(420.) > sidebar_subtitle_chars(200.));
    }

    #[test]
    fn a_long_address_keeps_its_port() {
        let shown = fit_end_visible("127.0.0.1:50123", 12);
        assert_eq!(shown.chars().count(), 12);
        assert!(shown.ends_with("50123"), "{shown}");
        assert!(shown.starts_with("127.0"), "{shown}");
        // Two clusters on one host differ in what is shown.
        assert_ne!(shown, fit_end_visible("127.0.0.1:50124", 12));
    }

    #[test]
    fn an_address_that_fits_is_unchanged() {
        assert_eq!(fit_end_visible("10.0.0.1:6443", 22), "10.0.0.1:6443");
    }
}
