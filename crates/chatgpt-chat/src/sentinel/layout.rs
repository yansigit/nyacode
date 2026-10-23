//! Small native layout environment, independent of the host desktop.
//!
//! Only directly attached, absolutely/fixed positioned text boxes are defined.
//! Fonts are immutable program assets. No system font discovery, CSS loading,
//! HTML parsing, network access or script execution is available here.

use super::Budget;
use super::Result;
use super::SentinelFailure as Error;
use super::profile::SCREEN_HEIGHT;
use super::profile::SCREEN_WIDTH;
use super::style::InlineStyle;
use parley::FontContext;
use parley::LayoutContext;
use parley::LineHeight;
use parley::StyleProperty;
use parley::fontique::Blob;
use parley::fontique::Collection;
use parley::fontique::CollectionOptions;
use parley::fontique::GenericFamily;
use parley::fontique::SourceCache;
use std::sync::Arc;

const FONTS: [(&str, GenericFamily, &[u8]); 3] = [
    (
        "DejaVu Sans",
        GenericFamily::SansSerif,
        include_bytes!("../../fonts/DejaVuSans.ttf"),
    ),
    (
        "DejaVu Sans Mono",
        GenericFamily::Monospace,
        include_bytes!("../../fonts/DejaVuSansMono.ttf"),
    ),
    (
        "DejaVu Serif",
        GenericFamily::Serif,
        include_bytes!("../../fonts/DejaVuSerif.ttf"),
    ),
];
// Includes the 1.42 MiB immutable profile and more than 256 KiB of private
// collection/cache state (the three-face initialization measured 4,424 bytes).
// Shaping buffers have a separate peak reservation below.
const FONT_RESERVATION: usize = 2 * 1024 * 1024;
const _: () =
    assert!(FONTS[0].2.len() + FONTS[1].2.len() + FONTS[2].2.len() + 256 * 1024 < FONT_RESERVATION);
const MEASUREMENT_RESERVATION: usize = 2 * 1024 * 1024;
const TEXT_LIMIT: usize = 512;

fn unsupported(reason: &'static str) -> Error {
    Error::UnsupportedNativeLayout { reason }
}

fn px(value: &str, default: f64, negative: bool) -> Result<f64> {
    if value.is_empty() || value == "auto" {
        return Ok(default);
    }
    let number: f64 = value
        .strip_suffix("px")
        .ok_or_else(|| unsupported("length_unit"))?
        .parse()
        .map_err(|_| unsupported("length_value"))?;
    if !number.is_finite() || number.abs() > 1_000_000.0 || !negative && number < 0.0 {
        return Err(unsupported("length_range"));
    }
    // Blink's LayoutUnit truncates specified floating point lengths to 1/64 px.
    Ok((number * 64.0).trunc() / 64.0)
}

fn families(value: &str) -> String {
    let mut families = Vec::new();
    for item in value.split(',') {
        let item = item.trim();
        let quoted = item.starts_with(['\'', '"']);
        let item = item.trim_matches(['\'', '"']);
        let family = if !quoted && item.eq_ignore_ascii_case("monospace")
            || item.eq_ignore_ascii_case("DejaVu Sans Mono")
        {
            "DejaVu Sans Mono"
        } else if !quoted
            && (item.eq_ignore_ascii_case("sans-serif") || item.eq_ignore_ascii_case("system-ui"))
            || item.eq_ignore_ascii_case("DejaVu Sans")
        {
            "DejaVu Sans"
        } else if !quoted && item.eq_ignore_ascii_case("serif")
            || item.eq_ignore_ascii_case("DejaVu Serif")
        {
            "DejaVu Serif"
        } else {
            continue;
        };
        if !families.contains(&family) {
            families.push(family);
        }
    }
    // Preserve the specified list's order. Quoted generic names are literal
    // names, and are unavailable in this profile. Then apply the declared
    // native fallback chain, which never discovers fonts on the host.
    if families.is_empty() {
        families.push("DejaVu Serif");
    }
    for family in ["DejaVu Sans", "DejaVu Serif", "DejaVu Sans Mono"] {
        if !families.contains(&family) {
            families.push(family);
        }
    }
    families
        .iter()
        .map(|name| format!("\"{name}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Default)]
pub(super) struct NativeLayout {
    fonts: Option<FontContext>,
}

impl NativeLayout {
    pub fn rect(
        &mut self,
        style: &InlineStyle,
        text: &str,
        budget: &mut Budget,
    ) -> Result<[f64; 4]> {
        budget.check()?;
        if style.get("display")? == "none" {
            return Ok([0.0; 4]);
        }
        if !matches!(style.get("position")?, "absolute" | "fixed") {
            return Err(unsupported("flow_position"));
        }
        if !matches!(
            style.get("display")?,
            "" | "block" | "inline" | "inline-block"
        ) {
            return Err(unsupported("display_mode"));
        }
        if !matches!(style.get("transform")?, "" | "none") {
            return Err(unsupported("transform"));
        }
        if !matches!(style.get("cssFloat")?, "" | "none") {
            return Err(unsupported("float"));
        }
        if !style.get("right")?.is_empty() || !style.get("bottom")?.is_empty() {
            return Err(unsupported("opposing_insets"));
        }
        if text.len() > TEXT_LIMIT {
            return Err(unsupported("text_length"));
        }
        if !text
            .chars()
            .all(|c| !c.is_control() || matches!(c, '\t' | '\n' | '\r' | '\x0c'))
        {
            return Err(unsupported("control_text"));
        }
        budget.work(text.len().saturating_add(1), 0)?;
        let size = px(style.get("fontSize")?, 16.0, false)?;
        if size > 512.0 {
            return Err(unsupported("font_size"));
        }
        let x = px(style.get("left")?, 8.0, true)?;
        let y = px(style.get("top")?, 8.0, true)?;
        let explicit_width = style.get("width")?;
        let explicit_height = style.get("height")?;
        // Reserve peak native scratch before normalizing or entering the shaper.
        // The font cache holds only the three immutable faces. Each measurement
        // owns and drops its shaping/layout buffers; nothing accumulates there.
        if self.fonts.is_none() {
            budget.allocate(FONT_RESERVATION)?;
            let mut fonts = FontContext {
                source_cache: SourceCache::new_shared(),
                collection: Collection::new(CollectionOptions {
                    shared: false,
                    system_fonts: false,
                }),
            };
            for (_, generic, bytes) in FONTS {
                let registered = fonts
                    .collection
                    .register_fonts(Blob::new(Arc::new(bytes) as _), None);
                if registered.len() != 1 {
                    return Err(unsupported("font_profile"));
                }
                fonts
                    .collection
                    .append_generic_families(generic, registered.iter().map(|(id, _)| *id));
            }
            self.fonts = Some(fonts);
            budget.check()?;
        }
        budget.allocate(MEASUREMENT_RESERVATION)?;
        let result = self.measure(style, text, size, x, y, explicit_width, explicit_height);
        budget.allocated -= MEASUREMENT_RESERVATION;
        budget.check()?;
        result
    }

    #[expect(clippy::too_many_arguments)]
    fn measure(
        &mut self,
        style: &InlineStyle,
        text: &str,
        size: f64,
        x: f64,
        y: f64,
        explicit_width: &str,
        explicit_height: &str,
    ) -> Result<[f64; 4]> {
        let text = text.split_ascii_whitespace().collect::<Vec<_>>().join(" ");
        let mut context = LayoutContext::<[u8; 4]>::new();
        let fonts = self
            .fonts
            .as_mut()
            .ok_or_else(|| unsupported("font_profile"))?;
        let mut builder = context.ranged_builder(fonts, &text, 1.0, true);
        let families = families(style.get("fontFamily")?);
        builder.push_default(StyleProperty::FontFamily(families.as_str().into()));
        builder.push_default(StyleProperty::FontSize(size as f32));
        builder.push_default(StyleProperty::LineHeight(LineHeight::NORMAL));
        let mut layout = builder.build(&text);
        layout.break_all_lines(None);
        if layout.lines().count() > 1 {
            return Err(unsupported("text_wrapping"));
        }
        if layout
            .lines()
            .flat_map(|line| line.runs())
            .flat_map(|run| run.clusters())
            .flat_map(|cluster| cluster.glyphs())
            .any(|glyph| glyph.id == 0)
        {
            // No fallback face is available in the declared profile. A missing
            // glyph's replacement box is not evidence of the required glyph.
            return Err(unsupported("font_coverage"));
        }
        let natural_width = if text.is_empty() {
            0.0
        } else {
            (f64::from(layout.width()) * 64.0).ceil() / 64.0
        };
        let natural_height = if text.is_empty() {
            0.0
        } else {
            f64::from(layout.height())
        };
        let width = px(explicit_width, natural_width, false)?;
        let height = px(explicit_height, natural_height, false)?;
        // Multi-line layout needs additional CSS line-breaking contracts. Do
        // not return a one-line height when the containing box would wrap.
        if natural_width > width
            || (explicit_width.is_empty() || explicit_width == "auto")
                && natural_width > f64::from(SCREEN_WIDTH) - x
        {
            return Err(unsupported("text_wrapping"));
        }
        if width > f64::from(SCREEN_WIDTH) * 4.0 || height > f64::from(SCREEN_HEIGHT) * 4.0 {
            return Err(unsupported("box_range"));
        }
        Ok([x, y, width, height])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sentinel::Limits;
    use tokio_util::sync::CancellationToken;

    fn budget() -> Budget {
        Budget::new(Limits::default(), CancellationToken::new())
    }

    fn style(budget: &mut Budget) -> InlineStyle {
        let mut style = InlineStyle::default();
        for (key, value) in [
            ("position", "absolute"),
            ("left", "10px"),
            ("top", "20px"),
            ("fontFamily", "DejaVu Sans"),
            ("fontSize", "16px"),
        ] {
            style.set(key, value, budget).unwrap();
        }
        style
    }

    #[test]
    fn native_shaping_matches_same_font_browser_fixtures() {
        let mut budget = budget();
        let mut style = style(&mut budget);
        let mut renderer = NativeLayout::default();
        for (size, width, height) in [
            ("16px", 234.25, 19.0),
            ("13.25px", 194.0, 15.0),
            ("72px", 1054.125, 84.0),
            ("0px", 0.0, 0.0),
        ] {
            style.set("fontSize", size, &mut budget).unwrap();
            assert_eq!(
                renderer
                    .rect(&style, "Native layout 0123456789 ffi", &mut budget)
                    .unwrap(),
                [10.0, 20.0, width, height]
            );
        }
        style.set("fontSize", "16px", &mut budget).unwrap();
        style.set("left", "10.2px", &mut budget).unwrap();
        style.set("top", "20.3px", &mut budget).unwrap();
        assert_eq!(
            renderer.rect(&style, "Native layout", &mut budget).unwrap(),
            [10.1875, 20.296875, 106.8125, 19.0]
        );
        assert_eq!(
            renderer.rect(&style, "", &mut budget).unwrap(),
            [10.1875, 20.296875, 0.0, 0.0]
        );
        style.set("display", "none", &mut budget).unwrap();
        assert_eq!(
            renderer.rect(&style, "Native layout", &mut budget).unwrap(),
            [0.0; 4]
        );
    }

    #[test]
    fn profile_families_and_unicode_match_browser_font_fixtures() {
        let mut budget = budget();
        let mut style = style(&mut budget);
        let mut renderer = NativeLayout::default();
        for (family, size, width, height) in [
            ("DejaVu Sans", "16px", 234.25, 19.0),
            ("DejaVu Sans", "72px", 1054.125, 84.0),
            ("DejaVu Sans Mono", "16px", 269.71875, 19.0),
            ("DejaVu Sans Mono", "72px", 1213.734375, 84.0),
            ("DejaVu Serif", "16px", 237.203125, 19.0),
            ("DejaVu Serif", "72px", 1067.390625, 84.0),
            ("\"monospace\"", "16px", 237.203125, 19.0),
            ("monospace", "16px", 269.71875, 19.0),
        ] {
            style.set("fontFamily", family, &mut budget).unwrap();
            style.set("fontSize", size, &mut budget).unwrap();
            assert_eq!(
                renderer
                    .rect(&style, "Native layout 0123456789 ffi", &mut budget)
                    .unwrap(),
                [10.0, 20.0, width, height]
            );
        }
        style.set("fontFamily", "DejaVu Sans", &mut budget).unwrap();
        style.set("fontSize", "16px", &mut budget).unwrap();
        for (text, width) in [
            ("éΩЖ", 39.3125),
            ("A\u{301}", 10.953125),
            ("مرحبا", 37.0),
            ("😀", 16.6875),
        ] {
            assert_eq!(
                renderer.rect(&style, text, &mut budget).unwrap(),
                [10.0, 20.0, width, 19.0]
            );
        }
    }

    #[test]
    fn layout_rejects_unavailable_semantics_and_checks_resources() {
        let mut budget = budget();
        let mut style = style(&mut budget);
        let mut renderer = NativeLayout::default();
        assert!(matches!(
            renderer.rect(&style, "\u{10ffff}", &mut budget),
            Err(Error::UnsupportedNativeLayout {
                reason: "font_coverage"
            })
        ));
        style.set("width", "20px", &mut budget).unwrap();
        assert_eq!(
            renderer.rect(&style, "Native layout", &mut budget),
            Err(unsupported("text_wrapping"))
        );
        let allocated = budget.allocated;
        assert_eq!(
            renderer.rect(&style, "Native layout", &mut budget),
            Err(unsupported("text_wrapping"))
        );
        assert_eq!(budget.allocated, allocated);
        budget.cancellation.cancel();
        assert_eq!(
            renderer.rect(&style, "Native layout", &mut budget),
            Err(Error::Cancelled)
        );
        let mut budget = Budget::new(
            Limits {
                allocation_bytes: 1024,
                ..Limits::default()
            },
            CancellationToken::new(),
        );
        assert_eq!(
            NativeLayout::default().rect(&style, "x", &mut budget),
            Err(Error::AllocationLimit)
        );
    }
}
