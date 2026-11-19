//! The orange accent, the only accent color in Flint (ARCHITECTURE.md, "Look and keyboard").
//!
//! The neutral and status colors of Flint Light and Flint Dark are data (`themes/flint.json`); the
//! accent is applied on top of them from [`ACCENT`], so trying another orange is a one-line change
//! here. It is not a user setting.

/// The accent in use. To try another candidate, name it here: [`EMBER`], [`TANGERINE`] or
/// [`SIGNAL`].
pub const ACCENT: Accent = EMBER;

/// An orange for both themes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Accent {
    pub name: &'static str,
    pub light: Shades,
    pub dark: Shades,
}

/// One theme's shades of an accent, as `#rrggbb`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shades {
    /// Fills and marks: primary buttons, focus rings, the selection, the active cluster.
    pub fill: &'static str,
    pub hover: &'static str,
    pub active: &'static str,
    /// Text and icons drawn on `fill`.
    pub on_fill: &'static str,
    /// The accent as text on the theme's surfaces: links and highlighted matches.
    pub text: &'static str,
}

/// Burnt orange in light, bright orange in dark; white on the light fill, near-black on the dark
/// one. The recommended default.
pub const EMBER: Accent = Accent {
    name: "Ember",
    light: Shades {
        fill: "#c2410c",
        hover: "#b13a0a",
        active: "#9a3412",
        on_fill: "#ffffff",
        text: "#c2410c",
    },
    dark: Shades {
        fill: "#f97316",
        hover: "#fb8530",
        active: "#ea580c",
        on_fill: "#111111",
        text: "#f97316",
    },
};

/// A warmer, more amber orange, softer in dark.
// Alternatives stay compiled so swapping [`ACCENT`] is one line; only the tests read them.
#[cfg_attr(not(test), allow(dead_code))]
pub const TANGERINE: Accent = Accent {
    name: "Tangerine",
    light: Shades {
        fill: "#b45309",
        hover: "#a14a08",
        active: "#92400e",
        on_fill: "#ffffff",
        text: "#b45309",
    },
    dark: Shades {
        fill: "#fb923c",
        hover: "#fda15a",
        active: "#f97316",
        on_fill: "#111111",
        text: "#fb923c",
    },
};

/// The most vivid orange, with near-black text on the fill in both themes; links use a darker
/// orange in light to stay readable.
#[cfg_attr(not(test), allow(dead_code))]
pub const SIGNAL: Accent = Accent {
    name: "Signal",
    light: Shades {
        fill: "#e8590c",
        hover: "#ee6420",
        active: "#f07330",
        on_fill: "#1a1a1a",
        text: "#bd4800",
    },
    dark: Shades {
        fill: "#ff801f",
        hover: "#ff8f3a",
        active: "#f0700f",
        on_fill: "#111111",
        text: "#ff801f",
    },
};

/// Every candidate, for the contrast tests.
#[cfg(test)]
pub const CANDIDATES: [Accent; 3] = [EMBER, TANGERINE, SIGNAL];

#[cfg(test)]
mod tests {
    use super::{ACCENT, CANDIDATES};

    #[test]
    fn the_accent_in_use_is_a_candidate_and_names_are_unique() {
        assert!(CANDIDATES.contains(&ACCENT));
        let mut names: Vec<_> = CANDIDATES.iter().map(|a| a.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), CANDIDATES.len());
    }

    #[test]
    fn shades_are_hex_colors() {
        for accent in CANDIDATES {
            for shades in [accent.light, accent.dark] {
                for value in [
                    shades.fill,
                    shades.hover,
                    shades.active,
                    shades.on_fill,
                    shades.text,
                ] {
                    assert!(
                        value.len() == 7
                            && value.starts_with('#')
                            && value[1..].chars().all(|c| c.is_ascii_hexdigit()),
                        "{}: {value}",
                        accent.name
                    );
                }
            }
        }
    }
}
