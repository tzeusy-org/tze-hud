//! Search ranking: typing in the panel turns the body into one ranked list of
//! matching rows across every section (CP-3).

use super::row::{Control, Row};

/// The text a row can be found by.
pub(super) struct Haystack<'a> {
    pub label: &'a str,
    pub section: &'a str,
    pub detail: Option<&'a str>,
    pub value: Option<&'a str>,
    pub keywords: &'a [&'a str],
}

impl<'a> Haystack<'a> {
    pub(super) fn of(row: &'a Row, section: &'a str) -> Self {
        let value = match &row.control {
            Control::ReadOnly { text, .. } | Control::Stepper { text } => Some(text.as_str()),
            Control::Choice { options, selected } => options.get(*selected).map(String::as_str),
            Control::Toggle(_)
            | Control::Grant { .. }
            | Control::Disclosure { .. }
            | Control::Action { .. }
            | Control::Field { .. } => None,
        };
        Self {
            label: &row.label,
            section,
            detail: row.detail.as_deref(),
            value,
            keywords: row.keywords,
        }
    }
}

/// True when `needle` starts `hay` or one of its words (a word starts after a
/// non-alphanumeric character, so `.` and `-` in config keys split too).
fn starts_word(hay: &str, needle: &str) -> bool {
    hay.match_indices(needle).any(|(i, _)| {
        hay[..i]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric())
    })
}

/// How well one lowercase `term` matches, or `None`.
fn term_score(term: &str, h: &Haystack<'_>) -> Option<u32> {
    let label = h.label.to_lowercase();
    let lower = |s: &str| s.to_lowercase();
    let candidates = [
        (label.starts_with(term), 100),
        (starts_word(&label, term), 80),
        (label.contains(term), 60),
        (h.keywords.iter().any(|k| starts_word(&lower(k), term)), 50),
        (starts_word(&lower(h.section), term), 40),
        (h.value.is_some_and(|v| lower(v).contains(term)), 30),
        (h.detail.is_some_and(|d| lower(d).contains(term)), 20),
    ];
    candidates
        .iter()
        .find(|(hit, _)| *hit)
        .map(|(_, score)| *score)
}

/// Score `query` against a row: every whitespace-separated term must match
/// somewhere; a better match (label prefix over a word in the detail) scores
/// higher. `None` means no match.
pub(super) fn score(query: &str, h: &Haystack<'_>) -> Option<u32> {
    let query = query.to_lowercase();
    let mut total = 0;
    let mut any = false;
    for term in query.split_whitespace() {
        total += term_score(term, h)?;
        any = true;
    }
    any.then_some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hay<'a>(label: &'a str, section: &'a str, keywords: &'a [&'a str]) -> Haystack<'a> {
        Haystack {
            label,
            section,
            detail: Some("Applies now"),
            value: Some("Ctrl+Shift+F12"),
            keywords,
        }
    }

    #[test]
    fn label_prefix_beats_word_beats_substring_beats_section_and_detail() {
        let prefix = score("pa", &hay("Pause chord", "Input", &[])).unwrap();
        let word = score("ch", &hay("Pause chord", "Input", &[])).unwrap();
        let sub = score("use", &hay("Pause chord", "Input", &[])).unwrap();
        let section = score("inp", &hay("Pause chord", "Input", &[])).unwrap();
        let detail = score("now", &hay("Pause chord", "Input", &[])).unwrap();
        assert!(prefix > word && word > sub && sub > section && section > detail);
    }

    #[test]
    fn every_term_must_match_and_case_is_ignored() {
        let h = hay("Pause chord", "Input", &["hotkey", "safe_mode_hotkey"]);
        assert!(score("PAUSE hotkey", &h).is_some());
        assert!(score("pause brightness", &h).is_none());
        assert!(score("   ", &h).is_none(), "blank query matches nothing");
        assert!(
            score("mode", &h).is_some(),
            "config-key words split on _ and ."
        );
        assert!(score("f12", &h).is_some(), "values are searchable");
    }
}
