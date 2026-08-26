//! Candidate verification: run the real regex over candidate files.
//!
//! The index is a filter, not an oracle — every candidate is re-checked with
//! ripgrep's matcher/searcher crates, which makes the whole pipeline immune
//! to gram-hash collisions and planner under-approximation.

use std::path::Path;

use grep_regex::RegexMatcher;
use grep_searcher::sinks::UTF8;
use grep_searcher::{BinaryDetection, SearcherBuilder};

#[derive(Debug, Clone)]
pub struct MatchLine {
    pub path: String,
    pub line_number: u64,
    pub line: String,
}

#[derive(Debug)]
pub enum VerifyError {
    BadPattern(String),
    Io(std::io::Error),
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VerifyError::BadPattern(e) => write!(f, "bad pattern: {e}"),
            VerifyError::Io(e) => write!(f, "verify io error: {e}"),
        }
    }
}

impl std::error::Error for VerifyError {}

pub struct Verifier {
    matcher: RegexMatcher,
}

impl Verifier {
    pub fn new(pattern: &str) -> Result<Self, VerifyError> {
        let matcher = RegexMatcher::new(pattern)
            .map_err(|e| VerifyError::BadPattern(e.to_string()))?;
        Ok(Self { matcher })
    }

    /// Search one file, appending matches to `out`. `display_path` is what
    /// gets reported (typically repo-relative).
    pub fn search_file(
        &self,
        abs_path: &Path,
        display_path: &str,
        out: &mut Vec<MatchLine>,
    ) -> Result<(), VerifyError> {
        let mut searcher = SearcherBuilder::new()
            .binary_detection(BinaryDetection::quit(b'\x00'))
            .line_number(true)
            .build();
        searcher
            .search_path(
                &self.matcher,
                abs_path,
                UTF8(|line_number, line| {
                    out.push(MatchLine {
                        path: display_path.to_string(),
                        line_number,
                        line: line.trim_end_matches(['\n', '\r']).to_string(),
                    });
                    Ok(true)
                }),
            )
            .map_err(VerifyError::Io)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_lines_and_skips_binary() {
        let dir = std::env::temp_dir().join(format!("gpx-verify-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let text = dir.join("a.txt");
        std::fs::write(&text, "one handleClick\nplain\nhandleClick again\n").unwrap();
        let bin = dir.join("b.bin");
        std::fs::write(&bin, b"handleClick\x00binary").unwrap();

        let v = Verifier::new("handleClick").unwrap();
        let mut out = Vec::new();
        v.search_file(&text, "a.txt", &mut out).unwrap();
        v.search_file(&bin, "b.bin", &mut out).unwrap();

        assert_eq!(out.len(), 2);
        assert_eq!(out[0].line_number, 1);
        assert_eq!(out[1].line_number, 3);
        assert!(out.iter().all(|m| m.path == "a.txt"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
