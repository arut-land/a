use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

/// Controls case sensitivity of the grep pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Casing {
    /// Case-insensitive unless the pattern contains an uppercase char.
    #[default]
    Smart,
    Sensitive,
    Insensitive,
}

impl Casing {
    pub fn is_insensitive_for(self, pattern: &str) -> bool {
        match self {
            Casing::Smart => !pattern.chars().any(char::is_uppercase),
            Casing::Insensitive => true,
            Casing::Sensitive => false,
        }
    }
}

/// Controls how the grep pattern is interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GrepMode {
    /// Literal plain text match: default path that doesn't require any regex machinery
    #[default]
    Plain,
    /// Regex mode: uses the same exact matching engine as ripgrep
    Regex,
    /// Smart fuzzy mode, allows user to make either a couple of single char typos or long gaps
    /// e.g. shcema -> shcema, or UserController -> UserAuthController
    Fuzzy,
}

/// A single content match within a file
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GrepMatch {
    /// 1-based line number.
    pub line: u64,
    /// 0-based byte column of first match start within the line.
    pub col: usize,
    /// Absolute byte offset of the matched line from the start of the file.
    #[serde(skip)]
    pub byte_offset: u64,
    /// The matched line text, truncated to `MAX_LINE_DISPLAY_LEN`.
    pub text: String,
    /// Byte offsets `(start, end)` within `text` for each match.
    pub ranges: SmallVec<[(u32, u32); 4]>,
    /// Fuzzy match score from neo_frizbee (only set in Fuzzy grep mode).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<u16>,
    /// Lines before the match (for context display). Empty when context is 0.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub before: Vec<String>,
    /// Lines after the match (for context display). Empty when context is 0.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<String>,
}

/// How a grep reads its pattern and what it keeps of each match.
#[derive(Debug, Clone, Copy)]
pub struct GrepOptions {
    pub mode: GrepMode,
    pub casing: Casing,
    /// The most matches kept from one file; 0 for every one.
    pub max_matches_per_file: usize,
    /// Number of context lines to include before each match. 0 = disabled.
    pub before_context: usize,
    /// Number of context lines to include after each match. 0 = disabled.
    pub after_context: usize,
}

impl Default for GrepOptions {
    fn default() -> Self {
        Self {
            mode: GrepMode::default(),
            casing: Casing::default(),
            max_matches_per_file: 200,
            before_context: 0,
            after_context: 0,
        }
    }
}
