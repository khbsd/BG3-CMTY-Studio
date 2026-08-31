//! Value-format conversion between `.lsefx` (toolkit) and `.lsfx` (runtime).
//!
//! - Vectors: comma-separated in toolkit ("0,1,0") ↔ space-separated in runtime ("0 1 0")
//! - Booleans: "1"/"0" in toolkit ↔ "True"/"False" in runtime

/// Convert a space-separated vector (runtime) to comma-separated (toolkit).
///
/// `"0 1 0"` → `"0,1,0"`
pub fn vec_runtime_to_toolkit(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(",")
}

/// Convert a comma-separated vector (toolkit) to space-separated (runtime).
///
/// `"0,1,0"` → `"0 1 0"`
pub fn vec_toolkit_to_runtime(value: &str) -> String {
    value.split(',').collect::<Vec<_>>().join(" ")
}

/// Convert "True"/"False" (runtime) to "1"/"0" (toolkit).
pub fn bool_runtime_to_toolkit(value: &str) -> &'static str {
    if value == "True" {
        "1"
    } else {
        "0"
    }
}

/// Convert "1"/"0" (toolkit) to "True"/"False" (runtime).
pub fn bool_toolkit_to_runtime(value: &str) -> &'static str {
    match value {
        "1" | "true" | "True" => "True",
        _ => "False",
    }
}

/// Types whose values are vectors (space-separated in runtime, comma-separated in toolkit).
const VECTOR_TYPES: &[&str] = &["fvec2", "fvec3", "fvec4", "ivec2", "ivec3", "ivec4"];

/// Types whose values are booleans.
const BOOL_TYPES: &[&str] = &["bool"];

/// Convert a runtime attribute value to toolkit datum format.
pub fn runtime_value_to_toolkit(value: &str, attr_type: &str) -> String {
    if VECTOR_TYPES.contains(&attr_type) {
        return vec_runtime_to_toolkit(value);
    }
    if BOOL_TYPES.contains(&attr_type) {
        return bool_runtime_to_toolkit(value).to_string();
    }
    value.to_string()
}

/// Convert a toolkit datum value to runtime attribute format.
pub fn toolkit_value_to_runtime(value: &str, attr_type: &str) -> String {
    if VECTOR_TYPES.contains(&attr_type) {
        return vec_toolkit_to_runtime(value);
    }
    if BOOL_TYPES.contains(&attr_type) {
        return bool_toolkit_to_runtime(value).to_string();
    }
    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vector_conversion() {
        assert_eq!(vec_runtime_to_toolkit("0 1 0"), "0,1,0");
        assert_eq!(vec_toolkit_to_runtime("0,1,0"), "0 1 0");
        assert_eq!(vec_runtime_to_toolkit("1.5 2.3"), "1.5,2.3");
        assert_eq!(vec_toolkit_to_runtime("1,2,3,4"), "1 2 3 4");
    }

    #[test]
    fn bool_conversion() {
        assert_eq!(bool_runtime_to_toolkit("True"), "1");
        assert_eq!(bool_runtime_to_toolkit("False"), "0");
        assert_eq!(bool_toolkit_to_runtime("1"), "True");
        assert_eq!(bool_toolkit_to_runtime("0"), "False");
        assert_eq!(bool_toolkit_to_runtime("true"), "True");
    }

    #[test]
    fn typed_conversion() {
        assert_eq!(runtime_value_to_toolkit("0 1 0", "fvec3"), "0,1,0");
        assert_eq!(runtime_value_to_toolkit("True", "bool"), "1");
        assert_eq!(runtime_value_to_toolkit("hello", "FixedString"), "hello");

        assert_eq!(toolkit_value_to_runtime("0,1,0", "fvec3"), "0 1 0");
        assert_eq!(toolkit_value_to_runtime("1", "bool"), "True");
        assert_eq!(toolkit_value_to_runtime("world", "FixedString"), "world");
    }

    // ── Unhappy-path / edge-case tests ─────────────────────────────

    #[test]
    fn empty_string_conversions() {
        assert_eq!(vec_runtime_to_toolkit(""), "");
        assert_eq!(vec_toolkit_to_runtime(""), "");
        assert_eq!(runtime_value_to_toolkit("", "fvec3"), "");
        assert_eq!(toolkit_value_to_runtime("", "fvec3"), "");
    }

    #[test]
    fn bool_unexpected_values() {
        // Non-standard bool inputs should fall through to "0" / "False"
        assert_eq!(bool_runtime_to_toolkit("yes"), "0");
        assert_eq!(bool_runtime_to_toolkit(""), "0");
        assert_eq!(bool_toolkit_to_runtime("yes"), "False");
        assert_eq!(bool_toolkit_to_runtime(""), "False");
        assert_eq!(bool_toolkit_to_runtime("2"), "False");
    }

    #[test]
    fn single_component_vector() {
        // A single value — no spaces or commas
        assert_eq!(vec_runtime_to_toolkit("42"), "42");
        assert_eq!(vec_toolkit_to_runtime("42"), "42");
    }

    #[test]
    fn unknown_type_passthrough() {
        // Unknown types should pass through unchanged
        assert_eq!(
            runtime_value_to_toolkit("hello world", "SomeNewType"),
            "hello world"
        );
        assert_eq!(
            toolkit_value_to_runtime("hello,world", "SomeNewType"),
            "hello,world"
        );
    }
}
