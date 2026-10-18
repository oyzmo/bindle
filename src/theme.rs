//! Bindle's own colour palette, and the stock GNOME one it can fall back to.
//!
//! Two surfaces need colouring and they are driven from here together, because
//! they have to agree: the widget chrome (Adwaita's named colours, overridden
//! through a `CssProvider`) and the text tags inside the editor and preview
//! buffers (`highlight::retint` / `preview::retint`), which GTK styles from
//! `TextTag` properties rather than from CSS and so cannot see the stylesheet.
//!
//! Everything is resolved for a concrete (theme, dark) pair — there is no CSS
//! branch on the colour scheme. `ui.rs` re-applies both halves whenever the
//! theme setting or `StyleManager::is_dark` changes, which is cheaper than it
//! sounds (one provider reload, a dozen tag property sets) and keeps the two
//! kinds of colour from drifting apart.
//!
//! Palette (myinput/README.md): sage `#7FA07A`, cream `#F4E7D3`,
//! cocoa `#4A3428`, tan `#8A6A50`. The syntax colours below are extensions in
//! the same warm family rather than the four base tones: highlighting needs
//! more separable hues than a two-hue palette can supply, and every value here
//! was checked for ≥4.5:1 against the surface it is drawn on.

use crate::settings::Theme;

/// Colours for the editor's `hl-*` tags and the preview's tinted tags.
/// Fields are hex strings because that is what `TextTag` takes.
pub struct Tags {
    pub heading: &'static str,
    pub marker: &'static str,
    pub code: &'static str,
    pub link: &'static str,
    pub url: &'static str,
    /// Blue, not a second green: `key` and `string` are the pair that carries
    /// a YAML/JSON/TOML document (`key:` against `"value"`), and two shades of
    /// the same hue read as one colour at body size. It shares `link`'s value
    /// deliberately — `hl-link` is emitted only by the markdown scanner and
    /// `hl-key` only by the data ones, so the two never meet in a document,
    /// and the palette keeps a single blue.
    pub key: &'static str,
    pub string: &'static str,
    pub number: &'static str,
    pub boolean: &'static str,
    pub comment: &'static str,
    pub section: &'static str,
    /// Preview: block quotes and the `dim` tag (heading disclosure markers).
    pub dim: &'static str,
    /// Preview: background behind inline code, as (colour, alpha).
    pub code_bg: (&'static str, f32),
}

/// The GNOME palette the app shipped with: mid-saturation tones picked to stay
/// readable on both the light and the dark Adwaita surfaces, hence one set.
const STOCK: Tags = Tags {
    heading: "#3584e4",
    marker: "#e66100",
    code: "#26a269",
    link: "#3584e4",
    url: "#86868a",
    key: "#3584e4",
    string: "#26a269",
    number: "#e66100",
    boolean: "#9141ac",
    comment: "#86868a",
    section: "#e66100",
    dim: "#808080",
    code_bg: ("#808080", 0.16),
};

/// Bindle on cream (`#FBF5EA`): darkened so each hue clears AA on a light
/// surface — sage and tan themselves are far too pale to read as text.
const BINDLE_LIGHT: Tags = Tags {
    heading: "#3F5C3A",
    marker: "#9A4F22",
    code: "#2F6B54",
    link: "#2C6B8F",
    url: "#7A6250",
    key: "#2C6B8F",
    string: "#2F6B54",
    number: "#9A4F22",
    boolean: "#7A4A7C",
    comment: "#7A6250",
    section: "#9A4F22",
    dim: "#7A6250",
    code_bg: ("#8A6A50", 0.14),
};

/// Bindle on cocoa (`#251A14`): the same hues lightened, since on a dark
/// surface it is the light end that carries the contrast.
const BINDLE_DARK: Tags = Tags {
    heading: "#9BBA95",
    marker: "#E0985E",
    code: "#7FC4A4",
    link: "#8FBEDC",
    url: "#A08D78",
    key: "#8FBEDC",
    string: "#7FC4A4",
    number: "#E0985E",
    boolean: "#C89BC9",
    comment: "#A08D78",
    section: "#E0985E",
    dim: "#A08D78",
    code_bg: ("#F4E7D3", 0.10),
};

/// Text-tag colours for the current theme and colour scheme.
pub fn tags(theme: Theme, dark: bool) -> &'static Tags {
    match (theme, dark) {
        (Theme::System, _) => &STOCK,
        (Theme::Bindle, false) => &BINDLE_LIGHT,
        (Theme::Bindle, true) => &BINDLE_DARK,
    }
}

/// One Adwaita colour, named without the `--`/`@` prefix.
type Color = (&'static str, &'static str);

// Adwaita's light surfaces are ordered sidebar < window < view (#EBEBED,
// #FAFAFB, #FFFFFF): the sidebar recedes, the card comes forward. The cream
// ramp below keeps that order, which is what separates the sidebar from the
// editor — giving them the same value makes the panel edge disappear.
const LIGHT: &[Color] = &[
    ("window_bg_color", "#FBF5EA"),
    ("window_fg_color", "#4A3428"),
    ("view_bg_color", "#FDF9F1"),
    ("view_fg_color", "#4A3428"),
    ("headerbar_bg_color", "#F3E7D4"),
    ("headerbar_fg_color", "#4A3428"),
    ("headerbar_backdrop_color", "#FBF5EA"),
    ("sidebar_bg_color", "#EFE0C8"),
    ("sidebar_fg_color", "#4A3428"),
    ("sidebar_backdrop_color", "#EBDABF"),
    ("secondary_sidebar_bg_color", "#EFE0C8"),
    ("secondary_sidebar_fg_color", "#4A3428"),
    ("card_bg_color", "#FDF9F1"),
    ("card_fg_color", "#4A3428"),
    ("dialog_bg_color", "#FBF5EA"),
    ("dialog_fg_color", "#4A3428"),
    ("popover_bg_color", "#FDF9F1"),
    ("popover_fg_color", "#4A3428"),
    // Sage is too pale to carry white text at its base tone; the pressed
    // variant from the palette notes reaches 3.7:1, on a par with Adwaita's
    // own blue. `accent_color` is the one drawn as *text* on cream, so it is
    // darker again (5.5:1).
    ("accent_bg_color", "#6E8C6A"),
    ("accent_fg_color", "#FFFFFF"),
    ("accent_color", "#4F6B4A"),
];

// Mirrored for dark, as Adwaita does (#2E2E32, #222226, #1D1D20): the
// sidebar is the *lightest* of the three there and the view the darkest.
const DARK: &[Color] = &[
    ("window_bg_color", "#2E2019"),
    ("window_fg_color", "#F4E7D3"),
    ("view_bg_color", "#251A14"),
    ("view_fg_color", "#F4E7D3"),
    ("headerbar_bg_color", "#382720"),
    ("headerbar_fg_color", "#F4E7D3"),
    ("headerbar_backdrop_color", "#2E2019"),
    ("sidebar_bg_color", "#33241C"),
    ("sidebar_fg_color", "#F4E7D3"),
    ("sidebar_backdrop_color", "#2E2019"),
    ("secondary_sidebar_bg_color", "#2E2019"),
    ("secondary_sidebar_fg_color", "#F4E7D3"),
    ("card_bg_color", "#382720"),
    ("card_fg_color", "#F4E7D3"),
    ("dialog_bg_color", "#382720"),
    ("dialog_fg_color", "#F4E7D3"),
    ("popover_bg_color", "#3C2A22"),
    ("popover_fg_color", "#F4E7D3"),
    // On dark, sage is the light end of the pair, so cocoa is what sits on
    // top of it (5.4:1) rather than white.
    ("accent_bg_color", "#7FA07A"),
    ("accent_fg_color", "#2E2019"),
    ("accent_color", "#9BBA95"),
];

/// CSS overriding Adwaita's named colours, or empty for the system theme.
///
/// Both spellings are emitted on purpose: libadwaita ≥ 1.6 resolves these as
/// CSS custom properties (which is also what `load_css` reads, e.g.
/// `var(--view-bg-color)` for the preview card), while 1.5 — the minimum this
/// app builds against — only knows the `@define-color` form.
pub fn stylesheet(theme: Theme, dark: bool) -> String {
    let colors = match (theme, dark) {
        (Theme::System, _) => return String::new(),
        (Theme::Bindle, false) => LIGHT,
        (Theme::Bindle, true) => DARK,
    };
    let mut css = String::new();
    for (name, value) in colors {
        css.push_str(&format!("@define-color {name} {value};\n"));
    }
    css.push_str(":root {\n");
    for (name, value) in colors {
        css.push_str(&format!("  --{}: {value};\n", name.replace('_', "-")));
    }
    css.push_str("}\n");
    css
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_theme_adds_no_css() {
        assert!(stylesheet(Theme::System, false).is_empty());
        assert!(stylesheet(Theme::System, true).is_empty());
    }

    #[test]
    fn bindle_defines_both_spellings_of_every_color() {
        for dark in [false, true] {
            let css = stylesheet(Theme::Bindle, dark);
            let colors = if dark { DARK } else { LIGHT };
            for (name, value) in colors {
                assert!(
                    css.contains(&format!("@define-color {name} {value};")),
                    "{name} missing its @define-color in dark={dark}"
                );
                let var = name.replace('_', "-");
                assert!(
                    css.contains(&format!("--{var}: {value};")),
                    "{name} missing its custom property in dark={dark}"
                );
            }
        }
    }

    /// The light and dark sets must cover the same names, or switching the
    /// colour scheme would leave a colour stuck at the other mode's value:
    /// the provider is reloaded wholesale, so a name absent from one set is
    /// simply not overridden there.
    #[test]
    fn light_and_dark_define_the_same_names() {
        let light: Vec<_> = LIGHT.iter().map(|(n, _)| *n).collect();
        let dark: Vec<_> = DARK.iter().map(|(n, _)| *n).collect();
        assert_eq!(light, dark);
    }

    /// Relative luminance, for ordering surfaces by lightness.
    fn lum(hex: &str) -> f64 {
        let h = hex.trim_start_matches('#');
        let channel = |i: usize| {
            let v = u8::from_str_radix(&h[i..i + 2], 16).unwrap() as f64 / 255.0;
            if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * channel(0) + 0.7152 * channel(2) + 0.0722 * channel(4)
    }

    /// The sidebar has to be separable from the editor beside it, and the
    /// card from the sidebar behind it. Adwaita gets that from three distinct
    /// surface values ordered sidebar < window < view on light and reversed
    /// on dark; equal values would erase the panel edge, which is exactly the
    /// mistake a hand-picked ramp invites.
    #[test]
    fn surfaces_step_the_way_adwaita_steps_them() {
        let find = |set: &[Color], name: &str| set.iter().find(|(n, _)| *n == name).unwrap().1;
        let (sidebar, window, view) = (
            lum(find(LIGHT, "sidebar_bg_color")),
            lum(find(LIGHT, "window_bg_color")),
            lum(find(LIGHT, "view_bg_color")),
        );
        assert!(sidebar < window, "light sidebar must recede behind the editor");
        assert!(window < view, "light card must come forward of the editor");

        let (sidebar, window, view) = (
            lum(find(DARK, "sidebar_bg_color")),
            lum(find(DARK, "window_bg_color")),
            lum(find(DARK, "view_bg_color")),
        );
        assert!(sidebar > window, "dark sidebar must lift above the editor");
        assert!(window > view, "dark card must sit below the editor");
    }

    /// Contrast of `fg` on `bg`, WCAG 2.1 relative luminance.
    fn contrast(fg: &str, bg: &str) -> f64 {
        fn luminance(hex: &str) -> f64 {
            let h = hex.trim_start_matches('#');
            let channel = |i: usize| {
                let v = u8::from_str_radix(&h[i..i + 2], 16).unwrap() as f64 / 255.0;
                if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
            };
            0.2126 * channel(0) + 0.7152 * channel(2) + 0.0722 * channel(4)
        }
        let (a, b) = (luminance(fg), luminance(bg));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    /// Every tag colour is drawn as text on the view surface, so every one of
    /// them has to clear AA there. This is the guard on the palette: the four
    /// base tones do not pass, which is why the sets above are derived rather
    /// than copied straight out of the icon.
    #[test]
    fn every_bindle_tag_color_clears_aa_on_its_surface() {
        for (dark, bg) in [(false, "#FBF5EA"), (true, "#251A14")] {
            let t = tags(Theme::Bindle, dark);
            let checked = [
                ("heading", t.heading),
                ("marker", t.marker),
                ("code", t.code),
                ("link", t.link),
                ("url", t.url),
                ("key", t.key),
                ("string", t.string),
                ("number", t.number),
                ("boolean", t.boolean),
                ("comment", t.comment),
                ("section", t.section),
                ("dim", t.dim),
            ];
            for (name, color) in checked {
                let ratio = contrast(color, bg);
                assert!(ratio >= 4.5, "{name} {color} on {bg} is only {ratio:.2}:1");
            }
        }
    }

    /// Body text on the two chrome surfaces, and the label drawn on an accent
    /// fill. Fills are large-text/UI targets, so 3:1 is the bar there.
    #[test]
    fn chrome_and_accent_pairings_are_legible() {
        let find = |set: &[Color], name: &str| {
            set.iter().find(|(n, _)| *n == name).unwrap().1
        };
        for set in [LIGHT, DARK] {
            for surface in ["window", "view", "headerbar", "card", "dialog", "popover"] {
                let bg = find(set, &format!("{surface}_bg_color"));
                let fg = find(set, &format!("{surface}_fg_color"));
                let ratio = contrast(fg, bg);
                assert!(ratio >= 4.5, "{surface}: {fg} on {bg} is only {ratio:.2}:1");
            }
            let ratio = contrast(find(set, "accent_fg_color"), find(set, "accent_bg_color"));
            assert!(ratio >= 3.0, "accent fill is only {ratio:.2}:1");
        }
    }
}
