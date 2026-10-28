//! Reducing a natural-language question to search terms.

const STOPWORDS: &[&str] = &[
    "the",
    "and",
    "for",
    "with",
    "from",
    "that",
    "this",
    "when",
    "into",
    "make",
    "fix",
    "fixes",
    "fixed",
    "add",
    "adds",
    "added",
    "use",
    "uses",
    "using",
    "update",
    "updates",
    "remove",
    "removes",
    "support",
    "instead",
    "than",
    "have",
    "has",
    "should",
    "does",
    "not",
    "are",
    "was",
    "now",
    "also",
    "some",
    "more",
    "bump",
    "release",
    "how",
    "what",
    "where",
    "which",
    "who",
    "why",
    "can",
    "could",
    "would",
    "will",
    "get",
    "set",
    "its",
    "our",
    "your",
    "their",
    "they",
    "them",
    "you",
    "all",
    "any",
    "one",
    "two",
    "new",
    "old",
    "code",
    "file",
    "files",
    "function",
    "where",
    "find",
    "show",
    "need",
    "want",
    "like",
    "just",
    "then",
    "there",
    "here",
    "about",
    "over",
    "under",
    "between",
    "while",
    "after",
    "before",
    "work",
    "works",
    "working",
    "do",
    "implement",
    "implemented",
    "implementation",
    "handle",
    "handles",
    "handling",
];
const MAX_TERMS: usize = 8;

/// A search term derived from the question.
#[derive(Clone, Debug, PartialEq)]
pub struct Term {
    /// Lowercased, used for case-insensitive text search.
    pub text: String,
    /// As written in the question, used for case-sensitive symbol lookup.
    pub original: String,
    /// Identifier-shaped terms (`snake_case`, `CamelCase`) are likelier to be real names.
    pub weight: f32,
}

fn is_identifier_shaped(w: &str) -> bool {
    w.contains('_')
        || (w.chars().any(char::is_uppercase)
            && w.chars().skip(1).any(char::is_lowercase)
            && w.chars().any(char::is_alphabetic)
            && w.chars().skip(1).any(char::is_uppercase))
}

/// Words of 3+ characters minus stopwords, deduplicated in order of appearance, capped.
pub fn query_terms(query: &str) -> Vec<Term> {
    let mut out: Vec<Term> = Vec::new();
    for word in query.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        if word.chars().count() < 3 {
            continue;
        }
        let lower = word.to_lowercase();
        if STOPWORDS.contains(&lower.as_str()) || out.iter().any(|t| t.text == lower) {
            continue;
        }
        let weight = if is_identifier_shaped(word) { 1.5 } else { 1.0 };
        out.push(Term {
            text: lower,
            original: word.to_string(),
            weight,
        });
        if out.len() == MAX_TERMS {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(q: &str) -> Vec<String> {
        query_terms(q).into_iter().map(|t| t.text).collect()
    }

    #[test]
    fn drops_stopwords_short_words_and_duplicates() {
        assert_eq!(
            texts("How do I parse the config file?"),
            ["parse", "config"]
        );
        assert_eq!(
            texts("Fix parser: handle Parser errors"),
            ["parser", "errors"]
        );
        assert!(texts("it is a to of").is_empty());
        assert!(texts("").is_empty());
    }

    #[test]
    fn keeps_identifiers_whole_and_weights_them_up() {
        let t = query_terms("why does tokenize_input fail on HttpServer");
        let by: Vec<(&str, &str, f32)> = t
            .iter()
            .map(|t| (t.text.as_str(), t.original.as_str(), t.weight))
            .collect();
        assert!(
            by.contains(&("tokenize_input", "tokenize_input", 1.5)),
            "{by:?}"
        );
        assert!(by.contains(&("httpserver", "HttpServer", 1.5)), "{by:?}");
        assert!(by.contains(&("fail", "fail", 1.0)));
    }

    #[test]
    fn caps_the_term_count() {
        let q = "alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo";
        assert_eq!(query_terms(q).len(), MAX_TERMS);
    }

    #[test]
    fn plain_capitalised_words_are_not_identifiers() {
        assert_eq!(query_terms("Parser Config")[0].weight, 1.0);
        assert_eq!(query_terms("PARSER")[0].weight, 1.0);
    }
}
