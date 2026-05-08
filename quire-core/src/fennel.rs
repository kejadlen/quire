use std::path::Path;

use miette::{Diagnostic, SourceOffset};
use mlua::{Lua, LuaSerdeExt};
use thiserror::Error;

const FENNEL_LUA: &str = include_str!("../vendor/fennel.lua");

/// Error kinds from the Fennel loader.
#[derive(Debug, Error, Diagnostic)]
pub enum FennelError {
    #[error(transparent)]
    #[diagnostic(code(fennel::io))]
    Io(#[from] std::io::Error),

    #[error("internal fennel error: {0}")]
    #[diagnostic(code(fennel::internal))]
    Internal(#[from] mlua::Error),

    /// Fennel/Lua evaluation failed. `message` is just the source
    /// name so miette renders `× <name>`; the actual Lua error text
    /// is reachable via the `#[source]` chain. Plain `Display` will
    /// only show the name — walk the chain (e.g. via
    /// `display_chain`) to surface the underlying error.
    #[error("{message}")]
    #[diagnostic(code(fennel::eval))]
    Eval {
        message: String,
        #[source_code]
        source_code: String,
        #[label("here")]
        label: Option<SourceOffset>,
        #[source]
        source: Box<mlua::Error>,
    },

    /// Result couldn't be deserialized into the requested type.
    /// Same display caveat as `Eval`: `message` is the source name,
    /// the deser error is in the `#[source]` chain.
    #[error("{message}")]
    #[diagnostic(code(fennel::type_mismatch))]
    TypeMismatch {
        message: String,
        #[source]
        source: Box<mlua::Error>,
    },
}

/// Owns a Lua VM with the Fennel compiler registered as a module.
///
/// Constructed once and reused across `load_string` / `load_file` calls.
pub struct Fennel {
    lua: Lua,
}

impl Fennel {
    /// Create a new Fennel instance.
    ///
    /// Loads the vendored `fennel.lua` into a fresh Lua VM and registers it
    /// as the `"fennel"` global so `fennel.eval` is available for compiling
    /// Fennel source.
    pub fn new() -> Result<Self, FennelError> {
        // Load all standard libraries including debug, which Fennel
        // requires internally (traceback, getinfo). The debug library is
        // marked unsafe by mlua because it can break Lua sandboxing, but
        // we only run trusted, vendored Fennel code in this VM.
        let lua = unsafe { Lua::unsafe_new() };

        // Load fennel.lua. The file returns its module table directly.
        let fennel_module: mlua::Table = lua.load(FENNEL_LUA).set_name("fennel.lua").eval()?;

        lua.globals().set("fennel", fennel_module)?;

        Ok(Self { lua })
    }

    /// Borrow the underlying Lua VM. Useful for callers that need to
    /// `to_value` / `from_value` against the same VM the Fennel script
    /// ran in.
    pub fn lua(&self) -> &Lua {
        &self.lua
    }

    /// Compile and evaluate a Fennel source string, returning the raw
    /// Lua value.
    ///
    /// `setup` is called before evaluation and can inject globals or
    /// modify the VM.
    ///
    /// `name` is used as the Lua source name (for tracebacks) and in
    /// miette error messages.
    pub fn eval_raw(
        &self,
        source: &str,
        name: &str,
        setup: impl Fn(&Lua) -> mlua::Result<()>,
    ) -> Result<mlua::Value, FennelError> {
        setup(&self.lua)?;

        let fennel: mlua::Table = self.lua.globals().get("fennel")?;
        let eval: mlua::Function = fennel.get("eval")?;
        let opts = self.lua.create_table()?;

        opts.set("filename", name)?;

        // Align Lua line numbers with Fennel source lines so debug
        // info points back at the user's `.fnl`.
        opts.set("correlate", true)?;

        let result = eval
            .call::<mlua::Value>((source, opts))
            .map_err(|e| FennelError::from_lua(source, name, e))?;

        Ok(result)
    }

    /// Compile and evaluate a Fennel source string, deserializing the result
    /// into `T`.
    ///
    /// `name` is used in error messages — typically a filename or a synthetic
    /// label like `HEAD:.quire/config.fnl`.
    pub fn load_string<T>(&self, source: &str, name: &str) -> Result<T, FennelError>
    where
        T: serde::de::DeserializeOwned,
    {
        let result = self.eval_raw(source, name, |_| Ok(()))?;

        self.lua
            .from_value(result)
            .map_err(|e| FennelError::TypeMismatch {
                message: name.to_string(),
                source: Box::new(e),
            })
    }

    /// Load and evaluate a Fennel file from disk, deserializing the result
    /// into `T`.
    pub fn load_file<T>(&self, path: &Path) -> Result<T, FennelError>
    where
        T: serde::de::DeserializeOwned,
    {
        let source = fs_err::read_to_string(path)?;
        self.load_string(&source, &path.display().to_string())
    }
}

impl FennelError {
    /// Construct an `Eval` error from an mlua error, extracting line
    /// information when available.
    pub(crate) fn from_lua(source: &str, name: &str, err: mlua::Error) -> Self {
        // Use only the filename/location as the message. The source chain
        // carries the full error details, so including them here would
        // duplicate the output in miette's × and ╰─▶ sections.
        let message = name.to_string();

        // Try to extract a line (and optional column) from the Lua
        // error for a label. None when the error message doesn't carry
        // a line — miette renders the source block without an inline
        // pointer in that case.
        let label = extract_line_col(&err.to_string())
            .and_then(|(line, col)| line_col_offset(source, line, col));

        FennelError::Eval {
            message,
            source_code: source.to_string(),
            label,
            source: Box::new(err),
        }
    }
}

/// Try to extract a line and optional column from a Lua error message.
///
/// Lua/Fennel errors embed the source location as `name:LINE:COLUMN: message`.
/// The name may contain colons (e.g. `HEAD:.quire/config.fnl`), so splitting
/// from the left breaks. Match the first `:LINE:COLUMN: ` run, which is
/// unambiguous — filenames don't end with `:digits:digits:`.
fn extract_line_col(msg: &str) -> Option<(usize, Option<usize>)> {
    // Match `:LINE:COLUMN: ` (parse error) or `:LINE: ` (runtime error).
    let re = regex::Regex::new(r":(\d+)(?::(\d+))?: ").ok()?;
    let caps = re.captures(msg)?;
    let line = caps
        .get(1)?
        .as_str()
        .parse::<usize>()
        .ok()
        .filter(|&n| n > 0)?;
    let col = caps.get(2).and_then(|m| m.as_str().parse::<usize>().ok());
    Some((line, col))
}

/// Convert a 1-based line (and optional column) to a byte offset in
/// the source. Column is also 1-based. When column is None, points
/// at the start of the line.
fn line_col_offset(source: &str, line: usize, col: Option<usize>) -> Option<SourceOffset> {
    let mut current_line = 1;
    for (i, ch) in source.char_indices() {
        if current_line == line {
            let byte_offset = if let Some(col) = col {
                // Advance col-1 characters from the start of the line.
                let line_start = i;
                let line_end = source[line_start..]
                    .find('\n')
                    .map(|n| line_start + n)
                    .unwrap_or(source.len());
                let line_text = &source[line_start..line_end];
                let mut byte_pos = 0;
                for (idx, c) in line_text.char_indices() {
                    if idx + 1 == col {
                        byte_pos = idx;
                        break;
                    }
                    byte_pos = idx + c.len_utf8();
                }
                line_start + byte_pos
            } else {
                i
            };
            return Some(SourceOffset::from(byte_offset));
        }
        if ch == '\n' {
            current_line += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    struct MirrorConfig {
        mirror: Mirror,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Mirror {
        url: String,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct FullConfig {
        mirror: Mirror,
        notifications: Notifications,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Notifications {
        to: Vec<String>,
        on: Vec<String>,
    }

    fn fennel() -> Fennel {
        Fennel::new().expect("Fennel::new() should succeed")
    }

    #[test]
    fn load_string_round_trips_simple_table() {
        let f = fennel();
        let config: MirrorConfig = f
            .load_string(
                r#"{:mirror {:url "https://github.com/owner/repo.git"}}"#,
                "test",
            )
            .expect("load_string should succeed");

        assert_eq!(
            config,
            MirrorConfig {
                mirror: Mirror {
                    url: "https://github.com/owner/repo.git".to_string(),
                }
            }
        );
    }

    #[test]
    fn load_string_round_trips_nested_table_with_lists() {
        let f = fennel();
        let source = r#"
{:mirror {:url "https://github.com/owner/repo.git"}
 :notifications {:to ["alpha@example.com"]
                 :on [:ci-failed :mirror-failed]}}
"#;
        let config: FullConfig = f
            .load_string(source, "config.fnl")
            .expect("load_string should succeed");

        assert_eq!(
            config,
            FullConfig {
                mirror: Mirror {
                    url: "https://github.com/owner/repo.git".to_string(),
                },
                notifications: Notifications {
                    to: vec!["alpha@example.com".to_string()],
                    on: vec!["ci-failed".to_string(), "mirror-failed".to_string()],
                },
            }
        );
    }

    #[test]
    fn load_string_rejects_malformed_fennel() {
        let f = fennel();
        let source = "{:bad {:}";
        let result: Result<MirrorConfig, _> = f.load_string(source, "bad.fnl");
        let err = result.unwrap_err();
        let FennelError::Eval {
            message,
            source_code,
            label,
            ..
        } = &err
        else {
            panic!("expected Eval, got {err:?}");
        };
        assert_eq!(message, "bad.fnl");
        assert_eq!(source_code, source);
        assert!(
            label.is_some(),
            "label should be set for line-bearing error"
        );
    }

    #[test]
    fn load_string_rejects_type_mismatch() {
        let f = fennel();
        let result: Result<MirrorConfig, _> = f.load_string("{:mirror {:url 42}}", "types.fnl");
        let err = result.unwrap_err();
        assert!(
            matches!(&err, FennelError::TypeMismatch { message, .. } if message == "types.fnl"),
            "expected TypeMismatch, got {err:?}",
        );
    }

    #[test]
    fn load_file_reads_from_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.fnl");
        fs_err::write(
            &path,
            r#"{:mirror {:url "https://github.com/owner/repo.git"}}"#,
        )
        .expect("write");

        let f = fennel();
        let config: MirrorConfig = f.load_file(&path).expect("load_file should succeed");
        assert_eq!(
            config,
            MirrorConfig {
                mirror: Mirror {
                    url: "https://github.com/owner/repo.git".to_string(),
                }
            }
        );
    }

    #[test]
    fn load_file_rejects_missing_file() {
        let f = fennel();
        let result: Result<MirrorConfig, _> = f.load_file(Path::new("/no/such/file.fnl"));
        let err = result.unwrap_err();
        assert!(
            matches!(&err, FennelError::Io(e) if e.kind() == std::io::ErrorKind::NotFound),
            "expected NotFound io error, got: {err}"
        );
        assert!(
            err.to_string().contains("/no/such/file.fnl"),
            "io error should mention path: {err}"
        );
    }

    #[test]
    fn error_label_works_with_colon_in_name() {
        let f = fennel();
        let source = "\n{:bad {:}";
        let result: Result<MirrorConfig, _> = f.load_string(source, "HEAD:.quire/config.fnl");
        let err = result.unwrap_err();
        let FennelError::Eval { label, .. } = &err else {
            unreachable!()
        };
        assert_eq!(
            label
                .expect("label should be set when line is extractable")
                .offset(),
            8,
            "label should point at the exact error column in line 2"
        );
    }

    #[test]
    fn eval_raw_setup_can_inject_globals() {
        let f = fennel();
        let result = f
            .eval_raw("custom_var", "test", |lua| {
                lua.globals().set("custom_var", 42)
            })
            .expect("eval_raw should succeed");
        assert_eq!(result.as_integer(), Some(42));
    }

    #[test]
    fn extract_line_col_parses_line_and_column() {
        assert_eq!(
            super::extract_line_col("name.fnl:5:12: parse error"),
            Some((5, Some(12)))
        );
    }

    #[test]
    fn extract_line_col_parses_line_only() {
        assert_eq!(
            super::extract_line_col("name.fnl:7: runtime error"),
            Some((7, None))
        );
    }

    #[test]
    fn extract_line_col_handles_colon_in_name() {
        assert_eq!(
            super::extract_line_col("HEAD:.quire/config.fnl:3:1: oops"),
            Some((3, Some(1)))
        );
    }

    #[test]
    fn extract_line_col_returns_none_without_location() {
        assert!(super::extract_line_col("no location info").is_none());
    }

    #[test]
    fn line_col_offset_returns_none_when_line_exceeds_source() {
        // Source has 2 lines, ask for line 10.
        assert!(super::line_col_offset("line1\nline2\n", 10, None).is_none());
    }
}
