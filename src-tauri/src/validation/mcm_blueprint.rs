//! Validation for MCM (Mod Configuration Menu) blueprint JSON files.
//!
//! Validates against the MCM Schema published at
//! <https://github.com/AtilioA/BG3-MCM/blob/main/.vscode/schema.json>.
//!
//! Parses JSON content and returns a `Vec<Diagnostic>` with line/column
//! positions for each issue found. An empty vec means the file is valid.

use serde_json::Value;
use std::collections::{HashMap, HashSet};

/// Valid MCM widget type names (MCM Schema 1.x).
const VALID_WIDGET_TYPES: &[&str] = &[
    "int",
    "float",
    "checkbox",
    "text",
    "enum",
    "radio",
    "slider_int",
    "slider_float",
    "drag_int",
    "drag_float",
    "list_v2",
    "color_picker",
    "color_edit",
    "keybinding_v2",
    "event_button",
];

// Re-export the Diagnostic types from se_config so we share a single type
// across all JSON-based validators.
pub use super::se_config::{Diagnostic, DiagnosticSeverity};

// ── Entry point ──

/// Validate the raw text of an MCM blueprint JSON file.
///
/// Returns diagnostics with 1-based line and column positions. An empty
/// `Vec` means the content is valid.
pub fn validate_mcm_blueprint(content: &str) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    // ── Step 1: parse JSON ──
    let root: Value = match serde_json::from_str(content) {
        Ok(v) => v,
        Err(e) => {
            diagnostics.push(Diagnostic {
                line: e.line(),
                col: e.column(),
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
                message: "MCM blueprint must be a JSON object".into(),
            });
            return diagnostics;
        }
    };

    // ── Step 2: SchemaVersion (required integer) ──
    match obj.get("SchemaVersion") {
        None => {
            diagnostics.push(Diagnostic {
                line: 1,
                col: 1,
                severity: DiagnosticSeverity::Error,
                message: "Missing required key \"SchemaVersion\"".into(),
            });
        }
        Some(val) => {
            if !val.is_i64() && !val.is_u64() {
                let (line, col) = find_key_position(content, "SchemaVersion");
                diagnostics.push(Diagnostic {
                    line,
                    col,
                    severity: DiagnosticSeverity::Error,
                    message: "\"SchemaVersion\" must be an integer".into(),
                });
            }
        }
    }

    // ── Step 3: Must have at least one of Tabs, Sections, or Settings ──
    let has_tabs = obj.get("Tabs").and_then(|v| v.as_array()).is_some();
    let has_sections = obj.get("Sections").and_then(|v| v.as_array()).is_some();
    let has_settings = obj.get("Settings").and_then(|v| v.as_array()).is_some();

    if !has_tabs && !has_sections && !has_settings {
        diagnostics.push(Diagnostic {
            line: 1,
            col: 1,
            severity: DiagnosticSeverity::Error,
            message:
                "Blueprint must contain at least one of \"Tabs\", \"Sections\", or \"Settings\""
                    .into(),
        });
        return diagnostics;
    }

    // Collect all setting IDs for uniqueness and cross-reference checks.
    let mut all_setting_ids: HashMap<String, usize> = HashMap::new();
    collect_all_setting_ids(obj, &mut all_setting_ids);

    // Track IDs seen so far for duplicate reporting at the right position.
    let mut seen_ids: HashSet<String> = HashSet::new();

    // ── Step 4: validate recursively ──
    if let Some(tabs) = obj.get("Tabs").and_then(|v| v.as_array()) {
        validate_tabs(
            content,
            tabs,
            "Tabs",
            &mut seen_ids,
            &all_setting_ids,
            &mut diagnostics,
        );
    }
    if let Some(sections) = obj.get("Sections").and_then(|v| v.as_array()) {
        validate_sections(
            content,
            sections,
            "Sections",
            &mut seen_ids,
            &all_setting_ids,
            &mut diagnostics,
        );
    }
    if let Some(settings) = obj.get("Settings").and_then(|v| v.as_array()) {
        validate_settings_array(
            content,
            settings,
            "Settings",
            &mut seen_ids,
            &all_setting_ids,
            &mut diagnostics,
        );
    }

    diagnostics
}

// ── Recursive collectors ──

/// Recursively collect all Setting IDs from the blueprint for uniqueness checking.
fn collect_all_setting_ids(obj: &serde_json::Map<String, Value>, ids: &mut HashMap<String, usize>) {
    if let Some(settings) = obj.get("Settings").and_then(|v| v.as_array()) {
        for setting in settings {
            if let Some(id) = setting.get("Id").and_then(|v| v.as_str()) {
                ids.entry(id.to_string())
                    .and_modify(|c| *c += 1)
                    .or_insert(1);
            }
        }
    }
    if let Some(tabs) = obj.get("Tabs").and_then(|v| v.as_array()) {
        for tab in tabs {
            if let Some(tab_obj) = tab.as_object() {
                collect_all_setting_ids(tab_obj, ids);
            }
        }
    }
    if let Some(sections) = obj.get("Sections").and_then(|v| v.as_array()) {
        for section in sections {
            if let Some(section_obj) = section.as_object() {
                collect_all_setting_ids(section_obj, ids);
            }
        }
    }
}

// ── Tab validation ──

fn validate_tabs(
    content: &str,
    tabs: &[Value],
    path: &str,
    seen_ids: &mut HashSet<String>,
    all_ids: &HashMap<String, usize>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for (i, tab) in tabs.iter().enumerate() {
        let tab_path = format!("{path}[{i}]");
        let tab_obj = match tab.as_object() {
            Some(o) => o,
            None => {
                diagnostics.push(Diagnostic {
                    line: 1,
                    col: 1,
                    severity: DiagnosticSeverity::Error,
                    message: format!("{tab_path} must be an object"),
                });
                continue;
            }
        };

        // TabId is required
        if tab_obj.get("TabId").and_then(|v| v.as_str()).is_none() {
            diagnostics.push(Diagnostic {
                line: 1,
                col: 1,
                severity: DiagnosticSeverity::Error,
                message: format!("{tab_path} is missing required \"TabId\""),
            });
        }

        // TabName is required
        if tab_obj.get("TabName").and_then(|v| v.as_str()).is_none() {
            diagnostics.push(Diagnostic {
                line: 1,
                col: 1,
                severity: DiagnosticSeverity::Error,
                message: format!("{tab_path} is missing required \"TabName\""),
            });
        }

        // Must have at least one of Settings, Tabs, or Sections
        let has_child_settings = tab_obj.get("Settings").and_then(|v| v.as_array()).is_some();
        let has_child_tabs = tab_obj.get("Tabs").and_then(|v| v.as_array()).is_some();
        let has_child_sections = tab_obj.get("Sections").and_then(|v| v.as_array()).is_some();
        if !has_child_settings && !has_child_tabs && !has_child_sections {
            diagnostics.push(Diagnostic {
                line: 1,
                col: 1,
                severity: DiagnosticSeverity::Warning,
                message: format!("{tab_path} has no \"Settings\", \"Tabs\", or \"Sections\""),
            });
        }

        // Recurse
        if let Some(child_tabs) = tab_obj.get("Tabs").and_then(|v| v.as_array()) {
            validate_tabs(
                content,
                child_tabs,
                &format!("{tab_path}.Tabs"),
                seen_ids,
                all_ids,
                diagnostics,
            );
        }
        if let Some(child_sections) = tab_obj.get("Sections").and_then(|v| v.as_array()) {
            validate_sections(
                content,
                child_sections,
                &format!("{tab_path}.Sections"),
                seen_ids,
                all_ids,
                diagnostics,
            );
        }
        if let Some(child_settings) = tab_obj.get("Settings").and_then(|v| v.as_array()) {
            validate_settings_array(
                content,
                child_settings,
                &format!("{tab_path}.Settings"),
                seen_ids,
                all_ids,
                diagnostics,
            );
        }

        // VisibleIf on tab
        if let Some(visible_if) = tab_obj.get("VisibleIf") {
            validate_visible_if(content, visible_if, &tab_path, all_ids, diagnostics);
        }
    }
}

// ── Section validation ──

fn validate_sections(
    content: &str,
    sections: &[Value],
    path: &str,
    seen_ids: &mut HashSet<String>,
    all_ids: &HashMap<String, usize>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for (i, section) in sections.iter().enumerate() {
        let sec_path = format!("{path}[{i}]");
        let sec_obj = match section.as_object() {
            Some(o) => o,
            None => {
                diagnostics.push(Diagnostic {
                    line: 1,
                    col: 1,
                    severity: DiagnosticSeverity::Error,
                    message: format!("{sec_path} must be an object"),
                });
                continue;
            }
        };

        // SectionId is required
        if sec_obj.get("SectionId").and_then(|v| v.as_str()).is_none() {
            diagnostics.push(Diagnostic {
                line: 1,
                col: 1,
                severity: DiagnosticSeverity::Error,
                message: format!("{sec_path} is missing required \"SectionId\""),
            });
        }

        // SectionName is required
        if sec_obj
            .get("SectionName")
            .and_then(|v| v.as_str())
            .is_none()
        {
            diagnostics.push(Diagnostic {
                line: 1,
                col: 1,
                severity: DiagnosticSeverity::Error,
                message: format!("{sec_path} is missing required \"SectionName\""),
            });
        }

        // Must have Settings or Tabs
        let has_child_settings = sec_obj.get("Settings").and_then(|v| v.as_array()).is_some();
        let has_child_tabs = sec_obj.get("Tabs").and_then(|v| v.as_array()).is_some();
        if !has_child_settings && !has_child_tabs {
            diagnostics.push(Diagnostic {
                line: 1,
                col: 1,
                severity: DiagnosticSeverity::Warning,
                message: format!("{sec_path} has no \"Settings\" or \"Tabs\""),
            });
        }

        // Recurse
        if let Some(child_tabs) = sec_obj.get("Tabs").and_then(|v| v.as_array()) {
            validate_tabs(
                content,
                child_tabs,
                &format!("{sec_path}.Tabs"),
                seen_ids,
                all_ids,
                diagnostics,
            );
        }
        if let Some(child_settings) = sec_obj.get("Settings").and_then(|v| v.as_array()) {
            validate_settings_array(
                content,
                child_settings,
                &format!("{sec_path}.Settings"),
                seen_ids,
                all_ids,
                diagnostics,
            );
        }

        // VisibleIf on section
        if let Some(visible_if) = sec_obj.get("VisibleIf") {
            validate_visible_if(content, visible_if, &sec_path, all_ids, diagnostics);
        }
    }
}

// ── Settings array validation ──

fn validate_settings_array(
    content: &str,
    settings: &[Value],
    path: &str,
    seen_ids: &mut HashSet<String>,
    all_ids: &HashMap<String, usize>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for (i, setting) in settings.iter().enumerate() {
        validate_setting(
            content,
            setting,
            &format!("{path}[{i}]"),
            seen_ids,
            all_ids,
            diagnostics,
        );
    }
}

/// Validate a single setting entry.
fn validate_setting(
    content: &str,
    setting: &Value,
    loc: &str,
    seen_ids: &mut HashSet<String>,
    all_ids: &HashMap<String, usize>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let obj = match setting.as_object() {
        Some(o) => o,
        None => {
            diagnostics.push(Diagnostic {
                line: 1,
                col: 1,
                severity: DiagnosticSeverity::Error,
                message: format!("{loc} must be an object"),
            });
            return;
        }
    };

    // ── Id (required, unique) ──
    let setting_id = match obj.get("Id").and_then(|v| v.as_str()) {
        Some(id) => {
            if !seen_ids.insert(id.to_string()) {
                let (line, col) = find_string_value_position(content, id);
                diagnostics.push(Diagnostic {
                    line,
                    col,
                    severity: DiagnosticSeverity::Error,
                    message: format!("Duplicate Setting Id \"{id}\""),
                });
            }
            Some(id)
        }
        None => {
            diagnostics.push(Diagnostic {
                line: 1,
                col: 1,
                severity: DiagnosticSeverity::Error,
                message: format!("{loc} is missing \"Id\""),
            });
            None
        }
    };

    // ── Name (required) ──
    if obj.get("Name").and_then(|v| v.as_str()).is_none() {
        let id_hint = setting_id.unwrap_or("?");
        diagnostics.push(Diagnostic {
            line: 1,
            col: 1,
            severity: DiagnosticSeverity::Error,
            message: format!("{loc} (Id=\"{id_hint}\") is missing \"Name\""),
        });
    }

    // ── Type (required, must be a known widget) ──
    let widget_type = match obj.get("Type").and_then(|v| v.as_str()) {
        Some(t) => {
            if !VALID_WIDGET_TYPES.contains(&t) {
                let (line, col) = find_string_value_position(content, t);
                diagnostics.push(Diagnostic {
                    line,
                    col,
                    severity: DiagnosticSeverity::Error,
                    message: format!("Unknown widget type \"{t}\""),
                });
            }
            Some(t)
        }
        None => {
            let id_hint = setting_id.unwrap_or("?");
            diagnostics.push(Diagnostic {
                line: 1,
                col: 1,
                severity: DiagnosticSeverity::Error,
                message: format!("{loc} (Id=\"{id_hint}\") is missing \"Type\""),
            });
            None
        }
    };

    // ── Description or Tooltip required (at least one) ──
    let has_desc = obj.get("Description").and_then(|v| v.as_str()).is_some();
    let has_tooltip = obj.get("Tooltip").and_then(|v| v.as_str()).is_some();
    if !has_desc && !has_tooltip {
        let id_hint = setting_id.unwrap_or("?");
        diagnostics.push(Diagnostic {
            line: 1, col: 1,
            severity: DiagnosticSeverity::Warning,
            message: format!(
                "{loc} (Id=\"{id_hint}\") should have at least one of \"Description\" or \"Tooltip\""
            ),
        });
    }

    // ── Default (required for all types except event_button) ──
    let is_event_button = widget_type == Some("event_button");
    if !is_event_button && obj.get("Default").is_none() {
        let id_hint = setting_id.unwrap_or("?");
        diagnostics.push(Diagnostic {
            line: 1,
            col: 1,
            severity: DiagnosticSeverity::Error,
            message: format!("{loc} (Id=\"{id_hint}\") is missing \"Default\""),
        });
    }

    // ── Default value type-check ──
    if let (Some(wtype), Some(default_val)) = (widget_type, obj.get("Default")) {
        validate_default(content, wtype, default_val, loc, diagnostics);
    }

    // ── Options validation by widget type ──
    if let Some(wtype) = widget_type {
        let options = obj.get("Options").and_then(|v| v.as_object());

        match wtype {
            // enum/radio require Options.Choices
            "enum" | "radio" => {
                match options {
                    None => {
                        let id_hint = setting_id.unwrap_or("?");
                        let (line, col) = find_string_value_position(content, id_hint);
                        diagnostics.push(Diagnostic {
                            line, col,
                            severity: DiagnosticSeverity::Error,
                            message: format!(
                                "{loc}: widget type \"{wtype}\" requires \"Options\" with \"Choices\" array"
                            ),
                        });
                    }
                    Some(opts) => {
                        match opts.get("Choices").and_then(|v| v.as_array()) {
                            None => {
                                let (line, col) = find_key_position(content, "Options");
                                diagnostics.push(Diagnostic {
                                    line, col,
                                    severity: DiagnosticSeverity::Error,
                                    message: format!(
                                        "{loc}: \"Options\" for \"{wtype}\" must contain a \"Choices\" array"
                                    ),
                                });
                            }
                            Some(choices) => {
                                // radio requires at least 1; enum allows 0 (for runtime-defined)
                                if wtype == "radio" && choices.is_empty() {
                                    let (line, col) = find_key_position(content, "Choices");
                                    diagnostics.push(Diagnostic {
                                        line,
                                        col,
                                        severity: DiagnosticSeverity::Error,
                                        message: format!(
                                            "{loc}: \"Choices\" must not be empty for radio widget"
                                        ),
                                    });
                                }
                            }
                        }
                    }
                }
            }
            // slider_int, slider_float, drag_int, drag_float require Options.Min and Options.Max
            "slider_int" | "slider_float" | "drag_int" | "drag_float" => match options {
                None => {
                    let id_hint = setting_id.unwrap_or("?");
                    let (line, col) = find_string_value_position(content, id_hint);
                    diagnostics.push(Diagnostic {
                            line, col,
                            severity: DiagnosticSeverity::Error,
                            message: format!(
                                "{loc}: widget type \"{wtype}\" requires \"Options\" with \"Min\" and \"Max\""
                            ),
                        });
                }
                Some(opts) => {
                    validate_slider_bounds(content, opts, loc, diagnostics);
                }
            },
            _ => {}
        }
    }

    // ── VisibleIf cross-reference ──
    if let Some(visible_if) = obj.get("VisibleIf") {
        validate_visible_if(content, visible_if, loc, all_ids, diagnostics);
    }
}

/// Validate that Default value matches the expected type for the widget.
fn validate_default(
    content: &str,
    wtype: &str,
    default_val: &Value,
    loc: &str,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let (line, col) = find_key_position(content, "Default");
    match wtype {
        "checkbox" => {
            if !default_val.is_boolean() {
                diagnostics.push(Diagnostic {
                    line,
                    col,
                    severity: DiagnosticSeverity::Error,
                    message: format!("{loc}: \"Default\" for checkbox must be a boolean"),
                });
            }
        }
        "int" | "slider_int" | "drag_int" => {
            if !default_val.is_i64() && !default_val.is_u64() {
                diagnostics.push(Diagnostic {
                    line,
                    col,
                    severity: DiagnosticSeverity::Error,
                    message: format!("{loc}: \"Default\" for {wtype} must be an integer"),
                });
            }
        }
        "float" | "slider_float" | "drag_float" => {
            if !default_val.is_f64() && !default_val.is_i64() && !default_val.is_u64() {
                diagnostics.push(Diagnostic {
                    line,
                    col,
                    severity: DiagnosticSeverity::Error,
                    message: format!("{loc}: \"Default\" for {wtype} must be a number"),
                });
            }
        }
        "text" | "enum" | "radio" => {
            if !default_val.is_string() {
                diagnostics.push(Diagnostic {
                    line,
                    col,
                    severity: DiagnosticSeverity::Error,
                    message: format!("{loc}: \"Default\" for {wtype} must be a string"),
                });
            }
        }
        "color_picker" | "color_edit" => {
            if !default_val.is_array() {
                diagnostics.push(Diagnostic {
                    line,
                    col,
                    severity: DiagnosticSeverity::Error,
                    message: format!(
                        "{loc}: \"Default\" for {wtype} must be an array (RGBA values)"
                    ),
                });
            }
        }
        "list_v2" => {
            if !default_val.is_object() {
                diagnostics.push(Diagnostic {
                    line,
                    col,
                    severity: DiagnosticSeverity::Error,
                    message: format!("{loc}: \"Default\" for list_v2 must be an object"),
                });
            }
        }
        "keybinding_v2" => {
            if !default_val.is_object() {
                diagnostics.push(Diagnostic {
                    line,
                    col,
                    severity: DiagnosticSeverity::Error,
                    message: format!("{loc}: \"Default\" for keybinding_v2 must be an object"),
                });
            }
        }
        // event_button — Default is optional and unused
        "event_button" => {}
        _ => {}
    }
}

/// Validate slider/drag Min/Max ordering inside Options object.
fn validate_slider_bounds(
    content: &str,
    options: &serde_json::Map<String, Value>,
    loc: &str,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let min_val = options.get("Min").and_then(|v| v.as_f64());
    let max_val = options.get("Max").and_then(|v| v.as_f64());

    match (min_val, max_val) {
        (Some(min), Some(max)) => {
            if min > max {
                let (line, col) = find_key_position(content, "Min");
                diagnostics.push(Diagnostic {
                    line,
                    col,
                    severity: DiagnosticSeverity::Error,
                    message: format!("{loc}: \"Min\" ({min}) must be ≤ \"Max\" ({max})"),
                });
            }
        }
        (None, _) => {
            let (line, col) = find_key_position(content, "Min");
            diagnostics.push(Diagnostic {
                line,
                col,
                severity: DiagnosticSeverity::Warning,
                message: format!("{loc}: slider is missing \"Min\""),
            });
        }
        (_, None) => {
            let (line, col) = find_key_position(content, "Max");
            diagnostics.push(Diagnostic {
                line,
                col,
                severity: DiagnosticSeverity::Warning,
                message: format!("{loc}: slider is missing \"Max\""),
            });
        }
    }
}

/// Validate VisibleIf cross-references resolve to existing setting IDs.
///
/// MCM Schema VisibleIf is an object: `{ "Conditions": [...], "LogicalOperator"?: "and"|"or" }`
fn validate_visible_if(
    content: &str,
    visible_if: &Value,
    loc: &str,
    all_ids: &HashMap<String, usize>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let vif_obj = match visible_if.as_object() {
        Some(o) => o,
        None => {
            let (line, col) = find_key_position(content, "VisibleIf");
            diagnostics.push(Diagnostic {
                line,
                col,
                severity: DiagnosticSeverity::Error,
                message: format!(
                    "{loc}: \"VisibleIf\" must be an object with a \"Conditions\" array"
                ),
            });
            return;
        }
    };

    let conditions = match vif_obj.get("Conditions").and_then(|v| v.as_array()) {
        Some(arr) => arr,
        None => {
            let (line, col) = find_key_position(content, "VisibleIf");
            diagnostics.push(Diagnostic {
                line,
                col,
                severity: DiagnosticSeverity::Error,
                message: format!("{loc}: \"VisibleIf\" must contain a \"Conditions\" array"),
            });
            return;
        }
    };

    for cond in conditions {
        if let Some(ref_id) = cond.get("SettingId").and_then(|v| v.as_str()) {
            if !all_ids.contains_key(ref_id) {
                let (line, col) = find_string_value_position(content, ref_id);
                diagnostics.push(Diagnostic {
                    line,
                    col,
                    severity: DiagnosticSeverity::Error,
                    message: format!("{loc}: VisibleIf references unknown Setting Id \"{ref_id}\""),
                });
            }
        }

        // Validate Operator is present and valid
        if let Some(op) = cond.get("Operator").and_then(|v| v.as_str()) {
            if !["==", "!=", ">", "<", ">=", "<="].contains(&op) {
                let (line, col) = find_string_value_position(content, op);
                diagnostics.push(Diagnostic {
                    line,
                    col,
                    severity: DiagnosticSeverity::Error,
                    message: format!("{loc}: VisibleIf Condition has invalid Operator \"{op}\""),
                });
            }
        }

        // ExpectedValue should be present
        if cond.get("ExpectedValue").is_none() {
            diagnostics.push(Diagnostic {
                line: 1,
                col: 1,
                severity: DiagnosticSeverity::Warning,
                message: format!("{loc}: VisibleIf Condition is missing \"ExpectedValue\""),
            });
        }
    }
}

// ── Helpers (mirror se_config.rs pattern) ──

/// Find the 1-based (line, col) of a JSON key in the raw source.
fn find_key_position(content: &str, key: &str) -> (usize, usize) {
    let needle = format!("\"{key}\"");
    let mut search_start = 0;

    while let Some(rel) = content[search_start..].find(&needle) {
        let abs = search_start + rel;
        let after = &content[abs + needle.len()..];
        let after_trimmed = after.trim_start();
        if after_trimmed.starts_with(':') {
            return offset_to_line_col(content, abs);
        }
        search_start = abs + 1;
    }

    (1, 1)
}

/// Find the 1-based (line, col) of a string value `"val"` (not a key).
fn find_string_value_position(content: &str, val: &str) -> (usize, usize) {
    let needle = format!("\"{val}\"");
    let mut search_start = 0;

    while let Some(rel) = content[search_start..].find(&needle) {
        let abs = search_start + rel;
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
    fn valid_blueprint_produces_no_diagnostics() {
        let content = r#"{
  "SchemaVersion": 1,
  "ModName": "TestMod",
  "Tabs": [
    {
      "TabName": "General",
      "TabId": "general",
      "Settings": [
        {
          "Id": "enable_feature",
          "Name": "Enable Feature",
          "Type": "checkbox",
          "Default": true,
          "Description": "Toggle the feature on or off."
        },
        {
          "Id": "difficulty",
          "Name": "Difficulty",
          "Type": "slider_int",
          "Default": 5,
          "Description": "Set the difficulty level.",
          "Options": { "Min": 1, "Max": 10 }
        },
        {
          "Id": "mode",
          "Name": "Mode",
          "Type": "enum",
          "Default": "normal",
          "Description": "Select the mode.",
          "Options": {
            "Choices": ["easy", "normal", "hard"]
          }
        }
      ]
    }
  ]
}"#;
        let diags = validate_mcm_blueprint(content);
        assert!(diags.is_empty(), "Expected no diagnostics, got: {diags:?}");
    }

    #[test]
    fn valid_blueprint_with_sections_at_root() {
        let content = r#"{
  "SchemaVersion": 1,
  "Sections": [
    {
      "SectionId": "main",
      "SectionName": "Main",
      "Settings": [
        {
          "Id": "enabled",
          "Name": "Enabled",
          "Type": "checkbox",
          "Default": true,
          "Tooltip": "Toggle on or off."
        }
      ]
    }
  ]
}"#;
        let diags = validate_mcm_blueprint(content);
        assert!(diags.is_empty(), "Expected no diagnostics, got: {diags:?}");
    }

    #[test]
    fn valid_blueprint_with_settings_at_root() {
        let content = r#"{
  "SchemaVersion": 1,
  "Settings": [
    {
      "Id": "enabled",
      "Name": "Enabled",
      "Type": "checkbox",
      "Default": true,
      "Description": "Toggle."
    }
  ]
}"#;
        let diags = validate_mcm_blueprint(content);
        assert!(diags.is_empty(), "Expected no diagnostics, got: {diags:?}");
    }

    #[test]
    fn missing_schema_version() {
        let content = r#"{"Tabs": [{"TabId": "a", "TabName": "A", "Settings": []}]}"#;
        let diags = validate_mcm_blueprint(content);
        assert!(has_error(&diags, "SchemaVersion"));
    }

    #[test]
    fn missing_tabs_sections_and_settings() {
        let content = r#"{"SchemaVersion": 1}"#;
        let diags = validate_mcm_blueprint(content);
        assert!(has_error(&diags, "Tabs"));
    }

    #[test]
    fn invalid_json() {
        let diags = validate_mcm_blueprint("{ not valid }");
        assert!(has_error(&diags, "Invalid JSON"));
    }

    #[test]
    fn not_an_object() {
        let diags = validate_mcm_blueprint("[1, 2]");
        assert!(has_error(&diags, "must be a JSON object"));
    }

    #[test]
    fn duplicate_setting_id() {
        let content = r#"{
  "SchemaVersion": 1,
  "Tabs": [
    {
      "TabName": "A",
      "TabId": "a",
      "Settings": [
        {"Id": "dup", "Name": "Dup1", "Type": "checkbox", "Default": true, "Tooltip": "x"},
        {"Id": "dup", "Name": "Dup2", "Type": "checkbox", "Default": false, "Tooltip": "y"}
      ]
    }
  ]
}"#;
        let diags = validate_mcm_blueprint(content);
        assert!(has_error(&diags, "Duplicate Setting Id"));
    }

    #[test]
    fn unknown_widget_type() {
        let content = r#"{
  "SchemaVersion": 1,
  "Tabs": [{"TabName": "A", "TabId": "a", "Settings": [
    {"Id": "x", "Name": "X", "Type": "spinbox", "Default": 1, "Tooltip": "x"}
  ]}]
}"#;
        let diags = validate_mcm_blueprint(content);
        assert!(has_error(&diags, "Unknown widget type"));
    }

    #[test]
    fn checkbox_default_must_be_bool() {
        let content = r#"{
  "SchemaVersion": 1,
  "Tabs": [{"TabName": "A", "TabId": "a", "Settings": [
    {"Id": "x", "Name": "X", "Type": "checkbox", "Default": "yes", "Tooltip": "x"}
  ]}]
}"#;
        let diags = validate_mcm_blueprint(content);
        assert!(has_error(&diags, "checkbox must be a boolean"));
    }

    #[test]
    fn slider_int_default_must_be_integer() {
        let content = r#"{
  "SchemaVersion": 1,
  "Tabs": [{"TabName": "A", "TabId": "a", "Settings": [
    {"Id": "x", "Name": "X", "Type": "slider_int", "Default": 1.5, "Tooltip": "x",
     "Options": {"Min": 0, "Max": 10}}
  ]}]
}"#;
        let diags = validate_mcm_blueprint(content);
        assert!(has_error(&diags, "slider_int must be an integer"));
    }

    #[test]
    fn slider_min_greater_than_max() {
        let content = r#"{
  "SchemaVersion": 1,
  "Tabs": [{"TabName": "A", "TabId": "a", "Settings": [
    {"Id": "x", "Name": "X", "Type": "slider_int", "Default": 5, "Tooltip": "x",
     "Options": {"Min": 10, "Max": 1}}
  ]}]
}"#;
        let diags = validate_mcm_blueprint(content);
        assert!(has_error(&diags, "Min"));
    }

    #[test]
    fn enum_requires_options_with_choices() {
        let content = r#"{
  "SchemaVersion": 1,
  "Tabs": [{"TabName": "A", "TabId": "a", "Settings": [
    {"Id": "x", "Name": "X", "Type": "enum", "Default": "a", "Tooltip": "x"}
  ]}]
}"#;
        let diags = validate_mcm_blueprint(content);
        assert!(has_error(&diags, "Options"));
    }

    #[test]
    fn radio_empty_choices_errors() {
        let content = r#"{
  "SchemaVersion": 1,
  "Tabs": [{"TabName": "A", "TabId": "a", "Settings": [
    {"Id": "x", "Name": "X", "Type": "radio", "Default": "a", "Tooltip": "x",
     "Options": {"Choices": []}}
  ]}]
}"#;
        let diags = validate_mcm_blueprint(content);
        assert!(has_error(&diags, "must not be empty"));
    }

    #[test]
    fn visible_if_unknown_reference() {
        let content = r#"{
  "SchemaVersion": 1,
  "Tabs": [{"TabName": "A", "TabId": "a", "Settings": [
    {"Id": "x", "Name": "X", "Type": "checkbox", "Default": true, "Tooltip": "x"},
    {"Id": "y", "Name": "Y", "Type": "text", "Default": "", "Tooltip": "y",
     "VisibleIf": {"Conditions": [{"SettingId": "nonexistent", "Operator": "==", "ExpectedValue": true}]}}
  ]}]
}"#;
        let diags = validate_mcm_blueprint(content);
        assert!(has_error(&diags, "unknown Setting Id"));
    }

    #[test]
    fn visible_if_valid_reference() {
        let content = r#"{
  "SchemaVersion": 1,
  "Tabs": [{"TabName": "A", "TabId": "a", "Settings": [
    {"Id": "toggle", "Name": "Toggle", "Type": "checkbox", "Default": true, "Tooltip": "x"},
    {"Id": "detail", "Name": "Detail", "Type": "text", "Default": "", "Tooltip": "y",
     "VisibleIf": {"Conditions": [{"SettingId": "toggle", "Operator": "==", "ExpectedValue": true}]}}
  ]}]
}"#;
        let diags = validate_mcm_blueprint(content);
        assert!(
            !has_error(&diags, "unknown Setting Id"),
            "Valid reference should not produce error: {diags:?}"
        );
    }

    #[test]
    fn missing_tab_id_errors() {
        let content = r#"{
  "SchemaVersion": 1,
  "Tabs": [{"TabName": "A", "Settings": [
    {"Id": "x", "Name": "X", "Type": "checkbox", "Default": true, "Tooltip": "x"}
  ]}]
}"#;
        let diags = validate_mcm_blueprint(content);
        assert!(has_error(&diags, "TabId"));
    }

    #[test]
    fn missing_tab_name_errors() {
        let content = r#"{
  "SchemaVersion": 1,
  "Tabs": [{"TabId": "a", "Settings": [
    {"Id": "x", "Name": "X", "Type": "checkbox", "Default": true, "Tooltip": "x"}
  ]}]
}"#;
        let diags = validate_mcm_blueprint(content);
        assert!(has_error(&diags, "TabName"));
    }

    #[test]
    fn missing_description_and_tooltip_warns() {
        let content = r#"{
  "SchemaVersion": 1,
  "Tabs": [{"TabName": "A", "TabId": "a", "Settings": [
    {"Id": "x", "Name": "X", "Type": "checkbox", "Default": true}
  ]}]
}"#;
        let diags = validate_mcm_blueprint(content);
        assert!(has_warning(&diags, "Description"));
    }

    #[test]
    fn event_button_no_default_required() {
        let content = r#"{
  "SchemaVersion": 1,
  "Tabs": [{"TabName": "A", "TabId": "a", "Settings": [
    {"Id": "btn", "Name": "Click Me", "Type": "event_button", "Description": "Does a thing."}
  ]}]
}"#;
        let diags = validate_mcm_blueprint(content);
        assert!(
            !has_error(&diags, "Default"),
            "event_button should not require Default: {diags:?}"
        );
    }

    #[test]
    fn nested_tabs_validated() {
        let content = r#"{
  "SchemaVersion": 1,
  "Tabs": [
    {
      "TabName": "Outer", "TabId": "outer",
      "Tabs": [
        {
          "TabName": "Inner", "TabId": "inner",
          "Settings": [
            {"Id": "dup", "Name": "A", "Type": "checkbox", "Default": true, "Tooltip": "x"},
            {"Id": "dup", "Name": "B", "Type": "checkbox", "Default": false, "Tooltip": "y"}
          ]
        }
      ]
    }
  ]
}"#;
        let diags = validate_mcm_blueprint(content);
        assert!(has_error(&diags, "Duplicate Setting Id"));
    }

    #[test]
    fn all_widget_types_accepted() {
        for wtype in VALID_WIDGET_TYPES {
            let default = match *wtype {
                "checkbox" => "true",
                "int" | "slider_int" | "drag_int" => "5",
                "float" | "slider_float" | "drag_float" => "1.5",
                "text" | "enum" | "radio" => "\"val\"",
                "color_picker" | "color_edit" => "[0.0, 1.0, 0.5, 1.0]",
                "list_v2" => r#"{"Enabled": true, "Elements": []}"#,
                "keybinding_v2" => r#"{"Keyboard": {"Key": "K"}}"#,
                "event_button" => "null",
                _ => "null",
            };
            let options = match *wtype {
                "enum" => r#", "Options": {"Choices": ["val"]}"#,
                "radio" => r#", "Options": {"Choices": ["val"]}"#,
                "slider_int" | "slider_float" | "drag_int" | "drag_float" => {
                    r#", "Options": {"Min": 0, "Max": 10}"#
                }
                _ => "",
            };
            let content = format!(
                r#"{{"SchemaVersion": 1, "Settings": [{{"Id": "test", "Name": "Test", "Type": "{wtype}", "Default": {default}, "Tooltip": "tip"{options}}}]}}"#
            );
            let diags = validate_mcm_blueprint(&content);
            let errors: Vec<_> = diags
                .iter()
                .filter(|d| matches!(d.severity, DiagnosticSeverity::Error))
                .collect();
            assert!(
                errors.is_empty(),
                "Widget type '{wtype}' produced errors: {errors:?}"
            );
        }
    }

    #[test]
    fn drag_int_requires_options_min_max() {
        let content = r#"{
  "SchemaVersion": 1,
  "Settings": [
    {"Id": "x", "Name": "X", "Type": "drag_int", "Default": 3, "Tooltip": "x"}
  ]
}"#;
        let diags = validate_mcm_blueprint(content);
        assert!(has_error(&diags, "Options"));
    }

    #[test]
    fn color_picker_default_must_be_array() {
        let content = r#"{
  "SchemaVersion": 1,
  "Settings": [
    {"Id": "c", "Name": "C", "Type": "color_picker", "Default": "red", "Tooltip": "x"}
  ]
}"#;
        let diags = validate_mcm_blueprint(content);
        assert!(has_error(&diags, "must be an array"));
    }

    #[test]
    fn section_missing_id_errors() {
        let content = r#"{
  "SchemaVersion": 1,
  "Sections": [
    {
      "SectionName": "No ID",
      "Settings": [
        {"Id": "x", "Name": "X", "Type": "checkbox", "Default": true, "Tooltip": "x"}
      ]
    }
  ]
}"#;
        let diags = validate_mcm_blueprint(content);
        assert!(has_error(&diags, "SectionId"));
    }
}
