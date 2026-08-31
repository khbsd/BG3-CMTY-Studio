//! Structural validator for Osiris goal files.
//!
//! Catches common structural errors without semantic analysis:
//! - Section structure (INITSECTION → KBSECTION → EXITSECTION → ENDEXITSECTION)
//! - Version header
//! - SubGoalCombiner after Version
//! - ParentTargetEdge as last meaningful section
//! - Unmatched IF/THEN pairs
//! - Unclosed strings

use serde::Serialize;

/// Severity of a diagnostic produced by the Osiris validator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Info,
}

/// A single diagnostic (error/warning) from structural validation.
#[derive(Debug, Clone, Serialize)]
pub struct Diagnostic {
    /// 1-based line number where the issue was detected.
    pub line: usize,
    /// Human-readable description of the problem.
    pub message: String,
    /// Severity level.
    pub severity: DiagnosticSeverity,
}

/// Validate the structural integrity of an Osiris goal file.
///
/// Returns a list of diagnostics. An empty list means the file passed validation.
pub fn validate_osiris_goal(content: &str) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let lines: Vec<&str> = content.lines().collect();

    if lines.is_empty() {
        diagnostics.push(Diagnostic {
            line: 1,
            message: "Goal file is empty".into(),
            severity: DiagnosticSeverity::Error,
        });
        return diagnostics;
    }

    // Track section order
    let mut found_version = false;
    let mut found_sgc = false;
    let mut found_init = false;
    let mut found_kb = false;
    let mut found_exit = false;
    let mut found_endexit = false;
    let mut found_parent_edge = false;

    // Track IF/THEN matching
    let mut if_count: usize = 0;
    let mut then_count: usize = 0;
    let mut last_if_line: usize = 0;

    // Track unclosed strings
    let mut in_string = false;
    let mut string_start_line: usize = 0;

    // Track section ordering (must be sequential)
    let mut last_section_order: u8 = 0; // 1=INIT, 2=KB, 3=EXIT, 4=ENDEXIT

    for (i, line) in lines.iter().enumerate() {
        let line_num = i + 1;
        let trimmed = line.trim();

        // Skip empty lines and comments
        if trimmed.is_empty() || trimmed.starts_with("//") {
            continue;
        }

        // Check for unclosed strings (simple check: odd number of unescaped quotes)
        let quote_count = trimmed.chars().filter(|&c| c == '"').count();
        if in_string {
            // We're inside a string from a previous line (shouldn't happen in Osiris)
            if quote_count % 2 == 1 {
                in_string = false;
            }
        } else if quote_count % 2 == 1 {
            in_string = true;
            string_start_line = line_num;
        }

        // Version check (should be first non-empty line)
        if trimmed.starts_with("Version") {
            if found_version {
                diagnostics.push(Diagnostic {
                    line: line_num,
                    message: "Duplicate Version declaration".into(),
                    severity: DiagnosticSeverity::Error,
                });
            }
            found_version = true;
            if !trimmed.contains("Version 1") {
                diagnostics.push(Diagnostic {
                    line: line_num,
                    message: "Version must be 'Version 1'".into(),
                    severity: DiagnosticSeverity::Warning,
                });
            }
            continue;
        }

        // SubGoalCombiner
        if trimmed.starts_with("SubGoalCombiner") {
            if !found_version {
                diagnostics.push(Diagnostic {
                    line: line_num,
                    message: "SubGoalCombiner before Version declaration".into(),
                    severity: DiagnosticSeverity::Error,
                });
            }
            if found_sgc {
                diagnostics.push(Diagnostic {
                    line: line_num,
                    message: "Duplicate SubGoalCombiner".into(),
                    severity: DiagnosticSeverity::Error,
                });
            }
            found_sgc = true;
            continue;
        }

        // Section keywords
        if trimmed == "INITSECTION" {
            if last_section_order >= 1 {
                diagnostics.push(Diagnostic {
                    line: line_num,
                    message: "Duplicate or out-of-order INITSECTION".into(),
                    severity: DiagnosticSeverity::Error,
                });
            }
            found_init = true;
            last_section_order = 1;
            continue;
        }

        if trimmed == "KBSECTION" {
            if last_section_order >= 2 {
                diagnostics.push(Diagnostic {
                    line: line_num,
                    message: "Duplicate or out-of-order KBSECTION".into(),
                    severity: DiagnosticSeverity::Error,
                });
            }
            if !found_init {
                diagnostics.push(Diagnostic {
                    line: line_num,
                    message: "KBSECTION without preceding INITSECTION".into(),
                    severity: DiagnosticSeverity::Error,
                });
            }
            found_kb = true;
            last_section_order = 2;
            continue;
        }

        if trimmed == "EXITSECTION" {
            if last_section_order >= 3 {
                diagnostics.push(Diagnostic {
                    line: line_num,
                    message: "Duplicate or out-of-order EXITSECTION".into(),
                    severity: DiagnosticSeverity::Error,
                });
            }
            if !found_kb {
                diagnostics.push(Diagnostic {
                    line: line_num,
                    message: "EXITSECTION without preceding KBSECTION".into(),
                    severity: DiagnosticSeverity::Error,
                });
            }
            found_exit = true;
            last_section_order = 3;
            continue;
        }

        if trimmed == "ENDEXITSECTION" {
            if last_section_order >= 4 {
                diagnostics.push(Diagnostic {
                    line: line_num,
                    message: "Duplicate ENDEXITSECTION".into(),
                    severity: DiagnosticSeverity::Error,
                });
            }
            if !found_exit {
                diagnostics.push(Diagnostic {
                    line: line_num,
                    message: "ENDEXITSECTION without preceding EXITSECTION".into(),
                    severity: DiagnosticSeverity::Error,
                });
            }
            found_endexit = true;
            last_section_order = 4;
            continue;
        }

        // ParentTargetEdge
        if trimmed.starts_with("ParentTargetEdge") {
            found_parent_edge = true;
            // Should be after ENDEXITSECTION
            if !found_endexit {
                diagnostics.push(Diagnostic {
                    line: line_num,
                    message: "ParentTargetEdge before ENDEXITSECTION".into(),
                    severity: DiagnosticSeverity::Warning,
                });
            }
            // Validate it has a quoted goal name
            if !trimmed.contains('"') {
                diagnostics.push(Diagnostic {
                    line: line_num,
                    message: "ParentTargetEdge missing quoted goal name".into(),
                    severity: DiagnosticSeverity::Error,
                });
            }
            continue;
        }

        // IF/THEN tracking (only in KBSECTION)
        if found_kb && !found_exit {
            if trimmed == "IF" {
                if_count += 1;
                last_if_line = line_num;
            }
            if trimmed == "THEN" {
                then_count += 1;
            }
        }
    }

    // Post-scan checks

    if !found_version {
        diagnostics.push(Diagnostic {
            line: 1,
            message: "Missing Version declaration (should be first line)".into(),
            severity: DiagnosticSeverity::Error,
        });
    }

    if !found_sgc {
        diagnostics.push(Diagnostic {
            line: 1,
            message: "Missing SubGoalCombiner declaration".into(),
            severity: DiagnosticSeverity::Warning,
        });
    }

    if !found_init {
        diagnostics.push(Diagnostic {
            line: 1,
            message: "Missing INITSECTION".into(),
            severity: DiagnosticSeverity::Error,
        });
    }

    if !found_kb {
        diagnostics.push(Diagnostic {
            line: 1,
            message: "Missing KBSECTION".into(),
            severity: DiagnosticSeverity::Error,
        });
    }

    if !found_exit {
        diagnostics.push(Diagnostic {
            line: 1,
            message: "Missing EXITSECTION".into(),
            severity: DiagnosticSeverity::Error,
        });
    }

    if !found_endexit {
        diagnostics.push(Diagnostic {
            line: 1,
            message: "Missing ENDEXITSECTION".into(),
            severity: DiagnosticSeverity::Error,
        });
    }

    if !found_parent_edge {
        diagnostics.push(Diagnostic {
            line: 1,
            message: "Missing ParentTargetEdge declaration".into(),
            severity: DiagnosticSeverity::Warning,
        });
    }

    // IF/THEN mismatch
    if if_count > then_count {
        diagnostics.push(Diagnostic {
            line: last_if_line,
            message: format!("Unmatched IF: found {if_count} IF(s) but only {then_count} THEN(s)"),
            severity: DiagnosticSeverity::Error,
        });
    } else if then_count > if_count {
        diagnostics.push(Diagnostic {
            line: 1,
            message: format!("Extra THEN: found {then_count} THEN(s) but only {if_count} IF(s)"),
            severity: DiagnosticSeverity::Error,
        });
    }

    // Unclosed string
    if in_string {
        diagnostics.push(Diagnostic {
            line: string_start_line,
            message: "Unclosed string literal".into(),
            severity: DiagnosticSeverity::Error,
        });
    }

    diagnostics
}

/// Cross-validates ParentTargetEdge references between multiple goals.
///
/// `goals` is a slice of (goal_name, file_content) pairs. Returns diagnostics
/// for any ParentTargetEdge that references a non-existent goal name
/// (excluding well-known base game goals).
pub fn cross_validate_parent_edges(goals: &[(&str, &str)]) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    // Build set of known goal names (from this mod's goals)
    let goal_names: std::collections::HashSet<&str> = goals.iter().map(|(name, _)| *name).collect();

    // Well-known base game parent goals that don't need to be in the mod
    let well_known_parents: std::collections::HashSet<&str> =
        ["__Shared_Campaign", "_Global", "__Init"]
            .into_iter()
            .collect();

    for (goal_name, content) in goals {
        for (i, line) in content.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with("ParentTargetEdge") {
                // Extract the quoted parent name
                if let Some(start) = trimmed.find('"') {
                    if let Some(end) = trimmed[start + 1..].find('"') {
                        let parent = &trimmed[start + 1..start + 1 + end];
                        if !goal_names.contains(parent) && !well_known_parents.contains(parent) {
                            diagnostics.push(Diagnostic {
                                line: i + 1,
                                message: format!(
                                    "Goal '{goal_name}': ParentTargetEdge references unknown goal '{parent}'"
                                ),
                                severity: DiagnosticSeverity::Warning,
                            });
                        }
                    }
                }
            }
        }
    }

    diagnostics
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_GOAL: &str = "\
Version 1
SubGoalCombiner SGC_AND
INITSECTION
DB_MyMod_Init(1);
KBSECTION
IF
CharacterJoinedParty(_Player)
THEN
DB_MyMod_Party(_Player);
EXITSECTION
NOT DB_MyMod_Init(1);
ENDEXITSECTION
ParentTargetEdge \"__Shared_Campaign\"";

    #[test]
    fn valid_goal_passes() {
        let diags = validate_osiris_goal(VALID_GOAL);
        let errors: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == DiagnosticSeverity::Error)
            .collect();
        assert!(errors.is_empty(), "Expected no errors, got: {errors:?}");
    }

    #[test]
    fn empty_file_returns_error() {
        let diags = validate_osiris_goal("");
        assert!(!diags.is_empty());
        assert!(diags[0].message.contains("empty"));
    }

    #[test]
    fn missing_version_detected() {
        let content = "SubGoalCombiner SGC_AND\nINITSECTION\nKBSECTION\nEXITSECTION\nENDEXITSECTION\nParentTargetEdge \"Test\"";
        let diags = validate_osiris_goal(content);
        assert!(diags.iter().any(|d| d.message.contains("Missing Version")));
    }

    #[test]
    fn missing_sections_detected() {
        let content = "Version 1\nSubGoalCombiner SGC_AND";
        let diags = validate_osiris_goal(content);
        assert!(diags
            .iter()
            .any(|d| d.message.contains("Missing INITSECTION")));
        assert!(diags
            .iter()
            .any(|d| d.message.contains("Missing KBSECTION")));
        assert!(diags
            .iter()
            .any(|d| d.message.contains("Missing EXITSECTION")));
        assert!(diags
            .iter()
            .any(|d| d.message.contains("Missing ENDEXITSECTION")));
    }

    #[test]
    fn unmatched_if_then_detected() {
        let content = "Version 1\nSubGoalCombiner SGC_AND\nINITSECTION\nKBSECTION\nIF\nCondition(_X)\nAND\nCondition2(_X)\nEXITSECTION\nENDEXITSECTION\nParentTargetEdge \"Test\"";
        let diags = validate_osiris_goal(content);
        assert!(diags.iter().any(|d| d.message.contains("Unmatched IF")));
    }

    #[test]
    fn out_of_order_sections_detected() {
        let content = "Version 1\nSubGoalCombiner SGC_AND\nKBSECTION\nINITSECTION\nEXITSECTION\nENDEXITSECTION\nParentTargetEdge \"Test\"";
        let diags = validate_osiris_goal(content);
        assert!(diags
            .iter()
            .any(|d| d.message.contains("without preceding INITSECTION")
                || d.message.contains("out-of-order")));
    }

    #[test]
    fn unclosed_string_detected() {
        let content = "Version 1\nSubGoalCombiner SGC_AND\nINITSECTION\nDB_Test(\"unclosed);\nKBSECTION\nEXITSECTION\nENDEXITSECTION\nParentTargetEdge \"Test\"";
        let diags = validate_osiris_goal(content);
        assert!(diags.iter().any(|d| d.message.contains("Unclosed string")));
    }

    #[test]
    fn parent_edge_without_quote_detected() {
        let content = "Version 1\nSubGoalCombiner SGC_AND\nINITSECTION\nKBSECTION\nEXITSECTION\nENDEXITSECTION\nParentTargetEdge NoQuotes";
        let diags = validate_osiris_goal(content);
        assert!(diags
            .iter()
            .any(|d| d.message.contains("missing quoted goal name")));
    }

    #[test]
    fn cross_validate_detects_unknown_parent() {
        let goals = vec![
            ("MyGoal", "Version 1\nSubGoalCombiner SGC_AND\nINITSECTION\nKBSECTION\nEXITSECTION\nENDEXITSECTION\nParentTargetEdge \"NonExistentGoal\""),
        ];
        let diags = cross_validate_parent_edges(&goals);
        assert!(!diags.is_empty());
        assert!(diags[0].message.contains("NonExistentGoal"));
    }

    #[test]
    fn cross_validate_allows_well_known_parents() {
        let goals = vec![
            ("MyGoal", "Version 1\nSubGoalCombiner SGC_AND\nINITSECTION\nKBSECTION\nEXITSECTION\nENDEXITSECTION\nParentTargetEdge \"__Shared_Campaign\""),
        ];
        let diags = cross_validate_parent_edges(&goals);
        assert!(diags.is_empty());
    }

    #[test]
    fn cross_validate_allows_mod_own_goals() {
        let goals = vec![
            ("ParentGoal", "Version 1\nSubGoalCombiner SGC_AND\nINITSECTION\nKBSECTION\nEXITSECTION\nENDEXITSECTION\nParentTargetEdge \"__Shared_Campaign\""),
            ("ChildGoal", "Version 1\nSubGoalCombiner SGC_AND\nINITSECTION\nKBSECTION\nEXITSECTION\nENDEXITSECTION\nParentTargetEdge \"ParentGoal\""),
        ];
        let diags = cross_validate_parent_edges(&goals);
        assert!(diags.is_empty());
    }

    #[test]
    fn duplicate_version_detected() {
        let content = "Version 1\nVersion 1\nSubGoalCombiner SGC_AND\nINITSECTION\nKBSECTION\nEXITSECTION\nENDEXITSECTION\nParentTargetEdge \"Test\"";
        let diags = validate_osiris_goal(content);
        assert!(diags
            .iter()
            .any(|d| d.message.contains("Duplicate Version")));
    }

    #[test]
    fn sgc_before_version_detected() {
        let content = "SubGoalCombiner SGC_AND\nVersion 1\nINITSECTION\nKBSECTION\nEXITSECTION\nENDEXITSECTION\nParentTargetEdge \"Test\"";
        let diags = validate_osiris_goal(content);
        assert!(diags
            .iter()
            .any(|d| d.message.contains("SubGoalCombiner before Version")));
    }

    #[test]
    fn comments_and_blank_lines_are_skipped() {
        let content = "// Header comment\n\nVersion 1\n// Another comment\nSubGoalCombiner SGC_AND\nINITSECTION\nKBSECTION\nEXITSECTION\nENDEXITSECTION\nParentTargetEdge \"Test\"";
        let diags = validate_osiris_goal(content);
        let errors: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == DiagnosticSeverity::Error)
            .collect();
        assert!(errors.is_empty(), "Expected no errors, got: {errors:?}");
    }

    #[test]
    fn extra_then_detected() {
        let content = "Version 1\nSubGoalCombiner SGC_AND\nINITSECTION\nKBSECTION\nTHEN\nDB_Oops(1);\nEXITSECTION\nENDEXITSECTION\nParentTargetEdge \"Test\"";
        let diags = validate_osiris_goal(content);
        assert!(diags.iter().any(|d| d.message.contains("Extra THEN")));
    }
}
