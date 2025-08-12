use anyhow::{Context as _, Result, bail};
use std::{
    ops::Range,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MentionUri {
    File(PathBuf),
    Symbol {
        path: PathBuf,
        name: String,
        line_range: Range<u32>,
    },
    Thread(String),
    TextThread(PathBuf),
    Rule(String),
    Selection {
        path: PathBuf,
        line_range: Range<u32>,
    },
}

impl MentionUri {
    pub fn parse(input: &str) -> Result<Self> {
        let url = url::Url::parse(input)?;
        let path = url.path();
        match url.scheme() {
            "file" => {
                if let Some(fragment) = url.fragment() {
                    let range = fragment
                        .strip_prefix("L")
                        .context("Line range must start with \"L\"")?;
                    let (start, end) = range
                        .split_once(":")
                        .context("Line range must use colon as separator")?;
                    let line_range = start
                        .parse::<u32>()
                        .context("Parsing line range start")?
                        .checked_sub(1)
                        .context("Line numbers should be 1-based")?
                        ..end
                            .parse::<u32>()
                            .context("Parsing line range end")?
                            .checked_sub(1)
                            .context("Line numbers should be 1-based")?;
                    let pairs = url.query_pairs().collect::<Vec<_>>();
                    match pairs.as_slice() {
                        [] => Ok(Self::Selection {
                            path: path.into(),
                            line_range,
                        }),
                        [(k, v)] => {
                            if k != "symbol" {
                                bail!("invalid query parameter")
                            }
                            Ok(Self::Symbol {
                                name: v.to_string(),
                                path: path.into(),
                                line_range,
                            })
                        }
                        _ => bail!("too many query pairs"),
                    }
                } else {
                    Ok(Self::File(path.into()))
                }
            }
            "zed" => {
                if let Some(thread) = path.strip_prefix("/agent/thread/") {
                    Ok(Self::Thread(thread.into()))
                } else if let Some(rule) = path.strip_prefix("/agent/rule/") {
                    Ok(Self::Rule(rule.into()))
                } else {
                    bail!("invalid zed url: {:?}", input);
                }
            }
            other => bail!("unrecognized scheme {:?}", other),
        }
    }

    pub fn name(&self) -> String {
        match self {
            MentionUri::File(path) => path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            MentionUri::Symbol { name, .. } => name.clone(),
            MentionUri::Thread(thread) => thread.to_string(),
            MentionUri::TextThread(thread) => thread.display().to_string(),
            MentionUri::Rule(rule) => rule.clone(),
            MentionUri::Selection {
                path, line_range, ..
            } => selection_name(path, line_range),
        }
    }

    // todo! return something that implements display to avoid extra allocs
    pub fn to_link(&self) -> String {
        let name = self.name();
        let uri = self.to_uri();
        format!("[{name}]({uri})")
    }

    pub fn to_uri(&self) -> String {
        match self {
            MentionUri::File(path) => {
                format!("file://{}", path.display())
            }
            MentionUri::Symbol {
                path,
                name,
                line_range,
            } => {
                format!(
                    "file://{}?symbol={}#L{}:{}",
                    path.display(),
                    name,
                    line_range.start + 1,
                    line_range.end + 1,
                )
            }
            MentionUri::Selection { path, line_range } => {
                format!(
                    "file://{}#L{}:{}",
                    path.display(),
                    line_range.start + 1,
                    line_range.end + 1,
                )
            }
            MentionUri::Thread(thread) => {
                format!("zed:///agent/thread/{}", thread)
            }
            MentionUri::TextThread(path) => {
                format!("zed:///agent/text-thread/{}", path.display())
            }
            MentionUri::Rule(rule) => {
                format!("zed:///agent/rule/{}", rule)
            }
        }
    }
}

pub fn selection_name(path: &Path, line_range: &Range<u32>) -> String {
    format!(
        "{} ({}:{})",
        path.file_name().unwrap_or_default().display(),
        line_range.start + 1,
        line_range.end + 1
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_file_uri() {
        let file_uri = "file:///path/to/file.rs";
        let parsed = MentionUri::parse(file_uri).unwrap();
        match &parsed {
            MentionUri::File(path) => assert_eq!(path.to_str().unwrap(), "/path/to/file.rs"),
            _ => panic!("Expected File variant"),
        }
        assert_eq!(parsed.to_uri(), file_uri);
    }

    #[test]
    fn test_parse_symbol_uri() {
        let symbol_uri = "file:///path/to/file.rs?symbol=MySymbol#L10:20";
        let parsed = MentionUri::parse(symbol_uri).unwrap();
        match &parsed {
            MentionUri::Symbol {
                path,
                name,
                line_range,
            } => {
                assert_eq!(path.to_str().unwrap(), "/path/to/file.rs");
                assert_eq!(name, "MySymbol");
                assert_eq!(line_range.start, 9);
                assert_eq!(line_range.end, 19);
            }
            _ => panic!("Expected Symbol variant"),
        }
        assert_eq!(parsed.to_uri(), symbol_uri);
    }

    #[test]
    fn test_parse_selection_uri() {
        let selection_uri = "file:///path/to/file.rs#L5:15";
        let parsed = MentionUri::parse(selection_uri).unwrap();
        match &parsed {
            MentionUri::Selection { path, line_range } => {
                assert_eq!(path.to_str().unwrap(), "/path/to/file.rs");
                assert_eq!(line_range.start, 4);
                assert_eq!(line_range.end, 14);
            }
            _ => panic!("Expected Selection variant"),
        }
        assert_eq!(parsed.to_uri(), selection_uri);
    }

    #[test]
    fn test_parse_thread_uri() {
        let thread_uri = "zed:///agent/thread/session123";
        let parsed = MentionUri::parse(thread_uri).unwrap();
        match &parsed {
            MentionUri::Thread(thread_id) => assert_eq!(thread_id, "session123"),
            _ => panic!("Expected Thread variant"),
        }
        assert_eq!(parsed.to_uri(), thread_uri);
    }

    #[test]
    fn test_parse_rule_uri() {
        let rule_uri = "zed:///agent/rule/my_rule";
        let parsed = MentionUri::parse(rule_uri).unwrap();
        match &parsed {
            MentionUri::Rule(rule) => assert_eq!(rule, "my_rule"),
            _ => panic!("Expected Rule variant"),
        }
        assert_eq!(parsed.to_uri(), rule_uri);
    }

    #[test]
    fn test_invalid_scheme() {
        assert!(MentionUri::parse("http://example.com").is_err());
        assert!(MentionUri::parse("https://example.com").is_err());
        assert!(MentionUri::parse("ftp://example.com").is_err());
    }

    #[test]
    fn test_invalid_zed_path() {
        assert!(MentionUri::parse("zed:///invalid/path").is_err());
        assert!(MentionUri::parse("zed:///agent/unknown/test").is_err());
    }

    #[test]
    fn test_invalid_line_range_format() {
        // Missing L prefix
        assert!(MentionUri::parse("file:///path/to/file.rs#10:20").is_err());

        // Missing colon separator
        assert!(MentionUri::parse("file:///path/to/file.rs#L1020").is_err());

        // Invalid numbers
        assert!(MentionUri::parse("file:///path/to/file.rs#L10:abc").is_err());
        assert!(MentionUri::parse("file:///path/to/file.rs#Labc:20").is_err());
    }

    #[test]
    fn test_invalid_query_parameters() {
        // Invalid query parameter name
        assert!(MentionUri::parse("file:///path/to/file.rs#L10:20?invalid=test").is_err());

        // Too many query parameters
        assert!(
            MentionUri::parse("file:///path/to/file.rs#L10:20?symbol=test&another=param").is_err()
        );
    }

    #[test]
    fn test_zero_based_line_numbers() {
        // Test that 0-based line numbers are rejected (should be 1-based)
        assert!(MentionUri::parse("file:///path/to/file.rs#L0:10").is_err());
        assert!(MentionUri::parse("file:///path/to/file.rs#L1:0").is_err());
        assert!(MentionUri::parse("file:///path/to/file.rs#L0:0").is_err());
    }
}
