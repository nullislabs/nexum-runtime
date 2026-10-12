//! Disk loading with `${VAR}` environment substitution.

use std::path::{Path, PathBuf};

use tracing::{info, warn};

use super::error::{EngineConfigError, EnvVarError};
use super::{EngineConfig, RawEngineConfig};

/// Read an engine config from disk, returning defaults if the file is
/// missing. Parse errors propagate via [`EngineConfigError`].
pub fn load_or_default(path: Option<&Path>) -> Result<EngineConfig, EngineConfigError> {
    let path = match path {
        Some(p) => p.to_path_buf(),
        None => PathBuf::from("engine.toml"),
    };

    if !path.exists() {
        warn!(
            path = %path.display(),
            "engine.toml not found - running with defaults (no chain RPC endpoints; \
             chain-backed host calls will return Unsupported)"
        );
        return Ok(EngineConfig {
            defaulted: true,
            ..EngineConfig::default()
        });
    }

    let raw = std::fs::read_to_string(&path)?;
    // Operators reference RPC URLs (which carry API keys) via
    // `${VAR_NAME}` placeholders so the committed `engine.toml` /
    // `engine.docker.toml` stays secret-free. The substitution runs
    // before TOML parse so a missing var fails fast with the exact
    // variable name, not a downstream "invalid URI" several layers
    // deep.
    let substituted = substitute_env_vars(&raw)?;
    // Parse the raw shape, then convert, so a bad `[chains]` key surfaces
    // as the typed `InvalidChainKey` rather than erased into a serde
    // string by the derived `Deserialize`.
    let parsed = toml::from_str::<RawEngineConfig>(&substituted).map_err(|mut err| {
        // toml prints the line it refuses, and after substitution that line
        // can hold an RPC URL: an unquoted `${VAR}` is enough. Blanked
        // text keeps the line and column and prints none of the content.
        let blank: String = substituted
            .bytes()
            .map(|b| if b == b'\n' { '\n' } else { ' ' })
            .collect();
        err.set_input(Some(&blank));
        err
    })?;
    let cfg = EngineConfig::try_from(parsed)?;
    info!(
        path = %path.display(),
        chains = cfg.chains.len(),
        state_dir = %cfg.engine.state_dir.display(),
        "engine config loaded",
    );
    Ok(cfg)
}

/// Replace every `${VAR_NAME}` token in `raw` with its environment value,
/// erroring on any missing variable. Recognized names match
/// `[A-Z_][A-Z0-9_]*`; anything else inside `${...}` is rejected.
///
/// A TOML comment is copied through untouched, so a token spelled out there
/// as an example is never looked up. Comments are found in the file as
/// written: a substituted value is opaque and cannot open or close one.
fn substitute_env_vars(raw: &str) -> Result<String, EnvVarError> {
    let mut out = String::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut lex = Lex::Bare;
    let mut i = 0;
    while i < bytes.len() {
        if lex != Lex::Comment && bytes[i..].starts_with(b"${") {
            // Find the closing `}`.
            let start = i + 2;
            let Some(end_offset) = raw[start..].find('}') else {
                return Err(EnvVarError::Unclosed { offset: i });
            };
            let end = start + end_offset;
            let name = &raw[start..end];
            if !is_valid_env_name(name) {
                return Err(EnvVarError::InvalidName {
                    name: name.to_owned(),
                });
            }
            match std::env::var(name) {
                Ok(val) => out.push_str(&val),
                Err(_) => {
                    return Err(EnvVarError::Missing {
                        name: name.to_owned(),
                    });
                }
            }
            i = end + 1;
        } else {
            // Every unit is whole chars and a token ends on an ASCII `}`,
            // so `i` stays on a char boundary.
            let (len, next) = lex.step(&raw[i..]);
            out.push_str(&raw[i..i + len]);
            i += len;
            lex = next;
        }
    }
    Ok(out)
}

/// Where the scanner stands in the TOML text. Only [`Lex::Comment`] turns
/// substitution off: a `#` inside any string form is data.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lex {
    /// Outside every string and comment.
    Bare,
    /// After a `#`, up to the end of the line.
    Comment,
    /// Inside `"..."`.
    Basic,
    /// Inside `'...'`.
    Literal,
    /// Inside `"""..."""`.
    MultiBasic,
    /// Inside `'''...'''`.
    MultiLiteral,
}

impl Lex {
    /// Length of the unit at the head of `rest` (a quote run, an escape
    /// pair, or one char) and the state after it. `rest` is not empty.
    fn step(self, rest: &str) -> (usize, Self) {
        let bytes = rest.as_bytes();
        let run = |quote: u8| bytes.iter().take_while(|&&b| b == quote).count();
        match (self, bytes[0]) {
            (Self::Bare, b'#') => (1, Self::Comment),
            (Self::Bare, b'"') if run(b'"') >= 3 => (3, Self::MultiBasic),
            (Self::Bare, b'"') => (1, Self::Basic),
            (Self::Bare, b'\'') if run(b'\'') >= 3 => (3, Self::MultiLiteral),
            (Self::Bare, b'\'') => (1, Self::Literal),
            (Self::Comment, b'\n') => (1, Self::Bare),
            (Self::Basic, b'"') | (Self::Literal, b'\'') => (1, Self::Bare),
            // A single-line string cannot span lines. Ending it here keeps
            // one unbalanced quote from turning every later comment into
            // string data.
            (Self::Basic | Self::Literal, b'\n') => (1, Self::Bare),
            // An escaped quote does not close the string, and an escaped
            // backslash does not escape the quote after it. Any other
            // escape is left to the next step, so `\${VAR}` still
            // substitutes.
            (Self::Basic | Self::MultiBasic, b'\\')
                if matches!(bytes.get(1), Some(b'"' | b'\\')) =>
            {
                (2, self)
            }
            (Self::MultiBasic, b'"') => self.multi_line_quotes(run(b'"')),
            (Self::MultiLiteral, b'\'') => self.multi_line_quotes(run(b'\'')),
            _ => (rest.chars().next().map_or(1, char::len_utf8), self),
        }
    }

    /// A run of three quotes closes a multi-line string, and TOML allows up
    /// to two more directly before them as content.
    fn multi_line_quotes(self, run: usize) -> (usize, Self) {
        if run >= 3 {
            (run.min(5), Self::Bare)
        } else {
            (run, self)
        }
    }
}

fn is_valid_env_name(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_uppercase() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_or_default_marks_a_missing_file_as_defaulted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("engine.toml");
        let cfg = load_or_default(Some(&missing)).expect("a missing file falls back to defaults");
        assert!(
            cfg.defaulted,
            "the missing-file fallback carries provenance"
        );
        assert!(cfg.chains.is_empty());
        // The absent-file path must not be the fail-open one.
        assert!(cfg.engine.require_component_digest);
    }

    #[test]
    fn load_or_default_marks_a_loaded_file_as_configured() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("engine.toml");
        std::fs::write(&path, "[chains.1]\nrpc_url = \"http://localhost:8545\"\n")
            .expect("write engine.toml");
        let cfg = load_or_default(Some(&path)).expect("the file parses");
        assert!(!cfg.defaulted, "a loaded engine.toml is not defaulted");
        assert_eq!(cfg.chains.len(), 1);
    }

    #[test]
    fn a_refused_line_is_located_and_not_printed() {
        // Two slips that put the URL on the line the parser refuses: a
        // repeated key, and a URL with no quotes round it.
        const URL: &str = "https://host-marker.example/v2/key-marker";
        for body in [
            format!("[chains.1]\nrpc_url = \"{URL}\"\nrpc_url = \"{URL}\"\n"),
            format!("[chains.1]\n\nrpc_url = {URL}\n"),
        ] {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("engine.toml");
            std::fs::write(&path, body).expect("write engine.toml");
            let err = load_or_default(Some(&path)).expect_err("the parser refuses");
            assert!(matches!(err, EngineConfigError::Toml(_)), "{err:?}");
            let text = format!("{err}\n{err:?}");
            for marker in ["host-marker", "key-marker"] {
                assert!(
                    !text.contains(marker),
                    "the refusal prints {marker}: {text}"
                );
            }
            assert!(err.to_string().contains("at line 3, column "), "{err}");
        }
    }

    //
    // These tests stash + restore process env vars under unique names
    // so parallel `cargo test` runs don't trip on each other.

    fn with_env<F: FnOnce()>(name: &str, value: &str, body: F) {
        let prev = std::env::var(name).ok();
        // SAFETY: tests are single-threaded within one test fn; setting
        // an env var here is fine since the unique-name convention
        // avoids cross-test races.
        unsafe { std::env::set_var(name, value) };
        body();
        match prev {
            Some(v) => unsafe { std::env::set_var(name, v) },
            None => unsafe { std::env::remove_var(name) },
        }
    }

    #[test]
    fn substitute_replaces_known_variable() {
        with_env("NEXUM_TEST_RPC", "wss://example.test/abc", || {
            let raw = r#"rpc_url = "${NEXUM_TEST_RPC}""#;
            let out = substitute_env_vars(raw).unwrap();
            assert_eq!(out, r#"rpc_url = "wss://example.test/abc""#);
        });
    }

    #[test]
    fn substitute_errors_on_missing_variable() {
        // Variable name must not collide with anything in the operator
        // environment. Use a guaranteed-unique prefix.
        let err =
            substitute_env_vars(r#"x = "${NEXUM_TEST_DEFINITELY_UNSET_VAR_XYZ}""#).unwrap_err();
        assert!(
            matches!(&err, EnvVarError::Missing { name }
                if name == "NEXUM_TEST_DEFINITELY_UNSET_VAR_XYZ"),
            "{err}"
        );
    }

    #[test]
    fn substitute_errors_on_invalid_name() {
        let err = substitute_env_vars(r#"x = "${lowercase_name}""#).unwrap_err();
        assert!(matches!(err, EnvVarError::InvalidName { .. }));
    }

    #[test]
    fn substitute_errors_on_unclosed_brace() {
        let err = substitute_env_vars(r#"x = "${UNCLOSED"#).unwrap_err();
        assert!(matches!(err, EnvVarError::Unclosed { .. }));
    }

    #[test]
    fn substitute_passes_text_with_no_placeholders_through() {
        let raw = "no placeholders here\nrpc_url = \"wss://x\"";
        assert_eq!(substitute_env_vars(raw).unwrap(), raw);
    }

    #[test]
    fn substitute_handles_multiple_placeholders_in_one_line() {
        with_env("NEXUM_TEST_A", "alpha", || {
            with_env("NEXUM_TEST_B", "beta", || {
                let raw = "k = \"${NEXUM_TEST_A}-${NEXUM_TEST_B}\"";
                let out = substitute_env_vars(raw).unwrap();
                assert_eq!(out, "k = \"alpha-beta\"");
            });
        });
    }

    #[test]
    fn substitute_preserves_utf8_around_placeholder() {
        // The hand-rolled byte loop must respect multi-byte UTF-8.
        with_env("NEXUM_TEST_U", "X", || {
            let raw = "k = \"河 ${NEXUM_TEST_U} ⚙️\"\n";
            let out = substitute_env_vars(raw).unwrap();
            assert_eq!(out, "k = \"河 X ⚙️\"\n");
        });
    }

    /// Unset in every environment, so a lookup of it is a `Missing` error.
    const UNSET: &str = "${NEXUM_TEST_COMMENT_ONLY_UNSET_VAR}";

    #[test]
    fn a_token_in_a_full_line_comment_is_left_alone() {
        // Set, so the assertion fails on a substitution and not only on an
        // error.
        with_env("NEXUM_TEST_FULL_LINE", "leaked", || {
            let raw = "# export ${NEXUM_TEST_FULL_LINE} first\n  # 河 ${NEXUM_TEST_FULL_LINE} ⚙️\r\nk = 1\n";
            assert_eq!(substitute_env_vars(raw).unwrap(), raw);
        });
    }

    #[test]
    fn a_token_in_a_trailing_comment_is_left_alone() {
        with_env("NEXUM_TEST_TRAILING", "alpha", || {
            let raw = "k = \"${NEXUM_TEST_TRAILING}\" # from ${NEXUM_TEST_TRAILING}\nn = 1 # ${NEXUM_TEST_TRAILING}\n";
            let out = substitute_env_vars(raw).unwrap();
            assert_eq!(
                out,
                "k = \"alpha\" # from ${NEXUM_TEST_TRAILING}\nn = 1 # ${NEXUM_TEST_TRAILING}\n"
            );
        });
    }

    #[test]
    fn a_missing_variable_in_a_comment_is_not_an_error() {
        let raw = format!("# set {UNSET}\nk = \"v\" # or {UNSET}\n");
        assert_eq!(substitute_env_vars(&raw).unwrap(), raw);
    }

    #[test]
    fn a_malformed_token_in_a_comment_is_not_an_error() {
        let raw = "# ${lowercase} and ${} and ${UNCLOSED\nk = 1\n";
        assert_eq!(substitute_env_vars(raw).unwrap(), raw);
    }

    #[test]
    fn a_missing_variable_in_a_value_after_a_comment_is_still_fatal() {
        let raw = format!("# example {UNSET}\nk = \"{UNSET}\"\n");
        let err = substitute_env_vars(&raw).unwrap_err();
        assert!(
            matches!(&err, EnvVarError::Missing { name }
                if name == "NEXUM_TEST_COMMENT_ONLY_UNSET_VAR"),
            "{err}"
        );
        assert!(
            err.to_string()
                .contains("NEXUM_TEST_COMMENT_ONLY_UNSET_VAR"),
            "{err}"
        );
    }

    /// Runs each `(raw, expected)` pair with an unset token appended as a
    /// trailing comment. The TOML parser must accept the output, so the
    /// appended text is a comment to it too and not the tail of a string.
    fn assert_substitutes(cases: &[(&str, &str)]) {
        for (raw, expected) in cases {
            let raw = format!("{raw} # {UNSET}\n");
            let out = substitute_env_vars(&raw).unwrap();
            assert_eq!(out, format!("{expected} # {UNSET}\n"), "{raw}");
            if let Err(err) = toml::from_str::<toml::Table>(&out) {
                panic!("{raw}: {err}");
            }
        }
    }

    #[test]
    fn a_hash_inside_each_string_form_does_not_start_a_comment() {
        with_env("NEXUM_TEST_HASH", "X", || {
            // The token after the in-string `#` substitutes.
            let cases = [
                (r#"k = "a # ${NEXUM_TEST_HASH}""#, r#"k = "a # X""#),
                (r"k = 'a # ${NEXUM_TEST_HASH}'", r"k = 'a # X'"),
                (
                    "k = \"\"\"\na # ${NEXUM_TEST_HASH}\n# ${NEXUM_TEST_HASH}\n\"\"\"",
                    "k = \"\"\"\na # X\n# X\n\"\"\"",
                ),
                (
                    "k = '''\na # ${NEXUM_TEST_HASH}\n# ${NEXUM_TEST_HASH}\n'''",
                    "k = '''\na # X\n# X\n'''",
                ),
                // A quoted key and an inline table are strings too.
                (
                    r##""a#b" = { c = "# ${NEXUM_TEST_HASH}" }"##,
                    r##""a#b" = { c = "# X" }"##,
                ),
            ];
            assert_substitutes(&cases);
        });
    }

    #[test]
    fn an_escaped_quote_does_not_close_a_basic_string() {
        with_env("NEXUM_TEST_ESC", "X", || {
            let cases = [
                // `\"` stays inside the string.
                (r#"k = "a\" # ${NEXUM_TEST_ESC}""#, r#"k = "a\" # X""#),
                // `\\` is an escaped backslash, so the next quote closes.
                (r#"k = "a\\""#, r#"k = "a\\""#),
                (r#"k = "a\\\" # ${NEXUM_TEST_ESC}""#, r#"k = "a\\\" # X""#),
                (
                    "k = \"\"\"a\\\"\"\" # ${NEXUM_TEST_ESC}\"\"\"",
                    "k = \"\"\"a\\\"\"\" # X\"\"\"",
                ),
            ];
            assert_substitutes(&cases);
        });
    }

    #[test]
    fn quotes_inside_a_multi_line_string_do_not_close_it() {
        with_env("NEXUM_TEST_MULTI", "X", || {
            let cases = [
                // One or two quotes are content.
                (
                    "k = \"\"\"a \"b\" \"\"c # ${NEXUM_TEST_MULTI}\"\"\"",
                    "k = \"\"\"a \"b\" \"\"c # X\"\"\"",
                ),
                (
                    "k = '''a 'b' ''c # ${NEXUM_TEST_MULTI}'''",
                    "k = '''a 'b' ''c # X'''",
                ),
                // Up to two quotes directly before the delimiter are content,
                // and the string still ends there.
                ("k = \"\"\"a\"\"\"\"\"", "k = \"\"\"a\"\"\"\"\""),
                ("k = '''a''''", "k = '''a''''"),
                // The other quote character is plain data in each form.
                (r#"k = "it's # ${NEXUM_TEST_MULTI}""#, r#"k = "it's # X""#),
                (
                    r#"k = 'say "hi" # ${NEXUM_TEST_MULTI}'"#,
                    r#"k = 'say "hi" # X'"#,
                ),
                // Empty strings open and close at once.
                (r#"a = "" "#, r#"a = "" "#),
                ("a = '' ", "a = '' "),
                (r#"a = """""" "#, r#"a = """""" "#),
            ];
            assert_substitutes(&cases);
        });
    }

    #[test]
    fn an_unterminated_string_ends_at_its_line() {
        // The TOML parser reports the stray quote; the scanner must not
        // read the comment on the next line as string data first.
        for stray in ["k = \"abc", "k = 'abc"] {
            let raw = format!("{stray}\n# {UNSET}\n");
            assert_eq!(substitute_env_vars(&raw).unwrap(), raw);
        }
    }

    #[test]
    fn substitute_replaces_an_unquoted_token_and_a_key() {
        with_env("NEXUM_TEST_BARE", "11155111", || {
            let raw = "[chains.${NEXUM_TEST_BARE}]\nrequest_timeout_secs = ${NEXUM_TEST_BARE}\n";
            let out = substitute_env_vars(raw).unwrap();
            assert_eq!(out, "[chains.11155111]\nrequest_timeout_secs = 11155111\n");
        });
    }

    #[test]
    fn a_config_with_a_token_in_a_comment_loads() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("engine.toml");
        std::fs::write(
            &path,
            format!(
                "# rpc_url = \"{UNSET}\"\n[chains.1]\nrpc_url = \"http://localhost:8545\" # or {UNSET}\n"
            ),
        )
        .expect("write engine.toml");
        let cfg = load_or_default(Some(&path)).expect("a comment token must not refuse the boot");
        assert_eq!(cfg.chains.len(), 1);
    }

    #[test]
    fn a_config_with_a_missing_variable_in_a_value_refuses_naming_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("engine.toml");
        std::fs::write(
            &path,
            format!("# a comment\n[chains.1]\nrpc_url = \"{UNSET}\"\n"),
        )
        .expect("write engine.toml");
        let err = load_or_default(Some(&path)).expect_err("a missing variable is fatal");
        assert!(
            matches!(&err, EngineConfigError::Substitute(EnvVarError::Missing { name })
                if name == "NEXUM_TEST_COMMENT_ONLY_UNSET_VAR"),
            "{err}"
        );
        assert!(
            err.to_string()
                .contains("NEXUM_TEST_COMMENT_ONLY_UNSET_VAR"),
            "{err}"
        );
    }
}
