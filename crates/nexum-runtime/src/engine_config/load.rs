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
    let cfg = EngineConfig::try_from(toml::from_str::<RawEngineConfig>(&substituted)?)?;
    info!(
        path = %path.display(),
        chains = cfg.chains.len(),
        state_dir = %cfg.engine.state_dir.display(),
        "engine config loaded",
    );
    Ok(cfg)
}

/// Replace every `${VAR_NAME}` token in `raw` with its environment value,
/// erroring on any missing variable. Recognised names match
/// `[A-Z_][A-Z0-9_]*`; anything else inside `${...}` is rejected.
fn substitute_env_vars(raw: &str) -> Result<String, EnvVarError> {
    let mut out = String::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
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
            // Push one UTF-8 char (find the next char boundary).
            #[expect(
                clippy::expect_used,
                reason = "i only ever advances by ch.len_utf8() or past an ASCII '}', so raw[i..] starts on a char boundary and is non-empty inside the loop"
            )]
            let ch = raw[i..]
                .chars()
                .next()
                .expect("byte index is on char boundary");
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    Ok(out)
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
            let raw = "# 河 ${NEXUM_TEST_U} ⚙️\n";
            let out = substitute_env_vars(raw).unwrap();
            assert_eq!(out, "# 河 X ⚙️\n");
        });
    }
}
