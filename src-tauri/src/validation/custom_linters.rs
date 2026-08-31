//! Custom linting system that loads regex-based lint rules from
//! `.cmtystudio/linters/*.json` files and applies them to script content.

use regex::Regex;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

// ── JSON schema types (deserialized from linter module files) ──

#[derive(Debug, Clone, Deserialize)]
pub struct LinterModuleJson {
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
    pub languages: Vec<String>,
    pub rules: Vec<LintRuleJson>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LintRuleJson {
    pub id: String,
    pub severity: String,
    pub pattern: String,
    #[serde(default)]
    pub flags: Option<String>,
    pub message: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub fixable: Option<bool>,
    #[serde(default)]
    pub negate: Option<bool>,
    #[serde(default)]
    pub block_pattern: Option<String>,
    #[serde(default)]
    pub multiline_until: Option<String>,
}

// ── Compiled types ──

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Info,
}

pub struct CustomLinterModule {
    pub filename: String,
    pub name: String,
    pub languages: Vec<String>,
    pub rules: Vec<CustomLintRule>,
}

pub struct CustomLintRule {
    pub id: String,
    pub severity: DiagnosticSeverity,
    pub pattern: Regex,
    pub message_template: String,
    pub negate: bool,
    pub block_pattern: Option<Regex>,
    pub multiline_until: Option<Regex>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CustomDiagnostic {
    pub line: usize,
    pub message: String,
    pub severity: DiagnosticSeverity,
    pub source: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct LintConfig {
    #[serde(default)]
    pub rules: HashMap<String, String>,
    #[serde(default)]
    pub ignore: Vec<String>,
}

// ── Size limit for linter JSON files (1 MB) ──
const MAX_LINTER_FILE_SIZE: u64 = 1024 * 1024;

// ── Core functions ──

/// Load all valid linter modules from `.cmtystudio/linters/` directory.
/// Skips files that fail to parse or have invalid regexes (logs warnings).
/// Skips `_config.json` (that's the override file, not a linter module).
pub fn load_custom_linters(project_path: &str) -> Vec<CustomLinterModule> {
    let linters_dir = PathBuf::from(project_path)
        .join(".cmtystudio")
        .join("linters");

    if !linters_dir.is_dir() {
        return Vec::new();
    }

    let entries = match std::fs::read_dir(&linters_dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("Failed to read linters directory: {e}");
            return Vec::new();
        }
    };

    let mut modules = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let file_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };

        // Skip _config.json
        if file_name == "_config.json" {
            continue;
        }

        // Only process .json files
        if !file_name.ends_with(".json") {
            continue;
        }

        // Enforce file size limit
        if let Ok(meta) = std::fs::metadata(&path) {
            if meta.len() > MAX_LINTER_FILE_SIZE {
                tracing::warn!(
                    "Linter file {} exceeds size limit ({} > {}), skipping",
                    file_name,
                    meta.len(),
                    MAX_LINTER_FILE_SIZE
                );
                continue;
            }
        }

        let stem = file_name.trim_end_matches(".json").to_string();

        // Validate filename doesn't contain path traversal
        if stem.contains("..") || stem.contains('/') || stem.contains('\\') || stem.contains('\0') {
            tracing::warn!("Linter filename contains invalid characters: {file_name}");
            continue;
        }

        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("Failed to read linter file {file_name}: {e}");
                continue;
            }
        };

        let module_json: LinterModuleJson = match serde_json::from_str(&content) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("Failed to parse linter file {file_name}: {e}");
                continue;
            }
        };

        match compile_linter_module(&stem, module_json) {
            Ok(module) => modules.push(module),
            Err(e) => {
                tracing::warn!("Failed to compile linter module {file_name}: {e}");
            }
        }
    }

    modules
}

/// Load `_config.json` from `.cmtystudio/linters/`.
/// Returns default (empty) config if file doesn't exist or is invalid.
pub fn load_lint_config(project_path: &str) -> LintConfig {
    let config_path = PathBuf::from(project_path)
        .join(".cmtystudio")
        .join("linters")
        .join("_config.json");

    if !config_path.is_file() {
        return LintConfig::default();
    }

    // Enforce file size limit
    if let Ok(meta) = std::fs::metadata(&config_path) {
        if meta.len() > MAX_LINTER_FILE_SIZE {
            tracing::warn!("Lint config file exceeds size limit, using defaults");
            return LintConfig::default();
        }
    }

    let content = match std::fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("Failed to read lint config: {e}");
            return LintConfig::default();
        }
    };

    match serde_json::from_str(&content) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("Failed to parse lint config: {e}");
            LintConfig::default()
        }
    }
}

/// Apply custom linters to content for a given language.
/// Returns diagnostics with namespaced rule IDs as the source.
pub fn apply_custom_linters(
    content: &str,
    language: &str,
    file_path: &str,
    linters: &[CustomLinterModule],
    config: &LintConfig,
) -> Vec<CustomDiagnostic> {
    // Check file-level ignore
    if is_file_ignored(file_path, &config.ignore) {
        return Vec::new();
    }

    let lines: Vec<&str> = content.lines().collect();
    let mut diagnostics = Vec::new();

    for linter in linters {
        // Check if this linter applies to the given language
        if !linter
            .languages
            .iter()
            .any(|l| l == "*" || l.eq_ignore_ascii_case(language))
        {
            continue;
        }

        for rule in &linter.rules {
            // Check if rule is disabled by config
            if let Some(override_val) = config.rules.get(&rule.id) {
                if override_val.eq_ignore_ascii_case("off") {
                    continue;
                }
            }

            // Determine effective severity (config override or rule default)
            let severity = config
                .rules
                .get(&rule.id)
                .and_then(|s| parse_severity(s))
                .unwrap_or(rule.severity);

            if rule.multiline_until.is_some() {
                // Block rule
                apply_block_rule(&lines, rule, severity, &mut diagnostics);
            } else {
                // Per-line rule
                apply_line_rule(&lines, rule, severity, &mut diagnostics);
            }
        }
    }

    // Apply inline suppression
    apply_suppressions(&lines, &mut diagnostics);

    diagnostics
}

// ── Internal helpers ──

/// Compile a parsed JSON module into a `CustomLinterModule` with compiled regexes.
fn compile_linter_module(
    filename: &str,
    json: LinterModuleJson,
) -> Result<CustomLinterModule, String> {
    let mut rules = Vec::with_capacity(json.rules.len());

    for rule_json in &json.rules {
        let severity = parse_severity(&rule_json.severity).ok_or_else(|| {
            format!(
                "Invalid severity '{}' for rule {}",
                rule_json.severity, rule_json.id
            )
        })?;

        let pattern = compile_regex(&rule_json.pattern, rule_json.flags.as_deref())
            .map_err(|e| format!("Invalid pattern for rule {}: {e}", rule_json.id))?;

        let block_pattern = rule_json
            .block_pattern
            .as_deref()
            .map(|p| compile_regex(p, rule_json.flags.as_deref()))
            .transpose()
            .map_err(|e| format!("Invalid block_pattern for rule {}: {e}", rule_json.id))?;

        let multiline_until = rule_json
            .multiline_until
            .as_deref()
            .map(|p| compile_regex(p, None))
            .transpose()
            .map_err(|e| format!("Invalid multiline_until for rule {}: {e}", rule_json.id))?;

        rules.push(CustomLintRule {
            id: format!("{}/{}", filename, rule_json.id),
            severity,
            pattern,
            message_template: rule_json.message.clone(),
            negate: rule_json.negate.unwrap_or(false),
            block_pattern,
            multiline_until,
        });
    }

    Ok(CustomLinterModule {
        filename: filename.to_string(),
        name: json.name,
        languages: json.languages,
        rules,
    })
}

/// Compile a regex pattern with optional flags.
fn compile_regex(pattern: &str, flags: Option<&str>) -> Result<Regex, String> {
    let mut regex_str = String::new();

    // Apply flags as inline modifiers
    if let Some(f) = flags {
        if f.contains('i') {
            regex_str.push_str("(?i)");
        }
        // Note: 'g' is handled by find_iter vs find; 'm' is handled by regex crate default
        if f.contains('m') {
            regex_str.push_str("(?m)");
        }
        if f.contains('s') {
            regex_str.push_str("(?s)");
        }
    }

    regex_str.push_str(pattern);

    Regex::new(&regex_str).map_err(|e| e.to_string())
}

/// Parse a severity string into a `DiagnosticSeverity`.
fn parse_severity(s: &str) -> Option<DiagnosticSeverity> {
    match s.to_lowercase().as_str() {
        "error" => Some(DiagnosticSeverity::Error),
        "warning" => Some(DiagnosticSeverity::Warning),
        "info" => Some(DiagnosticSeverity::Info),
        _ => None,
    }
}

/// Substitute `$1`..$9` in a message template using regex captures.
fn substitute_captures(template: &str, captures: &regex::Captures) -> String {
    let mut result = template.to_string();
    for i in 1..=9 {
        let placeholder = format!("${i}");
        if let Some(m) = captures.get(i) {
            result = result.replace(&placeholder, m.as_str());
        } else {
            result = result.replace(&placeholder, "");
        }
    }
    result
}

/// Apply a per-line rule to all lines.
fn apply_line_rule(
    lines: &[&str],
    rule: &CustomLintRule,
    severity: DiagnosticSeverity,
    diagnostics: &mut Vec<CustomDiagnostic>,
) {
    for (idx, line) in lines.iter().enumerate() {
        let line_num = idx + 1; // 1-based

        if rule.negate {
            // Negate: emit when pattern does NOT match
            if !rule.pattern.is_match(line) {
                diagnostics.push(CustomDiagnostic {
                    line: line_num,
                    message: rule.message_template.clone(),
                    severity,
                    source: rule.id.clone(),
                });
            }
        } else {
            // Normal: emit when pattern matches
            if let Some(captures) = rule.pattern.captures(line) {
                let message = substitute_captures(&rule.message_template, &captures);
                diagnostics.push(CustomDiagnostic {
                    line: line_num,
                    message,
                    severity,
                    source: rule.id.clone(),
                });
            }
        }
    }
}

/// Apply a block rule: collect lines from pattern match until delimiter, then test block_pattern.
fn apply_block_rule(
    lines: &[&str],
    rule: &CustomLintRule,
    severity: DiagnosticSeverity,
    diagnostics: &mut Vec<CustomDiagnostic>,
) {
    let multiline_until = match &rule.multiline_until {
        Some(r) => r,
        None => return,
    };

    let mut i = 0;
    while i < lines.len() {
        if let Some(start_captures) = rule.pattern.captures(lines[i]) {
            let block_start = i;
            let mut block_lines = vec![lines[i]];
            i += 1;

            // Collect until delimiter
            while i < lines.len() {
                block_lines.push(lines[i]);
                if multiline_until.is_match(lines[i]) {
                    break;
                }
                i += 1;
            }

            let block_text = block_lines.join("\n");

            if let Some(ref block_pat) = rule.block_pattern {
                if rule.negate {
                    // Negate: emit when block_pattern does NOT match within block
                    if !block_pat.is_match(&block_text) {
                        let message = substitute_captures(&rule.message_template, &start_captures);
                        diagnostics.push(CustomDiagnostic {
                            line: block_start + 1,
                            message,
                            severity,
                            source: rule.id.clone(),
                        });
                    }
                } else {
                    // Normal: emit when block_pattern matches within block
                    if block_pat.is_match(&block_text) {
                        let message = substitute_captures(&rule.message_template, &start_captures);
                        diagnostics.push(CustomDiagnostic {
                            line: block_start + 1,
                            message,
                            severity,
                            source: rule.id.clone(),
                        });
                    }
                }
            } else {
                // No block_pattern — just report the start of the block
                let message = substitute_captures(&rule.message_template, &start_captures);
                diagnostics.push(CustomDiagnostic {
                    line: block_start + 1,
                    message,
                    severity,
                    source: rule.id.clone(),
                });
            }
        }
        i += 1;
    }
}

/// Parse suppression comments and remove matching diagnostics.
fn apply_suppressions(lines: &[&str], diagnostics: &mut Vec<CustomDiagnostic>) {
    // Build a set of (line, rule_id) pairs to suppress
    let mut suppressed: Vec<(usize, String)> = Vec::new();

    for (idx, line) in lines.iter().enumerate() {
        let line_num = idx + 1;

        // Check for disable-line on this line
        if let Some(rule_ids) = parse_suppression(line, "cmty-lint-disable-line") {
            for rule_id in rule_ids {
                suppressed.push((line_num, rule_id));
            }
        }

        // Check for disable-next-line on this line (suppresses line_num + 1)
        if let Some(rule_ids) = parse_suppression(line, "cmty-lint-disable-next-line") {
            for rule_id in rule_ids {
                suppressed.push((line_num + 1, rule_id));
            }
        }
    }

    if suppressed.is_empty() {
        return;
    }

    diagnostics.retain(|d| {
        !suppressed
            .iter()
            .any(|(line, rule_id)| d.line == *line && d.source == *rule_id)
    });
}

/// Parse a suppression comment from a line of text.
/// Supports `//`, `--`, and `<!-- -->` comment styles.
/// Returns `None` if no suppression directive found.
fn parse_suppression(line: &str, directive: &str) -> Option<Vec<String>> {
    let lower = line.to_lowercase();

    // Find the directive in the line (case-insensitive)
    let directive_lower = directive.to_lowercase();
    let pos = lower.find(&directive_lower)?;

    // Verify it's preceded by a valid comment leader
    let before = &line[..pos];
    let is_comment = before.trim_end().ends_with("//")
        || before.trim_end().ends_with("--")
        || before.trim_end().ends_with("<!--");

    if !is_comment {
        return None;
    }

    // Extract rule IDs after the directive
    let after = &line[pos + directive.len()..];
    // Strip trailing XML comment close
    let after = after.trim();
    let after = after.strip_suffix("-->").unwrap_or(after).trim();

    if after.is_empty() {
        return None;
    }

    let rule_ids: Vec<String> = after
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    if rule_ids.is_empty() {
        None
    } else {
        Some(rule_ids)
    }
}

/// Check if a file path matches any of the ignore patterns.
/// Uses simple glob-like matching (supports `*` and `**`).
fn is_file_ignored(file_path: &str, patterns: &[String]) -> bool {
    let normalized = file_path.replace('\\', "/");
    patterns
        .iter()
        .any(|pattern| glob_match(pattern, &normalized))
}

/// Simple glob matching for ignore patterns.
/// Supports `*` (any chars except `/`), `**` (any chars including `/`),
/// and `?` (single char).
fn glob_match(pattern: &str, path: &str) -> bool {
    let pat = pattern.replace('\\', "/");

    // Convert glob pattern to regex
    let mut regex_str = String::from("(?i)^");
    let chars: Vec<char> = pat.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        match chars[i] {
            '*' => {
                if i + 1 < chars.len() && chars[i + 1] == '*' {
                    // ** matches everything including /
                    regex_str.push_str(".*");
                    i += 2;
                    // Skip trailing /
                    if i < chars.len() && chars[i] == '/' {
                        regex_str.push_str("/?");
                        i += 1;
                    }
                } else {
                    // * matches everything except /
                    regex_str.push_str("[^/]*");
                    i += 1;
                }
            }
            '?' => {
                regex_str.push_str("[^/]");
                i += 1;
            }
            '.' | '+' | '(' | ')' | '{' | '}' | '[' | ']' | '^' | '$' | '|' => {
                regex_str.push('\\');
                regex_str.push(chars[i]);
                i += 1;
            }
            c => {
                regex_str.push(c);
                i += 1;
            }
        }
    }

    regex_str.push('$');

    Regex::new(&regex_str)
        .map(|re| re.is_match(path))
        .unwrap_or(false)
}

// ── Metadata type for cmd_list_custom_linters ──

#[derive(Debug, Clone, serde::Serialize)]
pub struct LinterModuleInfo {
    pub filename: String,
    pub name: String,
    pub languages: Vec<String>,
    pub rule_count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_severity_values() {
        assert_eq!(parse_severity("error"), Some(DiagnosticSeverity::Error));
        assert_eq!(parse_severity("Warning"), Some(DiagnosticSeverity::Warning));
        assert_eq!(parse_severity("INFO"), Some(DiagnosticSeverity::Info));
        assert_eq!(parse_severity("off"), None);
        assert_eq!(parse_severity("unknown"), None);
    }

    #[test]
    fn substitute_captures_works() {
        let re = Regex::new(r"(foo)(bar)").unwrap();
        let text = "foobar";
        let caps = re.captures(text).unwrap();
        assert_eq!(
            substitute_captures("Found $1 and $2", &caps),
            "Found foo and bar"
        );
    }

    #[test]
    fn substitute_captures_missing_groups() {
        let re = Regex::new(r"(hello)").unwrap();
        let text = "hello";
        let caps = re.captures(text).unwrap();
        assert_eq!(substitute_captures("$1 $2 $3", &caps), "hello  ");
    }

    #[test]
    fn suppression_line_comment() {
        let rule_ids = parse_suppression(
            "  some code // cmty-lint-disable-line my-rule/foo",
            "cmty-lint-disable-line",
        );
        assert_eq!(rule_ids, Some(vec!["my-rule/foo".to_string()]));
    }

    #[test]
    fn suppression_lua_comment() {
        let rule_ids = parse_suppression(
            "  local x = 1 -- cmty-lint-disable-line rule1, rule2",
            "cmty-lint-disable-line",
        );
        assert_eq!(
            rule_ids,
            Some(vec!["rule1".to_string(), "rule2".to_string()])
        );
    }

    #[test]
    fn suppression_xml_comment() {
        let rule_ids = parse_suppression(
            "<!-- cmty-lint-disable-line my-rule/foo -->",
            "cmty-lint-disable-line",
        );
        assert_eq!(rule_ids, Some(vec!["my-rule/foo".to_string()]));
    }

    #[test]
    fn suppression_no_comment_leader() {
        let rule_ids = parse_suppression(
            "cmty-lint-disable-line my-rule/foo",
            "cmty-lint-disable-line",
        );
        assert_eq!(rule_ids, None);
    }

    #[test]
    fn glob_match_simple() {
        assert!(glob_match("*.lua", "test.lua"));
        assert!(!glob_match("*.lua", "test.txt"));
        assert!(glob_match("**/*.lua", "Scripts/foo/test.lua"));
        assert!(glob_match("Scripts/**", "Scripts/foo/bar.lua"));
        assert!(!glob_match("Scripts/*", "Scripts/foo/bar.lua"));
        assert!(glob_match("Scripts/*", "Scripts/bar.lua"));
    }

    #[test]
    fn glob_match_case_insensitive() {
        assert!(glob_match("*.LUA", "test.lua"));
        assert!(glob_match("scripts/**", "Scripts/foo.lua"));
    }

    #[test]
    fn simple_line_rule() {
        let rule = CustomLintRule {
            id: "test/deprecated".to_string(),
            severity: DiagnosticSeverity::Warning,
            pattern: Regex::new(r"DEPRECATED_(\w+)").unwrap(),
            message_template: "Don't use deprecated function $1".to_string(),
            negate: false,
            block_pattern: None,
            multiline_until: None,
        };

        let lines = vec!["local x = 1", "call DEPRECATED_foo()", "local y = 2"];
        let mut diags = Vec::new();
        apply_line_rule(&lines, &rule, DiagnosticSeverity::Warning, &mut diags);

        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].line, 2);
        assert_eq!(diags[0].message, "Don't use deprecated function foo");
        assert_eq!(diags[0].source, "test/deprecated");
    }

    #[test]
    fn negated_line_rule() {
        let rule = CustomLintRule {
            id: "test/require-header".to_string(),
            severity: DiagnosticSeverity::Info,
            pattern: Regex::new(r"^--\s*@module").unwrap(),
            message_template: "Missing @module header".to_string(),
            negate: true,
            block_pattern: None,
            multiline_until: None,
        };

        let lines = vec!["local x = 1", "-- @module Foo", "local y = 2"];
        let mut diags = Vec::new();
        apply_line_rule(&lines, &rule, DiagnosticSeverity::Info, &mut diags);

        // Lines 1 and 3 don't match the pattern → emit diagnostics (negate)
        assert_eq!(diags.len(), 2);
        assert_eq!(diags[0].line, 1);
        assert_eq!(diags[1].line, 3);
    }

    #[test]
    fn file_ignore_config() {
        let patterns = vec!["**/*.generated.lua".to_string(), "vendor/**".to_string()];
        assert!(is_file_ignored("Scripts/foo.generated.lua", &patterns));
        assert!(is_file_ignored("vendor/lib/util.lua", &patterns));
        assert!(!is_file_ignored("Scripts/main.lua", &patterns));
    }

    #[test]
    fn suppression_removes_matching_diags() {
        let lines = vec![
            "local x = DEPRECATED_foo() // cmty-lint-disable-line test/deprecated",
            "-- cmty-lint-disable-next-line test/other",
            "local y = BAD_CALL()",
        ];
        let mut diags = vec![
            CustomDiagnostic {
                line: 1,
                message: "deprecated".to_string(),
                severity: DiagnosticSeverity::Warning,
                source: "test/deprecated".to_string(),
            },
            CustomDiagnostic {
                line: 3,
                message: "bad call".to_string(),
                severity: DiagnosticSeverity::Error,
                source: "test/other".to_string(),
            },
        ];
        apply_suppressions(&lines, &mut diags);
        assert!(diags.is_empty());
    }

    #[test]
    fn default_lint_config() {
        let config = LintConfig::default();
        assert!(config.rules.is_empty());
        assert!(config.ignore.is_empty());
    }

    #[test]
    fn compile_module_invalid_regex_fails() {
        let json = LinterModuleJson {
            name: "test".to_string(),
            version: None,
            description: None,
            author: None,
            languages: vec!["lua".to_string()],
            rules: vec![LintRuleJson {
                id: "bad-regex".to_string(),
                severity: "error".to_string(),
                pattern: "[invalid".to_string(),
                flags: None,
                message: "test".to_string(),
                description: None,
                url: None,
                fixable: None,
                negate: None,
                block_pattern: None,
                multiline_until: None,
            }],
        };

        let result = compile_linter_module("test", json);
        assert!(result.is_err());
    }

    #[test]
    fn compile_module_success() {
        let json = LinterModuleJson {
            name: "My Linter".to_string(),
            version: Some("1.0".to_string()),
            description: None,
            author: None,
            languages: vec!["lua".to_string(), "osiris".to_string()],
            rules: vec![LintRuleJson {
                id: "no-print".to_string(),
                severity: "warning".to_string(),
                pattern: r"\bprint\b".to_string(),
                flags: None,
                message: "Avoid using print()".to_string(),
                description: None,
                url: None,
                fixable: None,
                negate: None,
                block_pattern: None,
                multiline_until: None,
            }],
        };

        let module = compile_linter_module("my-linter", json).unwrap();
        assert_eq!(module.filename, "my-linter");
        assert_eq!(module.name, "My Linter");
        assert_eq!(module.rules.len(), 1);
        assert_eq!(module.rules[0].id, "my-linter/no-print");
    }

    #[test]
    fn apply_custom_linters_language_filter() {
        let json = LinterModuleJson {
            name: "Lua Only".to_string(),
            version: None,
            description: None,
            author: None,
            languages: vec!["lua".to_string()],
            rules: vec![LintRuleJson {
                id: "no-print".to_string(),
                severity: "warning".to_string(),
                pattern: r"\bprint\b".to_string(),
                flags: None,
                message: "Avoid print".to_string(),
                description: None,
                url: None,
                fixable: None,
                negate: None,
                block_pattern: None,
                multiline_until: None,
            }],
        };

        let module = compile_linter_module("test", json).unwrap();
        let linters = vec![module];
        let config = LintConfig::default();

        // Matching language
        let diags = apply_custom_linters("print()", "lua", "test.lua", &linters, &config);
        assert_eq!(diags.len(), 1);

        // Non-matching language
        let diags = apply_custom_linters("print()", "osiris", "test.txt", &linters, &config);
        assert!(diags.is_empty());
    }

    #[test]
    fn apply_custom_linters_wildcard_language() {
        let json = LinterModuleJson {
            name: "Universal".to_string(),
            version: None,
            description: None,
            author: None,
            languages: vec!["*".to_string()],
            rules: vec![LintRuleJson {
                id: "no-todo".to_string(),
                severity: "info".to_string(),
                pattern: r"TODO".to_string(),
                flags: Some("i".to_string()),
                message: "TODO found".to_string(),
                description: None,
                url: None,
                fixable: None,
                negate: None,
                block_pattern: None,
                multiline_until: None,
            }],
        };

        let module = compile_linter_module("test", json).unwrap();
        let linters = vec![module];
        let config = LintConfig::default();

        let diags =
            apply_custom_linters("todo: fix this", "anything", "test.lua", &linters, &config);
        assert_eq!(diags.len(), 1);
    }

    #[test]
    fn config_override_severity() {
        let json = LinterModuleJson {
            name: "Test".to_string(),
            version: None,
            description: None,
            author: None,
            languages: vec!["*".to_string()],
            rules: vec![LintRuleJson {
                id: "my-rule".to_string(),
                severity: "error".to_string(),
                pattern: r"BAD".to_string(),
                flags: None,
                message: "Bad found".to_string(),
                description: None,
                url: None,
                fixable: None,
                negate: None,
                block_pattern: None,
                multiline_until: None,
            }],
        };

        let module = compile_linter_module("test", json).unwrap();
        let linters = vec![module];

        // Override severity to info
        let config = LintConfig {
            rules: [("test/my-rule".to_string(), "info".to_string())].into(),
            ignore: Vec::new(),
        };

        let diags = apply_custom_linters("BAD", "lua", "test.lua", &linters, &config);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].severity, DiagnosticSeverity::Info);
    }

    #[test]
    fn config_disable_rule() {
        let json = LinterModuleJson {
            name: "Test".to_string(),
            version: None,
            description: None,
            author: None,
            languages: vec!["*".to_string()],
            rules: vec![LintRuleJson {
                id: "my-rule".to_string(),
                severity: "error".to_string(),
                pattern: r"BAD".to_string(),
                flags: None,
                message: "Bad found".to_string(),
                description: None,
                url: None,
                fixable: None,
                negate: None,
                block_pattern: None,
                multiline_until: None,
            }],
        };

        let module = compile_linter_module("test", json).unwrap();
        let linters = vec![module];

        let config = LintConfig {
            rules: [("test/my-rule".to_string(), "off".to_string())].into(),
            ignore: Vec::new(),
        };

        let diags = apply_custom_linters("BAD", "lua", "test.lua", &linters, &config);
        assert!(diags.is_empty());
    }
}
