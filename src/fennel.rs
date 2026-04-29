use std::path::Path;

use miette::{Diagnostic, Result, SourceOffset};
use mlua::{Lua, LuaSerdeExt};

const FENNEL_LUA: &str = include_str!("../vendor/fennel.lua");

/// Error kinds from the Fennel loader.
#[derive(Debug, thiserror::Error, Diagnostic)]
pub enum FennelError {
    #[error("file not found: {0}")]
    FileNotFound(String),

    #[error(transparent)]
    #[diagnostic(code(fennel::io))]
    Io(#[from] std::io::Error),

    #[error("internal fennel error")]
    #[diagnostic(code(fennel::internal))]
    Internal(#[from] mlua::Error),

    #[error("{message}")]
    #[diagnostic(code(fennel::eval))]
    Eval {
        message: String,
        #[source_code]
        source_code: String,
        #[label("here")]
        label: SourceOffset,
        #[source]
        source: Box<mlua::Error>,
    },

    #[error("empty config: {name}")]
    #[diagnostic(code(fennel::empty))]
    Empty { name: String },

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

    /// Compile and evaluate a Fennel source string, returning the raw
    /// Lua value.
    ///
    /// `setup` is called before evaluation and can inject globals or
    /// modify the VM.
    ///
    /// `filename` is passed to Lua as the source name (used in tracebacks).
    /// `display` is used in miette error messages (can be more human-readable).
    pub fn eval_raw(
        &self,
        source: &str,
        filename: &str,
        display: &str,
        setup: impl Fn(&Lua) -> mlua::Result<()>,
    ) -> Result<mlua::Value, FennelError> {
        setup(&self.lua).map_err(|e| FennelError::from_lua(source, display, e))?;

        let fennel: mlua::Table = self.lua.globals().get("fennel")?;

        let eval: mlua::Function = fennel.get("eval")?;

        let opts = self.lua.create_table()?;

        opts.set("filename", filename)?;

        let result = eval
            .call::<mlua::Value>((source, opts))
            .map_err(|e| FennelError::from_lua(source, display, e))?;

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
        if source.trim().is_empty() {
            return Err(FennelError::Empty {
                name: name.to_string(),
            });
        }

        let result = self.eval_raw(source, name, name, |_| Ok(()))?;

        // Reject nil results — a config file that evaluates to nothing is
        // almost always a mistake.
        if matches!(result, mlua::Value::Nil) {
            return Err(FennelError::Empty {
                name: name.to_string(),
            });
        }

        self.lua
            .from_value(result)
            .map_err(|e| FennelError::TypeMismatch {
                message: format!("{name}: {e}"),
                source: Box::new(e),
            })
    }

    /// Load and evaluate a Fennel file from disk, deserializing the result
    /// into `T`.
    pub fn load_file<T>(&self, path: &Path) -> Result<T, FennelError>
    where
        T: serde::de::DeserializeOwned,
    {
        if !path.exists() {
            return Err(FennelError::FileNotFound(path.display().to_string()));
        }

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

        // Try to extract a line number from the Lua error for a label.
        let offset = extract_line_offset(&err)
            .and_then(|line| line_offset(source, line))
            .unwrap_or(SourceOffset::from(0));

        FennelError::Eval {
            message,
            source_code: source.to_string(),
            label: offset,
            source: Box::new(err),
        }
    }
}

/// Try to extract a line number from a Lua error message.
///
/// Lua/Fennel errors embed the source location as `name:LINE:COLUMN: message`.
/// The name may contain colons (e.g. `HEAD:.quire/config.fnl`), so splitting
/// from the left breaks. Match the first `:LINE:COLUMN: ` run, which is
/// unambiguous — filenames don't end with `:digits:digits:`.
fn extract_line_offset(err: &mlua::Error) -> Option<usize> {
    let msg = err.to_string();
    // Match `:LINE:COLUMN: ` (parse error) or `:LINE: ` (runtime error).
    let re = regex::Regex::new(r":(\d+)(?::\d+)?: ").ok()?;
    let caps = re.captures(&msg)?;
    caps.get(1)?
        .as_str()
        .parse::<usize>()
        .ok()
        .filter(|&n| n > 0)
}

/// Convert a 1-based line number to a byte offset in the source.
fn line_offset(source: &str, line: usize) -> Option<SourceOffset> {
    if line == 0 {
        return None;
    }
    let mut current_line = 1;
    for (i, ch) in source.char_indices() {
        if current_line == line {
            return Some(SourceOffset::from(i));
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
    fn load_string_rejects_empty_source() {
        let f = fennel();
        let result: Result<MirrorConfig, _> = f.load_string("", "empty.fnl");
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), FennelError::Empty { .. }));
    }

    #[test]
    fn load_string_rejects_whitespace_only() {
        let f = fennel();
        let result: Result<MirrorConfig, _> = f.load_string("  \n  ", "blank.fnl");
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), FennelError::Empty { .. }));
    }

    #[test]
    fn load_string_rejects_malformed_fennel() {
        let f = fennel();
        let result: Result<MirrorConfig, _> = f.load_string("{:bad {:}", "bad.fnl");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("bad.fnl"),
            "error should mention source name: {err}"
        );
    }

    #[test]
    fn load_string_rejects_type_mismatch() {
        let f = fennel();
        let result: Result<MirrorConfig, _> = f.load_string("{:mirror {:url 42}}", "types.fnl");
        assert!(result.is_err());
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
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), FennelError::FileNotFound(..)));
    }

    #[test]
    fn error_label_works_with_colon_in_name() {
        let f = fennel();
        let source = "\n{:bad {:}";
        let result: Result<MirrorConfig, _> = f.load_string(source, "HEAD:.quire/config.fnl");
        let err = result.unwrap_err();
        if let FennelError::Eval { label, .. } = &err {
            assert_eq!(
                label.offset(),
                1,
                "label should point at line 2 despite colons in name"
            );
        }
    }
}
