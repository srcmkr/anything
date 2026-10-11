//! Search input parsing and file name matching.
//!
//! Terms are separated by whitespace and must all match (AND), ignoring case:
//!
//! - A term without `*` matches anywhere in the name: `rechnung` finds
//!   `Rechnung-123.pdf`, `123-Rechnung.pdf` and `Sammelrechnung.pdf`.
//! - A term with `*` must match the whole name, `*` standing for any run of
//!   characters: `rechnung*.pdf` finds `Rechnung-123.pdf` but not
//!   `123-Rechnung.pdf`, and `*.pdf` finds every name ending in `.pdf`.

use memchr::memmem::Finder;
use std::ops::Range;

// A query holds a handful of terms; their size does not matter.
#[allow(clippy::large_enum_variant)]
enum Term {
    Contains(Finder<'static>),
    Pattern {
        prefix: Vec<u8>,
        middle: Vec<Finder<'static>>,
        suffix: Vec<u8>,
    },
}

impl Term {
    fn parse(term: &str) -> Self {
        let folded = term.to_lowercase();
        if !folded.contains('*') {
            return Term::Contains(Finder::new(folded.as_bytes()).into_owned());
        }
        let mut parts: Vec<&str> = folded.split('*').collect();
        let suffix = parts.pop().unwrap_or_default().as_bytes().to_vec();
        let prefix = parts.remove(0).as_bytes().to_vec();
        let middle = parts
            .into_iter()
            .filter(|p| !p.is_empty())
            .map(|p| Finder::new(p.as_bytes()).into_owned())
            .collect();
        Term::Pattern { prefix, middle, suffix }
    }

    /// Byte ranges of `folded` that the term matched, or `None`.
    fn find(&self, folded: &[u8], ranges: Option<&mut Vec<Range<usize>>>) -> bool {
        match self {
            Term::Contains(finder) => {
                if finder.needle().is_empty() {
                    return true;
                }
                match ranges {
                    None => finder.find(folded).is_some(),
                    Some(ranges) => {
                        let len = finder.needle().len();
                        let before = ranges.len();
                        let mut pos = 0;
                        while let Some(at) = finder.find(&folded[pos..]) {
                            ranges.push(pos + at..pos + at + len);
                            pos += at + len;
                        }
                        ranges.len() > before
                    }
                }
            }
            Term::Pattern { prefix, middle, suffix } => {
                if folded.len() < prefix.len() + suffix.len()
                    || !folded.starts_with(prefix)
                    || !folded.ends_with(suffix)
                {
                    return false;
                }
                let end = folded.len() - suffix.len();
                let mut pos = prefix.len();
                let mut found = Vec::with_capacity(middle.len());
                for finder in middle {
                    let at = match finder.find(&folded[pos..end]) {
                        Some(at) => pos + at,
                        None => return false,
                    };
                    pos = at + finder.needle().len();
                    found.push(at..pos);
                }
                if let Some(ranges) = ranges {
                    if !prefix.is_empty() {
                        ranges.push(0..prefix.len());
                    }
                    ranges.extend(found);
                    if !suffix.is_empty() {
                        ranges.push(end..folded.len());
                    }
                }
                true
            }
        }
    }
}

/// Parsed search input. See the module docs for the rules.
pub struct Query {
    terms: Vec<Term>,
}

impl Query {
    pub fn parse(input: &str) -> Self {
        Self { terms: input.split_whitespace().map(Term::parse).collect() }
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    pub(crate) fn matches(&self, folded_name: &[u8]) -> bool {
        self.terms.iter().all(|t| t.find(folded_name, None))
    }

    /// Byte ranges of `name` to highlight, sorted and merged. Empty when the
    /// name does not match.
    pub fn highlights(&self, name: &str) -> Vec<Range<usize>> {
        // Fold char by char and remember where each folded byte came from.
        let mut folded = String::with_capacity(name.len());
        let mut origin = Vec::with_capacity(name.len() + 1);
        for (at, c) in name.char_indices() {
            for lower in c.to_lowercase() {
                let start = folded.len();
                folded.push(lower);
                origin.extend(std::iter::repeat_n(at, folded.len() - start));
            }
        }
        origin.push(name.len());

        let mut ranges = Vec::new();
        for term in &self.terms {
            if !term.find(folded.as_bytes(), Some(&mut ranges)) {
                return Vec::new();
            }
        }
        // Map to the original name; an end maps to the start of the next char.
        let to_name = |folded_at: usize| {
            if folded_at >= folded.len() {
                return name.len();
            }
            let at = origin[folded_at];
            if folded_at > 0 && origin[folded_at - 1] == at {
                // Inside a char that folded to several bytes or chars.
                name[at..].chars().next().map_or(name.len(), |c| at + c.len_utf8())
            } else {
                at
            }
        };
        let mut ranges: Vec<Range<usize>> = ranges
            .into_iter()
            .map(|r| origin[r.start]..to_name(r.end))
            .filter(|r| !r.is_empty())
            .collect();
        ranges.sort_by_key(|r| r.start);
        let mut merged: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
        for r in ranges {
            match merged.last_mut() {
                Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
                _ => merged.push(r),
            }
        }
        merged
    }
}

#[cfg(test)]
// Highlights are lists of ranges, also when there is one.
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::Query;

    fn hit(query: &str, name: &str) -> bool {
        Query::parse(query).matches(name.to_lowercase().as_bytes())
    }

    #[test]
    fn extension_pattern() {
        assert!(hit("*.pdf", "Rechnung-123.pdf"));
        assert!(hit("*.PDF", "scan.Pdf"));
        assert!(!hit("*.pdf", "notes.pdf.txt"));
        assert!(!hit("*.pdf", "pdf"));
    }

    #[test]
    fn prefix_pattern() {
        assert!(hit("Rechnung*.pdf", "Rechnung-123.pdf"));
        assert!(hit("rechnung*.pdf", "Rechnungen.pdf"));
        assert!(!hit("Rechnung*.pdf", "123-Rechnung.pdf"));
        assert!(!hit("Rechnung*.pdf", "Rechnung-123.pdf.zip"));
        assert!(hit("re*n*g.pdf", "Rechnung.pdf"));
        assert!(!hit("ab*ba", "aba"));
        assert!(hit("*", "anything"));
    }

    #[test]
    fn word_anywhere() {
        for name in ["Rechnung-123.pdf", "123-Rechnung.pdf", "Sammelrechnung.pdf", "Rechnungen.pdf"] {
            assert!(hit("Rechnung", name), "{name}");
        }
        assert!(!hit("Rechnung", "Rechnen.pdf"));
    }

    #[test]
    fn all_terms_must_match() {
        assert!(hit("rechnung *.pdf", "Sammelrechnung_2024.pdf"));
        assert!(!hit("rechnung *.pdf", "Sammelrechnung_2024.odt"));
        assert!(hit("ÜBER", "Übersicht.md"));
    }

    #[test]
    fn highlights_map_back_to_the_name() {
        let q = Query::parse("rechnung");
        assert_eq!(q.highlights("Sammelrechnung-Rechnung.pdf"), [6..14, 15..23]);
        let q = Query::parse("Rechnung*.pdf");
        assert_eq!(q.highlights("Rechnung-123.pdf"), [0..8, 12..16]);
        let q = Query::parse("über");
        assert_eq!(q.highlights("Die Übersicht"), [4..9]);
        let q = Query::parse("i̇");
        // 'İ' folds to "i̇" (two chars); highlighting covers the whole char.
        assert_eq!(q.highlights("İstanbul"), [0..2]);
        assert!(Query::parse("zzz").highlights("abc").is_empty());
    }
}
