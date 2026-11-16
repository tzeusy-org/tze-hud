//! SVG element ID scanner for widget bundle binding resolution.
//!
//! At widget type registration time, bindings are validated by checking that
//! the referenced `target_element` (SVG element ID) exists in the referenced
//! SVG file.  This module scans an SVG document and collects all element IDs.
//!

use std::collections::HashSet;

use quick_xml::Reader;
use quick_xml::events::Event;

/// Collect all element IDs (`id` attribute values) from an SVG document.
///
/// Only elements with an `id` attribute are returned.  The SVG must already
/// have been validated as well-formed XML with an `<svg>` root — this function
/// performs a structural scan only.
///
/// # Errors
///
/// Returns an error string if the XML cannot be parsed.  Callers should convert
/// this into [`crate::error::BundleError::SvgParseError`].
pub fn collect_svg_element_ids(svg_text: &str) -> Result<HashSet<String>, String> {
    let mut ids = HashSet::new();
    let mut reader = Reader::from_str(svg_text);
    reader.config_mut().trim_text(true);

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                for attr in e.attributes().flatten() {
                    let local_name = attr.key.local_name();
                    let key = std::str::from_utf8(local_name.as_ref()).unwrap_or("");
                    if key == "id" {
                        if let Ok(val) = std::str::from_utf8(attr.value.as_ref()) {
                            let id = val.trim().to_string();
                            if !id.is_empty() {
                                ids.insert(id);
                            }
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("XML parse error: {e}")),
            _ => {}
        }
    }

    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collects_ids_from_elements() {
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg">
            <rect id="bar" width="100" height="200"/>
            <circle id="indicator" cx="50" cy="50" r="10"/>
            <g id="layer1"><text id="label">hello</text></g>
        </svg>"#;
        let ids = collect_svg_element_ids(svg).unwrap();
        assert!(ids.contains("bar"), "expected 'bar' in {ids:?}");
        assert!(ids.contains("indicator"));
        assert!(ids.contains("layer1"));
        assert!(ids.contains("label"));
    }

    #[test]
    fn elements_without_id_not_included() {
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg">
            <rect width="100" height="200"/>
            <rect id="only_one" x="0" y="0"/>
        </svg>"#;
        let ids = collect_svg_element_ids(svg).unwrap();
        assert_eq!(ids.len(), 1);
        assert!(ids.contains("only_one"));
    }

    #[test]
    fn invalid_xml_returns_error() {
        let bad = "not xml at all <<<<<";
        assert!(collect_svg_element_ids(bad).is_err());
    }
}
