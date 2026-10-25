//! Fast query parser for file search
//!
//! This parser takes a search query and extracts structured constraints
//! while preserving text for fuzzy matching. Designed for maximum performance:
//! - Single-pass parsing with minimal branching
//! - Stack-allocated string buffers

mod config;
mod constraints;
pub mod glob_detect;
pub mod location;
mod parser;

pub use config::{FileSearchConfig, GrepConfig};
pub use constraints::Constraint;
pub use parser::{FFFQuery, FuzzyQuery, QueryParser};

pub type ConstraintVec<'a> = Vec<Constraint<'a>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_query() {
        let parser = QueryParser::default();
        let result = parser.parse("");
        assert_eq!(result.constraints, []);
        assert_eq!(result.fuzzy_query, FuzzyQuery::Empty);
    }

    #[test]
    fn test_whitespace_only() {
        let parser = QueryParser::default();
        let result = parser.parse("   ");
        assert_eq!(result.constraints, []);
        assert_eq!(result.fuzzy_query, FuzzyQuery::Empty);
    }

    #[test]
    fn test_single_token() {
        let parser = QueryParser::default();
        let result = parser.parse("hello");
        assert_eq!(result.constraints, []);
        assert_eq!(result.fuzzy_query, FuzzyQuery::Text("hello"));
    }

    #[test]
    fn test_simple_text() {
        let parser = QueryParser::default();
        let result = parser.parse("hello world");

        match &result.fuzzy_query {
            FuzzyQuery::Parts(parts) => {
                assert_eq!(parts.len(), 2);
                assert_eq!(parts[0], "hello");
                assert_eq!(parts[1], "world");
            }
            _ => panic!("Expected Parts fuzzy query"),
        }

        assert_eq!(result.constraints.len(), 0);
    }

    #[test]
    fn test_extension_only() {
        let parser = QueryParser::default();
        // Single constraint token - returns Some so constraint can be applied
        let result = parser.parse("*.rs");
        assert!(matches!(result.fuzzy_query, FuzzyQuery::Empty));
        assert_eq!(result.constraints.len(), 1);
        assert!(matches!(result.constraints[0], Constraint::Extension("rs")));
    }

    #[test]
    fn test_glob_pattern() {
        let parser = QueryParser::default();
        let result = parser.parse("**/*.rs foo");
        assert_eq!(result.constraints.len(), 1);
        // Glob patterns with ** are treated as globs, not extensions
        match &result.constraints[0] {
            Constraint::Glob(pattern) => assert_eq!(*pattern, "**/*.rs"),
            other => panic!("Expected Glob constraint, got {other:?}"),
        }
    }

    #[test]
    fn test_negation_pattern() {
        let parser = QueryParser::default();
        let result = parser.parse("!test foo");
        assert_eq!(result.constraints.len(), 1);
        match &result.constraints[0] {
            Constraint::Not(inner) => {
                assert!(matches!(**inner, Constraint::Text("test")));
            }
            _ => panic!("Expected Not constraint"),
        }
    }

    #[test]
    fn test_path_segment() {
        let parser = QueryParser::default();
        let result = parser.parse("/src/ foo");
        assert_eq!(result.constraints.len(), 1);
        assert!(matches!(
            result.constraints[0],
            Constraint::PathSegment("src")
        ));
    }

    #[test]
    fn test_file_type() {
        let parser = QueryParser::default();
        let result = parser.parse("type:rust foo");
        assert_eq!(result.constraints.len(), 1);
        assert!(matches!(
            result.constraints[0],
            Constraint::FileType("rust")
        ));
    }

    #[test]
    fn test_complex_query() {
        let parser = QueryParser::default();
        let result = parser.parse("src name *.rs !test /lib/");

        // Verify we have fuzzy text
        match &result.fuzzy_query {
            FuzzyQuery::Parts(parts) => {
                assert_eq!(parts.len(), 2);
                assert_eq!(parts[0], "src");
                assert_eq!(parts[1], "name");
            }
            _ => panic!("Expected Parts fuzzy query"),
        }

        // Should have multiple constraints
        assert!(result.constraints.len() >= 3);

        // Verify specific constraints exist
        let has_extension = result
            .constraints
            .iter()
            .any(|c| matches!(c, Constraint::Extension("rs")));
        let has_not = result
            .constraints
            .iter()
            .any(|c| matches!(c, Constraint::Not(_)));
        let has_path = result
            .constraints
            .iter()
            .any(|c| matches!(c, Constraint::PathSegment("lib")));

        assert!(has_extension, "Should have Extension constraint");
        assert!(has_not, "Should have Not constraint");
        assert!(has_path, "Should have PathSegment constraint");
    }

    #[test]
    fn test_small_constraint_count() {
        let parser = QueryParser::default();
        let result = parser.parse("*.rs *.toml !test");
        assert_eq!(result.constraints.len(), 3);
    }

    #[test]
    fn test_many_fuzzy_parts() {
        let parser = QueryParser::default();
        let result = parser.parse("one two three four five six");

        match &result.fuzzy_query {
            FuzzyQuery::Parts(parts) => {
                assert_eq!(parts.len(), 6);
                assert_eq!(parts[0], "one");
                assert_eq!(parts[5], "six");
            }
            _ => panic!("Expected Parts fuzzy query"),
        }
    }
}
