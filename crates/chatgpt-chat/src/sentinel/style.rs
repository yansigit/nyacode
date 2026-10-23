//! Bounded inline declaration state for native, detached elements.
//! No stylesheet loading, cascade, layout, expressions or shorthand expansion.

use super::Budget;
use super::Result;
use super::SentinelFailure as Error;

#[derive(Default)]
pub(super) struct InlineStyle {
    declarations: Vec<(&'static str, String)>,
}

fn property(key: &str) -> Option<&'static str> {
    Some(match key {
        "display" => "display",
        "position" => "position",
        "width" => "width",
        "height" => "height",
        "visibility" => "visibility",
        "opacity" => "opacity",
        "transform" => "transform",
        "left" => "left",
        "top" => "top",
        "right" => "right",
        "bottom" => "bottom",
        "color" => "color",
        "cssFloat" => "float",
        "fontSize" => "font-size",
        "fontFamily" => "font-family",
        _ => return None,
    })
}

// The supported decimal subset round-trips through Chrome's six-significant-
// digit specified-value serializer. Values requiring rounding or exponent
// notation remain explicit failures instead of silently differing from CSSOM.
fn decimal(value: &str, negative: bool) -> Option<String> {
    let (sign, value) = if let Some(value) = value.strip_prefix('-') {
        if !negative {
            return None;
        }
        ("-", value)
    } else {
        ("", value.strip_prefix('+').unwrap_or(value))
    };
    let (integer, fraction) = if let Some((integer, fraction)) = value.split_once('.') {
        if fraction.is_empty() {
            return None;
        }
        (integer, fraction)
    } else {
        (value, "")
    };
    if integer.is_empty() && fraction.is_empty()
        || !integer.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let integer = integer.trim_start_matches('0');
    let fraction = fraction.trim_end_matches('0');
    let digits = format!("{integer}{fraction}");
    let significant = digits.trim_start_matches('0').len();
    if significant > 6
        || integer.len() > 6
        || (integer.is_empty() && !fraction.is_empty() && fraction.starts_with("0000"))
    {
        return None;
    }
    let sign = if significant == 0 { "" } else { sign };
    let integer = if integer.is_empty() { "0" } else { integer };
    Some(if fraction.is_empty() {
        format!("{sign}{integer}")
    } else {
        format!("{sign}{integer}.{fraction}")
    })
}

fn length(value: &str, negative: bool) -> Option<String> {
    if value == "0" {
        return Some("0px".into());
    }
    for unit in ["px", "rem", "em", "vw", "vh", "vmin", "vmax", "%"] {
        if let Some(number) = value.strip_suffix(unit) {
            return decimal(number, negative).map(|number| format!("{number}{unit}"));
        }
    }
    None
}

fn font_family(value: &str) -> Option<String> {
    const GENERIC: &[&str] = &[
        "serif",
        "sans-serif",
        "monospace",
        "cursive",
        "fantasy",
        "system-ui",
        "ui-serif",
        "ui-sans-serif",
        "ui-monospace",
        "ui-rounded",
        "emoji",
        "math",
        "fangsong",
    ];
    const GLOBAL: &[&str] = &[
        "inherit",
        "initial",
        "unset",
        "revert",
        "revert-layer",
        "default",
    ];
    let mut result = Vec::new();
    for item in value.split(',') {
        if result.len() == 8 {
            return None;
        }
        let item = item.trim_ascii();
        let quoted = item.starts_with(['\'', '"']);
        let item = if quoted {
            if item.len() < 2 || item.as_bytes().first() != item.as_bytes().last() {
                return None;
            }
            &item[1..item.len() - 1]
        } else {
            item
        };
        let words: Vec<_> = item.split_ascii_whitespace().collect();
        if words.is_empty()
            || words.len() > 8
            || words.iter().any(|word| {
                word == &"-"
                    || word
                        .strip_prefix('-')
                        .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
                    || !word.starts_with(|c: char| c.is_ascii_alphabetic() || c == '-' || c == '_')
                    || !word
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
            })
        {
            return None;
        }
        let normalized = words.join(" ");
        if quoted && normalized != item {
            return None;
        }
        let lower = normalized.to_ascii_lowercase();
        let generic = GENERIC.contains(&lower.as_str());
        let global = GLOBAL.contains(&lower.as_str());
        if !quoted
            && (global
                || words.len() > 1 && GENERIC.contains(&words[0].to_ascii_lowercase().as_str()))
        {
            return None;
        }
        result.push(if quoted && (generic || global) || words.len() > 1 {
            format!("\"{normalized}\"")
        } else if generic {
            lower
        } else {
            normalized
        });
    }
    Some(result.join(", "))
}

fn parsed_value(property: &'static str, value: &str) -> Result<String> {
    if value.len() > 128 || !value.is_ascii() {
        return Err(Error::UnsupportedStyleValue);
    }
    let value = value.trim_matches([' ', '\t', '\n', '\r', '\x0c']);
    if property == "font-family"
        && let Some(value) = font_family(value)
    {
        return Ok(value);
    }
    let value = value.to_ascii_lowercase();
    if value.is_empty()
        || matches!(
            value.as_str(),
            "inherit" | "initial" | "unset" | "revert" | "revert-layer"
        )
    {
        return Ok(value);
    }
    let keyword = match property {
        "display" => matches!(
            value.as_str(),
            "none"
                | "contents"
                | "block"
                | "inline"
                | "inline-block"
                | "flex"
                | "inline-flex"
                | "grid"
                | "inline-grid"
                | "flow-root"
                | "table"
                | "inline-table"
        ),
        "position" => matches!(
            value.as_str(),
            "static" | "relative" | "absolute" | "fixed" | "sticky"
        ),
        "visibility" => matches!(value.as_str(), "visible" | "hidden" | "collapse"),
        "float" => matches!(value.as_str(), "none" | "left" | "right"),
        "transform" => value == "none",
        "width" | "height" => matches!(
            value.as_str(),
            "auto" | "min-content" | "max-content" | "fit-content"
        ),
        "left" | "top" | "right" | "bottom" => value == "auto",
        "font-size" => matches!(
            value.as_str(),
            "xx-small"
                | "x-small"
                | "small"
                | "medium"
                | "large"
                | "x-large"
                | "xx-large"
                | "smaller"
                | "larger"
        ),
        "font-family" => matches!(
            value.as_str(),
            "serif" | "sans-serif" | "monospace" | "cursive" | "fantasy" | "system-ui"
        ),
        "color" => matches!(
            value.as_str(),
            "black"
                | "white"
                | "red"
                | "green"
                | "blue"
                | "yellow"
                | "transparent"
                | "currentcolor"
        ),
        "opacity" => matches!(value.as_str(), "0" | "1"),
        _ => false,
    };
    if keyword {
        return Ok(value);
    }
    if property == "opacity"
        && let Some(value) = decimal(&value, true)
    {
        return Ok(value);
    }
    let negative = matches!(property, "left" | "top" | "right" | "bottom");
    if (negative || matches!(property, "width" | "height" | "font-size"))
        && let Some(value) = length(&value, negative)
    {
        return Ok(value);
    }
    // Unsupported input is not reported as a successful CSS assignment. In
    // particular, parsing failures never store raw CSS or alter prior values.
    Err(Error::UnsupportedCssValue {
        property,
        syntax: if value.starts_with('#') {
            "hex_color"
        } else if value.contains('(') {
            "function"
        } else if value.contains(',') {
            "list"
        } else if value.contains('.') {
            "decimal"
        } else {
            "keyword_or_dimension"
        },
    })
}

impl InlineStyle {
    pub fn len(&self) -> usize {
        self.declarations.len()
    }

    pub fn get(&self, key: &str) -> Result<&str> {
        // These shorthands stay empty because their constituent properties
        // cannot be assigned through this deliberately small subset.
        if matches!(key, "background" | "overflow") {
            return Ok("");
        }
        let property = property(key).ok_or(Error::UnsupportedStyleProperty)?;
        Ok(self
            .declarations
            .iter()
            .find(|(p, _)| *p == property)
            .map_or("", |(_, v)| v))
    }

    pub fn set(&mut self, key: &str, value: &str, budget: &mut Budget) -> Result<()> {
        budget.step(0)?;
        let property = property(key).ok_or_else(|| {
            if key == "cssText" {
                Error::UnsupportedCssTextMutation
            } else {
                Error::UnsupportedStyleMutation
            }
        })?;
        // Charge bounded parser scratch and the retained value before parsing.
        budget.allocate(2048)?;
        let value = parsed_value(property, value)?;
        if let Some(at) = self.declarations.iter().position(|(p, _)| *p == property) {
            if value.is_empty() {
                self.declarations.remove(at);
            } else {
                self.declarations[at].1 = value;
            }
        } else if !value.is_empty() {
            self.declarations.push((property, value));
        }
        Ok(())
    }

    pub fn css_text(&self, budget: &mut Budget) -> Result<String> {
        budget.step(0)?;
        // CSSOM can serialize all four edges as the `inset` shorthand. That
        // shorthand's choice/order is outside this longhand-only subset.
        if ["left", "top", "right", "bottom"]
            .iter()
            .all(|key| self.declarations.iter().any(|(p, _)| p == key))
        {
            return Err(Error::UnsupportedStyleSerialization);
        }
        budget.allocate(
            self.declarations
                .iter()
                .map(|(k, v)| k.len() + v.len() + 4)
                .sum(),
        )?;
        let mut text = String::new();
        for (key, value) in &self.declarations {
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(key);
            text.push_str(": ");
            text.push_str(value);
            text.push(';');
        }
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sentinel::Limits;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn declarations_preserve_order_replace_remove_and_serialize_known_values() {
        let mut budget = Budget::new(Limits::default(), CancellationToken::new());
        let mut style = InlineStyle::default();
        style.set("width", "010PX", &mut budget).unwrap();
        style.set("position", " absolute ", &mut budget).unwrap();
        style.set("top", "-12px", &mut budget).unwrap();
        style.set("width", "0", &mut budget).unwrap();
        assert_eq!(style.len(), 3);
        assert_eq!(
            style.css_text(&mut budget).unwrap(),
            "width: 0px; position: absolute; top: -12px;"
        );
        style.set("position", "", &mut budget).unwrap();
        assert_eq!(style.len(), 2);
        assert_eq!(style.get("position"), Ok(""));
        assert_eq!(
            style.css_text(&mut budget).unwrap(),
            "width: 0px; top: -12px;"
        );
        assert_eq!(InlineStyle::default().len(), 0);
    }

    #[test]
    fn font_names_preserve_case_and_serialize_literal_families() {
        let mut budget = Budget::new(Limits::default(), CancellationToken::new());
        let mut style = InlineStyle::default();
        for (input, expected) in [
            ("Arial", "Arial"),
            ("ARIAL", "ARIAL"),
            ("Times   New Roman", "\"Times New Roman\""),
            ("DejaVu Serif", "\"DejaVu Serif\""),
            ("Foo inherit", "\"Foo inherit\""),
            ("inherit mono", "\"inherit mono\""),
            ("MONOSPACE", "monospace"),
            ("-apple-system", "-apple-system"),
            ("Arial, sans-serif", "Arial, sans-serif"),
            ("'Arial'", "Arial"),
            ("'serif'", "\"serif\""),
        ] {
            style.set("fontFamily", input, &mut budget).unwrap();
            assert_eq!(style.get("fontFamily"), Ok(expected));
        }
        for input in [
            "serif mono",
            "Arial, inherit",
            "'escaped\\name'",
            "Arial,,serif",
        ] {
            assert!(style.set("fontFamily", input, &mut budget).is_err());
        }
    }

    #[test]
    fn bounded_decimals_preserve_observed_cssom_serialization() {
        let mut budget = Budget::new(Limits::default(), CancellationToken::new());
        let mut style = InlineStyle::default();
        for (key, input, expected) in [
            ("width", "12.5px", "12.5px"),
            ("width", "+0012.500px", "12.5px"),
            ("opacity", ".5", "0.5"),
            ("opacity", "1.5", "1.5"),
            ("opacity", "0.01", "0.01"),
            ("top", "-.25px", "-0.25px"),
            ("left", "-0.00px", "0px"),
        ] {
            style.set(key, input, &mut budget).unwrap();
            assert_eq!(style.get(key), Ok(expected));
        }
        for input in ["0.000001px", "123456.789px", "1000000px"] {
            assert!(style.set("width", input, &mut budget).is_err());
            assert_eq!(style.get("width"), Ok("12.5px"));
        }
    }

    #[test]
    fn unsupported_values_leave_the_previous_declaration_intact() {
        let mut budget = Budget::new(Limits::default(), CancellationToken::new());
        let mut style = InlineStyle::default();
        style.set("width", "100px", &mut budget).unwrap();
        for value in [
            "calc(10px + 2vw)",
            "var(--size)",
            "-10px",
            "1.234567px",
            "!important",
            "9",
            "1000001px",
        ] {
            assert!(matches!(
                style.set("width", value, &mut budget),
                Err(Error::UnsupportedCssValue {
                    property: "width",
                    ..
                })
            ));

            assert_eq!(style.get("width"), Ok("100px"));
        }
        assert_eq!(
            style.set("background", "red", &mut budget),
            Err(Error::UnsupportedStyleMutation)
        );
        assert_eq!(
            style.set("cssText", "width: 2px", &mut budget),
            Err(Error::UnsupportedCssTextMutation)
        );
        assert_eq!(
            style.set("width", &"x".repeat(129), &mut budget),
            Err(Error::UnsupportedStyleValue)
        );
        for key in ["left", "top", "right", "bottom"] {
            style.set(key, "0", &mut budget).unwrap();
        }
        assert_eq!(
            style.css_text(&mut budget),
            Err(Error::UnsupportedStyleSerialization)
        );
    }

    #[test]
    fn cancelled_or_unfunded_mutations_do_not_change_style_state() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut budget = Budget::new(Limits::default(), cancel);
        let mut style = InlineStyle::default();
        assert_eq!(
            style.set("width", "1px", &mut budget),
            Err(Error::Cancelled)
        );
        assert_eq!(style.len(), 0);
        let mut budget = Budget::new(
            Limits {
                allocation_bytes: 511,
                ..Limits::default()
            },
            CancellationToken::new(),
        );
        assert_eq!(
            style.set("width", "1px", &mut budget),
            Err(Error::AllocationLimit)
        );
        assert_eq!(style.len(), 0);
    }
}
