//! Validation for ScriptExtender `Config.json` files.
//!
//! Parses JSON content and returns a `Vec<Diagnostic>` with line/column
//! positions for each issue found. An empty vec means the file is valid.

use serde_json::Value;

/// Known feature-flag values accepted in the `FeatureFlags` array.
const KNOWN_FEATURE_FLAGS: &[&str] = &["Lua", "Extender", "Osiris", "OsirisExtensions"];

/// Known top-level keys in SE Config.json.
const KNOWN_KEYS: &[&str] = &["RequiredVersion", "ModTable", "FeatureFlags", "Preload"];

// ── Public types ──

#[derive(Debug, Clone, serde::Serialize)]
pub struct Diagnostic {
    pub line: usize,
    pub col: usize,
    pub severity: DiagnosticSeverity,
    pub message: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Info,
}

// ── Entry point ──

/// Validate the raw text of an SE `Config.json` file.
///
/// Returns diagnostics with 1-based line and column positions. An empty
/// `Vec` means the content is valid.
pub fn validate_se_config(content: &str) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    // ── Step 1: parse JSON ──
    let root: Value = match serde_json::from_str(content) {
        Ok(v) => v,
        Err(e) => {
            let (line, col) = json_error_position(&e);
            diagnostics.push(Diagnostic {
                line,
                col,
                severity: DiagnosticSeverity::Error,
                message: format!("Invalid JSON: {e}"),
            });
            return diagnostics;
        }
    };

    let obj = match root.as_object() {
        Some(o) => o,
        None => {
            diagnostics.push(Diagnostic {
                line: 1,
                col: 1,
                severity: DiagnosticSeverity::Error,
                message: "Config must be a JSON object".into(),
            });
            return diagnostics;
        }
    };

    // ── Step 2: check for unknown keys (warnings) ──
    for key in obj.keys() {
        if !KNOWN_KEYS.contains(&key.as_str()) {
            let (line, col) = find_key_position(content, key);
            diagnostics.push(Diagnostic {
                line,
                col,
                severity: DiagnosticSeverity::Warning,
                message: format!("Unknown key \"{key}\""),
            });
        }
    }

    // ── Step 3: RequiredVersion ──
    match obj.get("RequiredVersion") {
        None => {
            diagnostics.push(Diagnostic {
                line: 1,
                col: 1,
                severity: DiagnosticSeverity::Error,
                message: "Missing required key \"RequiredVersion\"".into(),
            });
        }
        Some(val) => {
            let (line, col) = find_key_position(content, "RequiredVersion");
            if let Some(n) = val.as_i64() {
                if n < 1 {
                    diagnostics.push(Diagnostic {
                        line,
                        col,
                        severity: DiagnosticSeverity::Error,
                        message: format!("\"RequiredVersion\" must be ≥ 1, got {n}"),
                    });
                }
            } else if let Some(n) = val.as_u64() {
                // already >= 0, but check >= 1
                if n < 1 {
                    diagnostics.push(Diagnostic {
                        line,
                        col,
                        severity: DiagnosticSeverity::Error,
                        message: "\"RequiredVersion\" must be ≥ 1, got 0".into(),
                    });
                }
            } else {
                diagnostics.push(Diagnostic {
                    line,
                    col,
                    severity: DiagnosticSeverity::Error,
                    message: "\"RequiredVersion\" must be an integer".into(),
                });
            }
        }
    }

    // ── Step 4: ModTable ──
    match obj.get("ModTable") {
        None => {
            diagnostics.push(Diagnostic {
                line: 1,
                col: 1,
                severity: DiagnosticSeverity::Error,
                message: "Missing required key \"ModTable\"".into(),
            });
        }
        Some(val) => {
            let (line, col) = find_key_position(content, "ModTable");
            match val.as_str() {
                None => {
                    diagnostics.push(Diagnostic {
                        line,
                        col,
                        severity: DiagnosticSeverity::Error,
                        message: "\"ModTable\" must be a string".into(),
                    });
                }
                Some("") => {
                    diagnostics.push(Diagnostic {
                        line,
                        col,
                        severity: DiagnosticSeverity::Error,
                        message: "\"ModTable\" must not be empty".into(),
                    });
                }
                Some(s) => {
                    if !is_valid_lua_identifier(s) {
                        diagnostics.push(Diagnostic {
                            line,
                            col,
                            severity: DiagnosticSeverity::Error,
                            message: format!(
                                "\"ModTable\" must be a valid Lua identifier \
                                 ([A-Za-z_][A-Za-z0-9_]*), got \"{s}\""
                            ),
                        });
                    }
                }
            }
        }
    }

    // ── Step 5: FeatureFlags ──
    match obj.get("FeatureFlags") {
        None => {
            diagnostics.push(Diagnostic {
                line: 1,
                col: 1,
                severity: DiagnosticSeverity::Error,
                message: "Missing required key \"FeatureFlags\"".into(),
            });
        }
        Some(val) => {
            let (line, col) = find_key_position(content, "FeatureFlags");
            match val.as_array() {
                None => {
                    diagnostics.push(Diagnostic {
                        line,
                        col,
                        severity: DiagnosticSeverity::Error,
                        message: "\"FeatureFlags\" must be an array".into(),
                    });
                }
                Some(arr) => {
                    for item in arr {
                        match item.as_str() {
                            None => {
                                diagnostics.push(Diagnostic {
                                    line,
                                    col,
                                    severity: DiagnosticSeverity::Error,
                                    message: "\"FeatureFlags\" entries must be strings".into(),
                                });
                            }
                            Some(flag) => {
                                if !KNOWN_FEATURE_FLAGS.contains(&flag) {
                                    let (fl, fc) = find_string_value_position(content, flag);
                                    diagnostics.push(Diagnostic {
                                        line: fl,
                                        col: fc,
                                        severity: DiagnosticSeverity::Warning,
                                        message: format!("Unknown feature flag \"{flag}\""),
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // ── Step 6: Preload (optional) ──
    if let Some(val) = obj.get("Preload") {
        if !val.is_boolean() {
            let (line, col) = find_key_position(content, "Preload");
            diagnostics.push(Diagnostic {
                line,
                col,
                severity: DiagnosticSeverity::Error,
                message: "\"Preload\" must be a boolean".into(),
            });
        }
    }

    diagnostics
}

// ── Helpers ──

/// Extract 1-based line/col from a `serde_json::Error`.
fn json_error_position(e: &serde_json::Error) -> (usize, usize) {
    (e.line(), e.column())
}

/// Check if `s` is a valid Lua identifier: `[A-Za-z_][A-Za-z0-9_]*`.
fn is_valid_lua_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        None => false,
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {
            chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        _ => false,
    }
}

/// Find the 1-based (line, col) of a JSON key in the raw source.
///
/// Searches for `"key"` preceded by a valid JSON-key context (start of
/// object or comma). Falls back to (1, 1) if not found.
fn find_key_position(content: &str, key: &str) -> (usize, usize) {
    let needle = format!("\"{key}\"");
    let mut search_start = 0;

    while let Some(rel) = content[search_start..].find(&needle) {
        let abs = search_start + rel;

        // Verify this is a key (followed by `:`), not a value
        let after = &content[abs + needle.len()..];
        let after_trimmed = after.trim_start();
        if after_trimmed.starts_with(':') {
            return offset_to_line_col(content, abs);
        }

        search_start = abs + 1;
    }

    (1, 1)
}

/// Find the 1-based (line, col) of a string value `"val"` in the raw source.
///
/// Searches for `"val"` that is NOT followed by `:` (i.e., it's a value,
/// not a key). Falls back to (1, 1).
fn find_string_value_position(content: &str, val: &str) -> (usize, usize) {
    let needle = format!("\"{val}\"");
    let mut search_start = 0;

    while let Some(rel) = content[search_start..].find(&needle) {
        let abs = search_start + rel;

        // Must NOT be followed by `:` (that would be a key)
        let after = &content[abs + needle.len()..];
        let after_trimmed = after.trim_start();
        if !after_trimmed.starts_with(':') {
            return offset_to_line_col(content, abs);
        }

        search_start = abs + 1;
    }

    (1, 1)
}

/// Convert a byte offset into 1-based (line, col).
fn offset_to_line_col(content: &str, offset: usize) -> (usize, usize) {
    let before = &content[..offset];
    let line = before.matches('\n').count() + 1;
    let col = match before.rfind('\n') {
        Some(nl) => offset - nl,
        None => offset + 1,
    };
    (line, col)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has_error(diags: &[Diagnostic], substr: &str) -> bool {
        diags
            .iter()
            .any(|d| matches!(d.severity, DiagnosticSeverity::Error) && d.message.contains(substr))
    }

    fn has_warning(diags: &[Diagnostic], substr: &str) -> bool {
        diags.iter().any(|d| {
            matches!(d.severity, DiagnosticSeverity::Warning) && d.message.contains(substr)
        })
    }

    #[test]
    fn valid_config_produces_no_diagnostics() {
        let content = r#"{
    "RequiredVersion": 1,
    "ModTable": "MyMod",
    "FeatureFlags": ["Lua", "Osiris"]
}"#;
        let diags = validate_se_config(content);
        assert!(diags.is_empty(), "Expected no diagnostics, got: {diags:?}");
    }

    #[test]
    fn valid_config_with_preload() {
        let content = r#"{
    "RequiredVersion": 4,
    "ModTable": "CoolMod_v2",
    "FeatureFlags": ["Extender"],
    "Preload": true
}"#;
        let diags = validate_se_config(content);
        assert!(diags.is_empty(), "Expected no diagnostics, got: {diags:?}");
    }

    #[test]
    fn invalid_json_produces_error() {
        let diags = validate_se_config("{ not valid json }");
        assert!(has_error(&diags, "Invalid JSON"));
    }

    #[test]
    fn non_object_root_produces_error() {
        let diags = validate_se_config("[1, 2, 3]");
        assert!(has_error(&diags, "must be a JSON object"));
    }

    #[test]
    fn missing_required_keys() {
        let diags = validate_se_config("{}");
        assert!(has_error(
            &diags,
            "Missing required key \"RequiredVersion\""
        ));
        assert!(has_error(&diags, "Missing required key \"ModTable\""));
        assert!(has_error(&diags, "Missing required key \"FeatureFlags\""));
    }

    #[test]
    fn required_version_must_be_positive_integer() {
        let content = r#"{"RequiredVersion": 0, "ModTable": "M", "FeatureFlags": []}"#;
        let diags = validate_se_config(content);
        assert!(has_error(&diags, "must be ≥ 1"));

        let content = r#"{"RequiredVersion": -5, "ModTable": "M", "FeatureFlags": []}"#;
        let diags = validate_se_config(content);
        assert!(has_error(&diags, "must be ≥ 1"));

        let content = r#"{"RequiredVersion": "one", "ModTable": "M", "FeatureFlags": []}"#;
        let diags = validate_se_config(content);
        assert!(has_error(&diags, "must be an integer"));
    }

    #[test]
    fn mod_table_must_be_valid_lua_identifier() {
        // Empty string
        let content = r#"{"RequiredVersion": 1, "ModTable": "", "FeatureFlags": []}"#;
        let diags = validate_se_config(content);
        assert!(has_error(&diags, "must not be empty"));

        // Starts with digit
        let content = r#"{"RequiredVersion": 1, "ModTable": "1Bad", "FeatureFlags": []}"#;
        let diags = validate_se_config(content);
        assert!(has_error(&diags, "valid Lua identifier"));

        // Contains special chars
        let content = r#"{"RequiredVersion": 1, "ModTable": "my-mod", "FeatureFlags": []}"#;
        let diags = validate_se_config(content);
        assert!(has_error(&diags, "valid Lua identifier"));

        // Non-string
        let content = r#"{"RequiredVersion": 1, "ModTable": 42, "FeatureFlags": []}"#;
        let diags = validate_se_config(content);
        assert!(has_error(&diags, "must be a string"));
    }

    #[test]
    fn feature_flags_unknown_value_warns() {
        let content =
            r#"{"RequiredVersion": 1, "ModTable": "M", "FeatureFlags": ["Lua", "BogusFlag"]}"#;
        let diags = validate_se_config(content);
        assert!(has_warning(&diags, "Unknown feature flag \"BogusFlag\""));
        // "Lua" should not produce a warning
        assert!(!has_warning(&diags, "Unknown feature flag \"Lua\""));
    }

    #[test]
    fn feature_flags_non_array_is_error() {
        let content = r#"{"RequiredVersion": 1, "ModTable": "M", "FeatureFlags": "Lua"}"#;
        let diags = validate_se_config(content);
        assert!(has_error(&diags, "must be an array"));
    }

    #[test]
    fn feature_flags_non_string_entry_is_error() {
        let content = r#"{"RequiredVersion": 1, "ModTable": "M", "FeatureFlags": [42]}"#;
        let diags = validate_se_config(content);
        assert!(has_error(&diags, "entries must be strings"));
    }

    #[test]
    fn unknown_top_level_keys_produce_warnings() {
        let content = r#"{
    "RequiredVersion": 1,
    "ModTable": "MyMod",
    "FeatureFlags": ["Lua"],
    "SomethingExtra": true
}"#;
        let diags = validate_se_config(content);
        assert!(has_warning(&diags, "Unknown key \"SomethingExtra\""));
        // No errors — the rest is valid
        assert!(!diags
            .iter()
            .any(|d| matches!(d.severity, DiagnosticSeverity::Error)));
    }

    #[test]
    fn preload_must_be_boolean() {
        let content =
            r#"{"RequiredVersion": 1, "ModTable": "M", "FeatureFlags": [], "Preload": "yes"}"#;
        let diags = validate_se_config(content);
        assert!(has_error(&diags, "must be a boolean"));
    }

    #[test]
    fn line_col_positions_are_accurate() {
        let content = r#"{
    "RequiredVersion": 1,
    "ModTable": "bad-name",
    "FeatureFlags": ["Lua"]
}"#;
        let diags = validate_se_config(content);
        let modtable_diag = diags
            .iter()
            .find(|d| d.message.contains("Lua identifier"))
            .expect("should have ModTable diagnostic");
        // "ModTable" is on line 3
        assert_eq!(modtable_diag.line, 3);
    }
}
