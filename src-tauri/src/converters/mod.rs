//! Bidirectional transform between `LsxResource` (runtime `.lsfx`) and
//! `EffectResource` (toolkit `.lsefx`).
//!
//! The runtime `.lsx` (Divine-converted from `.lsfx` binary) uses nested
//! `<node id="Property">` children with `AttributeName`/`FullName`/`Type`/`Value`
//! attributes. The toolkit `.lsefx` uses GUID-identified `<property>` elements.
//!
//! The [`AllSparkRegistry`] bridges the two by mapping GUID ↔ property name.

pub mod value_convert;

use crate::allspark::AllSparkRegistry;
use crate::models::effect::*;
use crate::models::{LsxNode, LsxNodeAttribute, LsxRegion, LsxResource};
use std::collections::HashMap;
use value_convert::{runtime_value_to_toolkit, toolkit_value_to_runtime};

// ── Well-known implicit GUIDs ──────────────────────────────────────

/// The "Name" implicit property — always contains the component class name.
const IMPLICIT_NAME_GUID: &str = "ef1d7d1e-02b6-4548-80d9-5ef2fbcda237";

/// The "Start / End Time" implicit property — contains "start,end" as comma-separated.
const IMPLICIT_POSITION_GUID: &str = "035b5248-d0ca-44b7-853f-3acb84110e67";

/// The "Required" module — always present on every component.
const REQUIRED_MODULE_GUID: &str = "286df729-035e-4bb8-a210-c836ddbbbacc";

// ── FullName → AttributeName overrides ─────────────────────────────
//
// The runtime `.lsx` format uses an `AttributeName` that sometimes differs from
// the XCD property name (the leaf of `FullName`).  These overrides are hard-coded
// in the AllSpark editor runtime — they do NOT appear in any config file.
//
// Extracted by scanning 500+ vanilla `.lsx` effect files.

/// Resolve the runtime `AttributeName` for a given `FullName`.
///
/// Returns the override if one exists, otherwise the leaf segment of `FullName`.
fn fullname_to_attribute_name(full_name: &str) -> &str {
    match full_name {
        "Appearance.Light UUID" => "Light GUID",
        "Appearance.Material GUID" => "Material",
        "Behavior.Position" => "Keyframed Position",
        "Behavior.Rotation" => "RPM",
        "Behavior.Rotation / Life" => "Rotation/Life",
        "Behaviour.Offset" => "Keyframed Position",
        "BoundingBox" => "",
        "BoundingSphere" => "Bounding Volume",
        "Center" => "Position",
        "Debug Mesh.Animation" => "Mesh Animation",
        "Debug Mesh.Animation Speed" => "Mesh Animation Speed",
        "Emitter.Behavior.Emit Velocity Angles" => "Init. Velocity Angle",
        "Emitter.Behavior.Emit Velocity Axis" => "Init. Velocity Axis",
        "Emitter.Behavior.Emit Velocity Axis Rotation" => "Init. Velocity Axis Rotation",
        "Emitter.Behavior.Infinite Lifespan" => "Infinite Lifetime",
        "Emitter.Behavior.Inherit Velocity Percent" => "Percent",
        "Emitter.Behavior.Inherit Velocity Randomly" => "Random",
        "Emitter.Behavior.Lifespan" => "Lifetime",
        "Emitter.Box.Box Dimensions" => "Dimensions",
        "Keyframed Offset" => "Keyframed Position",
        "Offset" => "Position",
        "Particle.Appearance.Attached Fx Inherits Rotation" => "Attached Effect Inherits Rotation",
        "Particle.Appearance.Attached Fx Inherits Scale" => "Attached Effect Inherits Scale",
        "Particle.Appearance.Attached FX only" => "Attached Effect Only",
        "Particle.Appearance.Initial Rotation" => "Initial Rotation Angle",
        "Particle.Appearance.Initial Rotation Speed" => "Initial Rotation Rate",
        "Particle.Appearance.Random Initial Rotation" => "",
        "Particle.Appearance.Render Options - Attached Fx.Align To Velocity" => {
            "Attached Fx Align To Velocity"
        }
        "Particle.Appearance.Render Options - Axis Aligned.Orientation" => {
            "Initial Orientation/Axis Lock"
        }
        "Particle.Appearance.Render Style" => "Alignment",
        "Particle.Appearance.Rotation" => "Rotation/Life",
        "Particle.Appearance.Rotation Speed" => "Rotation Rate/Life",
        "Particle.Appearance.Velocity Offset" => "Velocity/Life",
        "Particle.Behavior.Particle Death.Attach Death Fx To Parent" => "Death Fx Inherits Alpha",
        "Scale And Rotation.Rotation / Life" => "Rotation/Life",
        // Default: use the leaf segment of FullName
        _ => full_name.rsplit('.').next().unwrap_or(full_name),
    }
}

// ═══════════════════════════════════════════════════════════════════
//  LsxResource  →  EffectResource  (decompile: .lsfx → .lsefx)
// ═══════════════════════════════════════════════════════════════════

/// Convert a runtime `LsxResource` (from an `.lsfx` binary) into a toolkit `EffectResource`.
pub fn lsx_to_effect(resource: &LsxResource, registry: &AllSparkRegistry) -> EffectResource {
    let mut effect = EffectResource::default();

    let effect_region = match resource.regions.iter().find(|r| r.id == "Effect") {
        Some(r) => r,
        None => return effect,
    };

    // Collect all EffectComponent nodes
    let mut all_components: Vec<&LsxNode> = Vec::new();
    find_effect_components(&effect_region.nodes, &mut all_components);

    if !all_components.is_empty() {
        decompile_components(&all_components, &mut effect, registry);
    }

    effect
}

fn find_effect_components<'a>(nodes: &'a [LsxNode], result: &mut Vec<&'a LsxNode>) {
    for node in nodes {
        match node.id.as_str() {
            "EffectComponents" => {
                for child in &node.children {
                    if child.id == "EffectComponent" {
                        result.push(child);
                    }
                }
            }
            "Effect" => find_effect_components(&node.children, result),
            "EffectComponent" => result.push(node),
            _ => {}
        }
    }
}

fn decompile_components(
    components: &[&LsxNode],
    effect: &mut EffectResource,
    registry: &AllSparkRegistry,
) {
    // Single track group with one track (default grouping for vanilla files)
    let mut tg = TrackGroup {
        name: "New Track Group".to_string(),
        ids: vec![TrackGroupId {
            value: "2".to_string(),
        }],
        tracks: Vec::new(),
    };

    // Group components by their Track attribute into tracks
    let mut track_map: HashMap<u32, Vec<&LsxNode>> = HashMap::new();
    for node in components {
        let track_idx = node_attr_value(node, "Track")
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(0);
        track_map.entry(track_idx).or_default().push(node);
    }

    let mut track_indices: Vec<u32> = track_map.keys().copied().collect();
    track_indices.sort();

    for idx in track_indices {
        let comp_nodes = &track_map[&idx];
        let mut track = Track {
            name: "Track".to_string(),
            muted: "False".to_string(),
            locked: "False".to_string(),
            mute_state_override: "None".to_string(),
            components: Vec::new(),
        };

        for cn in comp_nodes {
            track.components.push(decompile_component(cn, registry));
        }
        tg.tracks.push(track);
    }

    effect.track_groups.push(tg);
}

fn decompile_component(node: &LsxNode, registry: &AllSparkRegistry) -> EffectComponent {
    let class_name = node_attr_value(node, "Type")
        .or_else(|| node_attr_value(node, "Name"))
        .unwrap_or_default();

    let start_time = node_attr_value(node, "StartTime").unwrap_or_else(|| "0".to_string());
    let end_time = node_attr_value(node, "EndTime").unwrap_or_else(|| "0".to_string());

    let mut comp = EffectComponent {
        class_name: class_name.clone(),
        start: start_time.clone(),
        end: end_time.clone(),
        instance_name: node_attr_value(node, "ID")
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
        properties: Vec::new(),
        property_groups: Vec::new(),
        modules: Vec::new(),
    };

    // 1. Synthesize implicit "Name" property
    comp.properties.push(EffectProperty {
        guid: IMPLICIT_NAME_GUID.to_string(),
        data: vec![Datum {
            value: Some(class_name.clone()),
            ..Datum::default()
        }],
        platform_metadata: Vec::new(),
    });

    // 2. Synthesize implicit "Position" (Start / End Time) property
    comp.properties.push(EffectProperty {
        guid: IMPLICIT_POSITION_GUID.to_string(),
        data: vec![Datum {
            value: Some(format!("{start_time},{end_time}")),
            ..Datum::default()
        }],
        platform_metadata: Vec::new(),
    });

    // 3. Process nested Property child nodes
    let properties_node = find_child_node(node, "Properties");
    if let Some(props) = properties_node {
        for prop_node in &props.children {
            if prop_node.id != "Property" {
                continue;
            }
            decompile_property_node(prop_node, &class_name, &mut comp, registry);
        }
    }

    // 4. Add property groups from XCD definition (with fresh instance GUIDs)
    if let Some(comp_def) = registry.components.get(&class_name) {
        for pg_def in &comp_def.property_groups {
            comp.property_groups.push(PropertyGroup {
                guid: uuid::Uuid::new_v4().to_string(),
                name: pg_def.name.clone(),
                collapsed: pg_def.collapsed.clone(),
            });
        }
    }

    // 5. Add the "Required" module (always present on every component)
    comp.modules.push(EffectModule {
        guid: REQUIRED_MODULE_GUID.to_string(),
        muted: "False".to_string(),
        index: 0,
    });

    comp
}

/// Convert a runtime `<node id="Property">` into an `EffectProperty`.
fn decompile_property_node(
    prop_node: &LsxNode,
    class_name: &str,
    comp: &mut EffectComponent,
    registry: &AllSparkRegistry,
) {
    let _attr_name = node_attr_value(prop_node, "AttributeName").unwrap_or_default();
    let full_name = node_attr_value(prop_node, "FullName").unwrap_or_default();

    // Skip the implicit "Name" property — already synthesized above
    if full_name == "Name" {
        return;
    }

    // Look up GUID via FullName in the AllSpark registry
    let guid = registry
        .resolve_property_name(class_name, &full_name)
        .map(|s| s.to_string())
        .unwrap_or_else(|| full_name.clone());

    // Check for animation Frames child
    let frames_node = find_child_node(prop_node, "Frames");

    if let Some(frames) = frames_node {
        // Animated property → RampChannelData
        let rcd = convert_frames_to_rcd(frames);
        comp.properties.push(EffectProperty {
            guid,
            data: vec![Datum {
                ramp_channel_data: Some(rcd),
                ..Datum::default()
            }],
            platform_metadata: Vec::new(),
        });
    } else {
        // Simple property → value
        let raw_value = node_attr_value(prop_node, "Value").unwrap_or_default();

        // Get the LSX type from the Value attribute for format conversion
        let value_attr_type = prop_node
            .attributes
            .iter()
            .find(|a| a.id == "Value")
            .map(|a| a.attr_type.as_str())
            .unwrap_or("FixedString");

        let toolkit_value = runtime_value_to_toolkit(&raw_value, value_attr_type);

        comp.properties.push(EffectProperty {
            guid,
            data: vec![Datum {
                value: Some(toolkit_value),
                ..Datum::default()
            }],
            platform_metadata: Vec::new(),
        });
    }
}

/// Convert a runtime `<node id="Frames">` into a `RampChannelData`.
fn convert_frames_to_rcd(frames_node: &LsxNode) -> RampChannelData {
    let _frame_type = node_attr_value(frames_node, "FrameType").unwrap_or_else(|| "0".to_string());

    let mut keyframes = Vec::new();

    for frame in &frames_node.children {
        if frame.id != "Frame" {
            continue;
        }

        let time = node_attr_value(frame, "Time").unwrap_or_else(|| "0".to_string());

        // Frame value can be in different attributes depending on the property type
        // (Color, Value, etc.) — find the first non-Time attribute and use it
        let value = frame
            .attributes
            .iter()
            .find(|a| a.id != "Time")
            .map(|a| {
                // Convert vector values from space-separated to comma-separated
                runtime_value_to_toolkit(&a.value, &a.attr_type)
            })
            .unwrap_or_else(|| "0".to_string());

        keyframes.push(Keyframe {
            time,
            value,
            interpolation: node_attr_value(frame, "InterpolationType"),
        });
    }

    // Create a single ramp channel for the frames
    let channel = RampChannel {
        channel_type: "Linear".to_string(),
        id: uuid::Uuid::new_v4().to_string(),
        selected: false,
        keyframes,
    };

    RampChannelData {
        channels: vec![channel],
    }
}

/// Find the first direct child node with the given id.
fn find_child_node<'a>(parent: &'a LsxNode, child_id: &str) -> Option<&'a LsxNode> {
    parent.children.iter().find(|n| n.id == child_id)
}

// ═══════════════════════════════════════════════════════════════════
//  EffectResource  →  LsxResource  (compile: .lsefx → .lsfx)
// ═══════════════════════════════════════════════════════════════════

/// Convert a toolkit `EffectResource` into a runtime `LsxResource` (for `.lsfx` binary).
///
/// Produces the vanilla nested format:
/// ```text
/// Effect
///   Duration (float) = max EndTime
///   EffectComponents
///     EffectComponent  (envelope: EndTime, ID, Name, StartTime, Track, Type)
///       Properties
///         Property  (AttributeName, FullName, Type uint8, Value typed)
///           [Frames]  (for animated properties)
/// ```
pub fn effect_to_lsx(effect: &EffectResource, registry: &AllSparkRegistry) -> LsxResource {
    let mut component_nodes: Vec<LsxNode> = Vec::new();
    let mut max_end_time: f64 = 0.0;
    let mut track_index: u32 = 0;

    for tg in &effect.track_groups {
        for track in &tg.tracks {
            for comp in &track.components {
                if let Ok(et) = comp.end.parse::<f64>() {
                    if et > max_end_time {
                        max_end_time = et;
                    }
                }
                let node = compile_component(comp, track_index, registry);
                component_nodes.push(node);
            }
            track_index += 1;
        }
    }

    let effect_components = LsxNode {
        id: "EffectComponents".to_string(),
        key_attribute: None,
        attributes: Vec::new(),
        children: component_nodes,
        commented: false,
    };

    let effect_node = LsxNode {
        id: "Effect".to_string(),
        key_attribute: None,
        attributes: vec![lsx_attr("Duration", "float", &format_float(max_end_time))],
        children: vec![effect_components],
        commented: false,
    };

    LsxResource {
        regions: vec![LsxRegion {
            id: "Effect".to_string(),
            nodes: vec![effect_node],
        }],
    }
}

fn compile_component(
    comp: &EffectComponent,
    track_index: u32,
    registry: &AllSparkRegistry,
) -> LsxNode {
    // Envelope attributes (alphabetical order matching vanilla)
    let attrs: Vec<LsxNodeAttribute> = vec![
        lsx_attr("EndTime", "float", &comp.end),
        lsx_attr("ID", "guid", &comp.instance_name),
        lsx_attr("Name", "LSString", &comp.class_name),
        lsx_attr("StartTime", "float", &comp.start),
        lsx_attr("Track", "uint32", &track_index.to_string()),
        lsx_attr("Type", "LSString", &comp.class_name),
    ];

    // Build Property child nodes
    let mut property_nodes: Vec<LsxNode> = Vec::new();

    for prop in &comp.properties {
        // Skip the "Start / End Time" implicit property — it is reconstructed
        // from the envelope StartTime/EndTime attributes and never appears as
        // a Property node in vanilla .lsx output.
        let guid_lower = prop.guid.to_lowercase();
        if guid_lower == IMPLICIT_POSITION_GUID {
            continue;
        }

        if let Some(prop_node) = compile_property(prop, &comp.class_name, registry) {
            property_nodes.push(prop_node);
        }
    }

    let mut children = Vec::new();
    if !property_nodes.is_empty() {
        children.push(LsxNode {
            id: "Properties".to_string(),
            key_attribute: None,
            attributes: Vec::new(),
            children: property_nodes,
            commented: false,
        });
    }

    LsxNode {
        id: "EffectComponent".to_string(),
        key_attribute: None,
        attributes: attrs,
        children,
        commented: false,
    }
}

/// Build a runtime `<node id="Property">` from an `EffectProperty`.
fn compile_property(
    prop: &EffectProperty,
    component_class: &str,
    registry: &AllSparkRegistry,
) -> Option<LsxNode> {
    let guid_lower = prop.guid.to_lowercase();

    // Resolve property name and type from AllSpark
    let prop_name = registry
        .resolve_property_guid(&guid_lower)
        .unwrap_or(&prop.guid);

    let (allspark_type, full_name) =
        resolve_property_meta(&guid_lower, prop_name, component_class, registry);

    // Resolve the runtime AttributeName (override table or leaf of FullName)
    let attr_name = fullname_to_attribute_name(&full_name);

    let type_uint8 = allspark_type_to_uint8(&allspark_type);

    let datum = prop.data.first()?;

    // Animated property → Frames children
    if let Some(ref rcd) = datum.ramp_channel_data {
        let frames_node = compile_frames(rcd, &allspark_type);
        let mut attrs = vec![
            lsx_attr("AttributeName", "FixedString", attr_name),
            lsx_attr("FullName", "FixedString", &full_name),
            lsx_attr("Type", "uint8", &type_uint8.to_string()),
        ];
        // Animated properties don't have a Value attribute — only Frames children
        let _ = &mut attrs; // suppress unused warning
        return Some(LsxNode {
            id: "Property".to_string(),
            key_attribute: None,
            attributes: attrs,
            children: vec![frames_node],
            commented: false,
        });
    }

    let val = datum.value.as_deref().unwrap_or("");

    // Range properties (IntegerRangeSlider / FloatRangeSlider) → Min/Max attrs
    if allspark_type == "IntegerRangeSlider" || allspark_type == "FloatRangeSlider" {
        let (range_type, min_val, max_val) = split_range_value(val, &allspark_type);
        return Some(LsxNode {
            id: "Property".to_string(),
            key_attribute: None,
            attributes: vec![
                lsx_attr("AttributeName", "FixedString", attr_name),
                lsx_attr("FullName", "FixedString", &full_name),
                lsx_attr("Max", range_type, &max_val),
                lsx_attr("Min", range_type, &min_val),
                lsx_attr("Type", "uint8", &type_uint8.to_string()),
            ],
            children: Vec::new(),
            commented: false,
        });
    }

    // Simple property → Value attribute
    let lsx_type = allspark_type_to_lsx(&allspark_type);
    let runtime_value = toolkit_value_to_runtime(val, &lsx_type);

    Some(LsxNode {
        id: "Property".to_string(),
        key_attribute: None,
        attributes: vec![
            lsx_attr("AttributeName", "FixedString", attr_name),
            lsx_attr("FullName", "FixedString", &full_name),
            lsx_attr("Type", "uint8", &type_uint8.to_string()),
            lsx_attr("Value", &lsx_type, &runtime_value),
        ],
        children: Vec::new(),
        commented: false,
    })
}

/// Compile `RampChannelData` into a runtime `<node id="Frames">`.
fn compile_frames(rcd: &RampChannelData, _allspark_type: &str) -> LsxNode {
    let channel = match rcd.channels.first() {
        Some(ch) => ch,
        None => {
            return LsxNode {
                id: "Frames".to_string(),
                key_attribute: None,
                attributes: vec![lsx_attr("FrameType", "uint8", "0")],
                children: Vec::new(),
                commented: false,
            };
        }
    };

    let frame_type = "0"; // Simple keyframes (Time + Value)

    let frame_nodes: Vec<LsxNode> = channel
        .keyframes
        .iter()
        .map(|kf| {
            let runtime_val = toolkit_value_to_runtime(&kf.value, "float");
            LsxNode {
                id: "Frame".to_string(),
                key_attribute: None,
                attributes: vec![
                    lsx_attr("Time", "float", &kf.time),
                    lsx_attr("Value", "float", &runtime_val),
                ],
                children: Vec::new(),
                commented: false,
            }
        })
        .collect();

    LsxNode {
        id: "Frames".to_string(),
        key_attribute: None,
        attributes: vec![lsx_attr("FrameType", "uint8", frame_type)],
        children: frame_nodes,
        commented: false,
    }
}

/// Resolve the AllSpark type name and the hierarchical FullName for a property.
fn resolve_property_meta(
    guid_lower: &str,
    prop_name: &str,
    component_class: &str,
    registry: &AllSparkRegistry,
) -> (String, String) {
    // Look up in the component's XCD definition
    if let Some(comp_def) = registry.components.get(component_class) {
        if let Some(prop_def) = comp_def.properties.get(guid_lower) {
            let full_name = build_full_name(comp_def, guid_lower, &prop_def.name);
            return (prop_def.type_name.clone(), full_name);
        }
    }

    // Fallback: just use the property name
    ("FixedString".to_string(), prop_name.to_string())
}

/// Build the hierarchical FullName by checking which property group contains
/// this property and prepending the group name(s).
fn build_full_name(
    comp_def: &crate::allspark::ComponentDef,
    guid_lower: &str,
    leaf_name: &str,
) -> String {
    for pg in &comp_def.property_groups {
        if pg.property_refs.contains(&guid_lower.to_string()) {
            // Property is in this group — prepend group name
            if pg.name == "Property Group" || pg.name.is_empty() {
                return leaf_name.to_string();
            }
            return format!("{}.{}", pg.name, leaf_name);
        }
    }
    leaf_name.to_string()
}

/// Map AllSpark definition type to the uint8 Type value used in runtime `.lsx`.
fn allspark_type_to_uint8(allspark_type: &str) -> u8 {
    match allspark_type {
        "Boolean" => 0,
        "IntegerSlider" => 1,
        "IntegerRangeSlider" => 2,
        "ColorRamp" => 3,
        "FloatSlider" => 4,
        "FloatRangeSlider" => 5,
        "Ramp" => 6,
        "Text" | "CustomString" | "ShortNameList" | "DropDownList" | "AnimationSubSet" => 7,
        "Vector3" => 8,
        "Guid" => 10,
        // Fallback: treat as string
        _ => 7,
    }
}

/// Map AllSpark definition type to the LSX attribute type string.
fn allspark_type_to_lsx(allspark_type: &str) -> String {
    match allspark_type {
        "Boolean" => "bool",
        "FloatSlider" | "FloatRangeSlider" => "float",
        "IntegerSlider" | "IntegerRangeSlider" => "int32",
        "Vector3" => "fvec3",
        "ColorRamp" => "fvec4",
        "Guid" => "FixedString",
        "Ramp" => "float",
        _ => "LSString",
    }
    .to_string()
}

/// Split a toolkit range value "min,max" into (lsx_type, min, max).
fn split_range_value(val: &str, allspark_type: &str) -> (&'static str, String, String) {
    let parts: Vec<&str> = val.splitn(2, ',').collect();
    let (min, max) = if parts.len() == 2 {
        (parts[0].to_string(), parts[1].to_string())
    } else {
        (val.to_string(), val.to_string())
    };
    let lsx_type = if allspark_type == "IntegerRangeSlider" {
        "int32"
    } else {
        "float"
    };
    (lsx_type, min, max)
}

/// Format a float for the Duration attribute, dropping unnecessary trailing zeros.
fn format_float(val: f64) -> String {
    if val == val.floor() && val.abs() < 1e15 {
        format!("{}", val as i64)
    } else {
        format!("{val}")
    }
}

#[allow(dead_code)]
fn guess_type_from_value(value: &str) -> String {
    if value.contains(',') {
        let parts = value.split(',').count();
        return match parts {
            2 => "fvec2",
            3 => "fvec3",
            4 => "fvec4",
            _ => "FixedString",
        }
        .to_string();
    }
    // GUID pattern: 36 chars with 4 dashes
    if value.len() == 36 && value.chars().filter(|c| *c == '-').count() == 4 {
        return "guid".to_string();
    }
    if value.parse::<f64>().is_ok() {
        if value.contains('.') {
            return "float".to_string();
        }
        return "int32".to_string();
    }
    "FixedString".to_string()
}

// ── Helpers ─────────────────────────────────────────────────────────

fn node_attr_value(node: &LsxNode, attr_id: &str) -> Option<String> {
    node.attributes
        .iter()
        .find(|a| a.id == attr_id)
        .map(|a| a.value.clone())
}

fn lsx_attr(id: &str, attr_type: &str, value: &str) -> LsxNodeAttribute {
    LsxNodeAttribute {
        id: id.to_string(),
        attr_type: attr_type.to_string(),
        value: value.to_string(),
        handle: None,
        version: None,
        arguments: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{LsxRegion, LsxResource};

    fn simple_registry() -> AllSparkRegistry {
        let xcd = r#"
<root version="0.0">
  <components>
    <component name="BoundingSphere" tooltip="" color="">
      <properties>
        <property name="Center" id="c1115291-39d1-43a2-8259-31c2ef4dbd93">
          <definition type="Vector3" specializable="True" tooltip="">
            <data><datum value="0,0,0"/></data>
          </definition>
        </property>
        <property name="Radius" id="ba2ee0f9-d369-4d36-bd63-30d4c8c46a0a">
          <definition type="FloatSlider" specializable="True" tooltip="">
            <data><datum value="1"/></data>
          </definition>
        </property>
        <property name="Visible" id="cae8dded-764a-4529-91c0-9a1c32e367f2">
          <definition type="Boolean" specializable="True" tooltip="">
            <data><datum value="1"/></data>
          </definition>
        </property>
        <propertygroup id="88322fa2-e0bc-4656-a7f1-483bbc5f092e" name="Property Group" collapsed="False">
          <properties>
            <property id="ef1d7d1e-02b6-4548-80d9-5ef2fbcda237"/>
            <property id="035b5248-d0ca-44b7-853f-3acb84110e67"/>
            <property id="c1115291-39d1-43a2-8259-31c2ef4dbd93"/>
            <property id="ba2ee0f9-d369-4d36-bd63-30d4c8c46a0a"/>
          </properties>
        </propertygroup>
      </properties>
    </component>
  </components>
</root>"#;

        let xmd = r#"
<root version="0.0">
  <modules>
    <module name="Required" id="286DF729-035E-4BB8-A210-C836DDBBBACC" required="True">
      <properties>
        <property name="Name" id="ef1d7d1e-02b6-4548-80d9-5ef2fbcda237" component="ParticleSystem" />
        <property name="Start / End Time" id="035b5248-d0ca-44b7-853f-3acb84110e67" component="ParticleSystem" />
      </properties>
    </module>
  </modules>
</root>"#;

        let mut reg = AllSparkRegistry::default();
        reg.parse_xcd(xcd).unwrap();
        reg.parse_xmd(xmd).unwrap();
        reg
    }

    /// Build a runtime LsxResource matching the actual vanilla .lsx format
    /// (nested Property child nodes, not flat attributes).
    fn make_bounding_sphere_lsx() -> LsxResource {
        LsxResource {
            regions: vec![LsxRegion {
                id: "Effect".to_string(),
                nodes: vec![LsxNode {
                    id: "Effect".to_string(),
                    key_attribute: None,
                    attributes: vec![lsx_attr("Duration", "float", "2")],
                    children: vec![LsxNode {
                        id: "EffectComponents".to_string(),
                        key_attribute: None,
                        attributes: Vec::new(),
                        children: vec![LsxNode {
                            id: "EffectComponent".to_string(),
                            key_attribute: None,
                            attributes: vec![
                                lsx_attr("EndTime", "float", "2"),
                                lsx_attr("ID", "guid", "97c55db4-514c-489b-86b7-65e0fe73244a"),
                                lsx_attr("Name", "LSString", "BoundingSphere"),
                                lsx_attr("StartTime", "float", "0"),
                                lsx_attr("Track", "uint32", "0"),
                                lsx_attr("Type", "LSString", "BoundingSphere"),
                            ],
                            children: vec![LsxNode {
                                id: "Properties".to_string(),
                                key_attribute: None,
                                attributes: Vec::new(),
                                children: vec![
                                    // Name property (implicit)
                                    LsxNode {
                                        id: "Property".to_string(),
                                        key_attribute: None,
                                        attributes: vec![
                                            lsx_attr("AttributeName", "FixedString", "Name"),
                                            lsx_attr("FullName", "FixedString", "Name"),
                                            lsx_attr("Type", "uint8", "7"),
                                            lsx_attr("Value", "LSString", "BoundingSphere"),
                                        ],
                                        children: Vec::new(),
                                        commented: false,
                                    },
                                    // Center property
                                    LsxNode {
                                        id: "Property".to_string(),
                                        key_attribute: None,
                                        attributes: vec![
                                            lsx_attr("AttributeName", "FixedString", "Position"),
                                            lsx_attr("FullName", "FixedString", "Center"),
                                            lsx_attr("Type", "uint8", "8"),
                                            lsx_attr("Value", "fvec3", "0 0 0"),
                                        ],
                                        children: Vec::new(),
                                        commented: false,
                                    },
                                    // Radius property
                                    LsxNode {
                                        id: "Property".to_string(),
                                        key_attribute: None,
                                        attributes: vec![
                                            lsx_attr("AttributeName", "FixedString", "Radius"),
                                            lsx_attr("FullName", "FixedString", "Radius"),
                                            lsx_attr("Type", "uint8", "4"),
                                            lsx_attr("Value", "float", "3"),
                                        ],
                                        children: Vec::new(),
                                        commented: false,
                                    },
                                    // Visible property
                                    LsxNode {
                                        id: "Property".to_string(),
                                        key_attribute: None,
                                        attributes: vec![
                                            lsx_attr("AttributeName", "FixedString", "Visible"),
                                            lsx_attr("FullName", "FixedString", "Visible"),
                                            lsx_attr("Type", "uint8", "0"),
                                            lsx_attr("Value", "bool", "True"),
                                        ],
                                        children: Vec::new(),
                                        commented: false,
                                    },
                                ],
                                commented: false,
                            }],
                            commented: false,
                        }],
                        commented: false,
                    }],
                    commented: false,
                }],
            }],
        }
    }

    #[test]
    fn decompile_nested_property_format() {
        let registry = simple_registry();
        let resource = make_bounding_sphere_lsx();

        let effect = lsx_to_effect(&resource, &registry);
        assert_eq!(effect.track_groups.len(), 1);

        let comp = &effect.track_groups[0].tracks[0].components[0];
        assert_eq!(comp.class_name, "BoundingSphere");
        assert_eq!(comp.instance_name, "97c55db4-514c-489b-86b7-65e0fe73244a");
        assert_eq!(comp.start, "0");
        assert_eq!(comp.end, "2");

        // Implicit Name property
        let name_prop = comp
            .properties
            .iter()
            .find(|p| p.guid == "ef1d7d1e-02b6-4548-80d9-5ef2fbcda237")
            .expect("Name property should exist");
        assert_eq!(name_prop.data[0].value.as_deref(), Some("BoundingSphere"));

        // Implicit Position (start/end time) property
        let pos_prop = comp
            .properties
            .iter()
            .find(|p| p.guid == "035b5248-d0ca-44b7-853f-3acb84110e67")
            .expect("Position property should exist");
        assert_eq!(pos_prop.data[0].value.as_deref(), Some("0,2"));

        // Center property — should be comma-separated
        let center = comp
            .properties
            .iter()
            .find(|p| p.guid == "c1115291-39d1-43a2-8259-31c2ef4dbd93")
            .expect("Center property should exist");
        assert_eq!(center.data[0].value.as_deref(), Some("0,0,0"));

        // Radius property
        let radius = comp
            .properties
            .iter()
            .find(|p| p.guid == "ba2ee0f9-d369-4d36-bd63-30d4c8c46a0a")
            .expect("Radius property should exist");
        assert_eq!(radius.data[0].value.as_deref(), Some("3"));

        // Visible property — should be "1" (toolkit format)
        let visible = comp
            .properties
            .iter()
            .find(|p| p.guid == "cae8dded-764a-4529-91c0-9a1c32e367f2")
            .expect("Visible property should exist");
        assert_eq!(visible.data[0].value.as_deref(), Some("1"));

        // Property group from XCD (with fresh instance GUID, not XCD GUID)
        assert_eq!(comp.property_groups.len(), 1);
        assert_ne!(
            comp.property_groups[0].guid, "88322fa2-e0bc-4656-a7f1-483bbc5f092e",
            "Should generate a fresh GUID, not reuse the XCD definition GUID"
        );
        assert_eq!(comp.property_groups[0].name, "Property Group");
        assert_eq!(comp.property_groups[0].collapsed, "False");

        // Required module
        assert_eq!(comp.modules.len(), 1);
        assert_eq!(comp.modules[0].guid, "286df729-035e-4bb8-a210-c836ddbbbacc");
    }

    #[test]
    fn compile_simple_component() {
        let registry = simple_registry();

        let effect = EffectResource {
            track_groups: vec![TrackGroup {
                name: "TG".to_string(),
                ids: vec![TrackGroupId {
                    value: "1".to_string(),
                }],
                tracks: vec![Track {
                    name: "Track".to_string(),
                    components: vec![EffectComponent {
                        class_name: "BoundingSphere".to_string(),
                        start: "0".to_string(),
                        end: "5".to_string(),
                        instance_name: "inst-0001".to_string(),
                        properties: vec![
                            EffectProperty {
                                guid: "c1115291-39d1-43a2-8259-31c2ef4dbd93".to_string(),
                                data: vec![Datum {
                                    value: Some("0,1,0".to_string()),
                                    ..Datum::default()
                                }],
                                platform_metadata: Vec::new(),
                            },
                            EffectProperty {
                                guid: "ba2ee0f9-d369-4d36-bd63-30d4c8c46a0a".to_string(),
                                data: vec![Datum {
                                    value: Some("5".to_string()),
                                    ..Datum::default()
                                }],
                                platform_metadata: Vec::new(),
                            },
                        ],
                        property_groups: Vec::new(),
                        modules: Vec::new(),
                    }],
                    ..Track::default()
                }],
            }],
            ..EffectResource::default()
        };

        let resource = effect_to_lsx(&effect, &registry);
        assert_eq!(resource.regions.len(), 1);
        assert_eq!(resource.regions[0].id, "Effect");

        // Effect node
        let effect_node = &resource.regions[0].nodes[0];
        assert_eq!(effect_node.id, "Effect");

        // Duration attribute
        let duration = effect_node
            .attributes
            .iter()
            .find(|a| a.id == "Duration")
            .unwrap();
        assert_eq!(duration.value, "5");
        assert_eq!(duration.attr_type, "float");

        // EffectComponents container
        let ec = &effect_node.children[0];
        assert_eq!(ec.id, "EffectComponents");
        assert_eq!(ec.children.len(), 1);

        let node = &ec.children[0]; // EffectComponent
        assert_eq!(node.id, "EffectComponent");

        // Check envelope attrs
        let type_attr = node.attributes.iter().find(|a| a.id == "Type").unwrap();
        assert_eq!(type_attr.value, "BoundingSphere");
        assert_eq!(type_attr.attr_type, "LSString");

        let end_time = node.attributes.iter().find(|a| a.id == "EndTime").unwrap();
        assert_eq!(end_time.value, "5");
        assert_eq!(end_time.attr_type, "float");

        let start_time = node
            .attributes
            .iter()
            .find(|a| a.id == "StartTime")
            .unwrap();
        assert_eq!(start_time.value, "0");

        let track_attr = node.attributes.iter().find(|a| a.id == "Track").unwrap();
        assert_eq!(track_attr.value, "0");
        assert_eq!(track_attr.attr_type, "uint32");

        // Properties container
        let props_node = &node.children[0];
        assert_eq!(props_node.id, "Properties");
        assert_eq!(props_node.children.len(), 2); // Center + Radius

        // Center property: should be space-separated in runtime
        let center_prop = &props_node.children[0];
        assert_eq!(center_prop.id, "Property");
        let full_name = center_prop
            .attributes
            .iter()
            .find(|a| a.id == "FullName")
            .unwrap();
        assert_eq!(full_name.value, "Center");
        let prop_type = center_prop
            .attributes
            .iter()
            .find(|a| a.id == "Type")
            .unwrap();
        assert_eq!(prop_type.value, "8"); // Vector3
        let value = center_prop
            .attributes
            .iter()
            .find(|a| a.id == "Value")
            .unwrap();
        assert_eq!(value.value, "0 1 0");
        assert_eq!(value.attr_type, "fvec3");

        // Radius property
        let radius_prop = &props_node.children[1];
        let full_name = radius_prop
            .attributes
            .iter()
            .find(|a| a.id == "FullName")
            .unwrap();
        assert_eq!(full_name.value, "Radius");
        let prop_type = radius_prop
            .attributes
            .iter()
            .find(|a| a.id == "Type")
            .unwrap();
        assert_eq!(prop_type.value, "4"); // FloatSlider
        let value = radius_prop
            .attributes
            .iter()
            .find(|a| a.id == "Value")
            .unwrap();
        assert_eq!(value.value, "5");
        assert_eq!(value.attr_type, "float");
    }

    // ── Unhappy-path tests ─────────────────────────────────────────

    #[test]
    fn decompile_no_effect_region() {
        let registry = simple_registry();
        let resource = LsxResource {
            regions: vec![LsxRegion {
                id: "NotEffect".to_string(),
                nodes: Vec::new(),
            }],
        };
        let effect = lsx_to_effect(&resource, &registry);
        assert!(
            effect.track_groups.is_empty(),
            "no Effect region → empty result"
        );
    }

    #[test]
    fn decompile_empty_resource() {
        let registry = simple_registry();
        let resource = LsxResource { regions: vec![] };
        let effect = lsx_to_effect(&resource, &registry);
        assert!(effect.track_groups.is_empty());
    }

    #[test]
    fn decompile_effect_region_with_no_components() {
        let registry = simple_registry();
        let resource = LsxResource {
            regions: vec![LsxRegion {
                id: "Effect".to_string(),
                nodes: vec![LsxNode {
                    id: "Effect".to_string(),
                    key_attribute: None,
                    attributes: vec![lsx_attr("Duration", "float", "10")],
                    children: vec![LsxNode {
                        id: "EffectComponents".to_string(),
                        key_attribute: None,
                        attributes: Vec::new(),
                        children: Vec::new(), // no EffectComponent children
                        commented: false,
                    }],
                    commented: false,
                }],
            }],
        };
        let effect = lsx_to_effect(&resource, &registry);
        assert!(
            effect.track_groups.is_empty(),
            "no components → no track groups"
        );
    }

    #[test]
    fn decompile_component_missing_all_attributes() {
        let registry = simple_registry();
        // EffectComponent node with no attributes at all
        let resource = LsxResource {
            regions: vec![LsxRegion {
                id: "Effect".to_string(),
                nodes: vec![LsxNode {
                    id: "EffectComponent".to_string(),
                    key_attribute: None,
                    attributes: Vec::new(),
                    children: Vec::new(),
                    commented: false,
                }],
            }],
        };
        let effect = lsx_to_effect(&resource, &registry);
        assert_eq!(effect.track_groups.len(), 1);
        let comp = &effect.track_groups[0].tracks[0].components[0];
        // Should not panic — uses defaults
        assert!(comp.class_name.is_empty());
        assert_eq!(comp.start, "0");
        assert_eq!(comp.end, "0");
    }

    #[test]
    fn decompile_unknown_component_class() {
        let registry = simple_registry();
        // Component class not in the registry
        let resource = LsxResource {
            regions: vec![LsxRegion {
                id: "Effect".to_string(),
                nodes: vec![LsxNode {
                    id: "EffectComponent".to_string(),
                    key_attribute: None,
                    attributes: vec![
                        lsx_attr("Type", "LSString", "UnknownComponentXYZ"),
                        lsx_attr("ID", "guid", "aaaa-bbbb"),
                        lsx_attr("StartTime", "float", "0"),
                        lsx_attr("EndTime", "float", "5"),
                    ],
                    children: Vec::new(),
                    commented: false,
                }],
            }],
        };
        let effect = lsx_to_effect(&resource, &registry);
        let comp = &effect.track_groups[0].tracks[0].components[0];
        assert_eq!(comp.class_name, "UnknownComponentXYZ");
        // Should still produce implicit properties + Required module
        assert!(
            comp.properties.len() >= 2,
            "at least Name + Position implicit props"
        );
        assert_eq!(comp.modules.len(), 1); // Required module
                                           // No property groups since class is unknown
        assert!(comp.property_groups.is_empty());
    }

    #[test]
    fn compile_property_with_unknown_guid() {
        let registry = simple_registry();

        let effect = EffectResource {
            track_groups: vec![TrackGroup {
                name: "TG".to_string(),
                ids: Vec::new(),
                tracks: vec![Track {
                    name: "T".to_string(),
                    components: vec![EffectComponent {
                        class_name: "BoundingSphere".to_string(),
                        start: "0".to_string(),
                        end: "1".to_string(),
                        instance_name: "inst".to_string(),
                        properties: vec![EffectProperty {
                            guid: "00000000-dead-beef-0000-000000000000".to_string(),
                            data: vec![Datum {
                                value: Some("42".to_string()),
                                ..Datum::default()
                            }],
                            platform_metadata: Vec::new(),
                        }],
                        property_groups: Vec::new(),
                        modules: Vec::new(),
                    }],
                    ..Track::default()
                }],
            }],
            ..EffectResource::default()
        };

        let resource = effect_to_lsx(&effect, &registry);
        let ec = &resource.regions[0].nodes[0].children[0];
        assert_eq!(ec.children.len(), 1);
        // The unknown-GUID property should still produce a node (fallback)
        let props = &ec.children[0].children[0];
        assert_eq!(props.id, "Properties");
        let prop_node = &props.children[0];
        // FullName fallback: uses the GUID as the name
        let full = prop_node
            .attributes
            .iter()
            .find(|a| a.id == "FullName")
            .unwrap();
        assert_eq!(full.value, "00000000-dead-beef-0000-000000000000");
    }

    #[test]
    fn compile_property_with_no_data_is_skipped() {
        let registry = simple_registry();

        let effect = EffectResource {
            track_groups: vec![TrackGroup {
                name: "TG".to_string(),
                ids: Vec::new(),
                tracks: vec![Track {
                    name: "T".to_string(),
                    components: vec![EffectComponent {
                        class_name: "BoundingSphere".to_string(),
                        start: "0".to_string(),
                        end: "1".to_string(),
                        instance_name: "inst".to_string(),
                        properties: vec![EffectProperty {
                            guid: "c1115291-39d1-43a2-8259-31c2ef4dbd93".to_string(),
                            data: Vec::new(), // no data
                            platform_metadata: Vec::new(),
                        }],
                        property_groups: Vec::new(),
                        modules: Vec::new(),
                    }],
                    ..Track::default()
                }],
            }],
            ..EffectResource::default()
        };

        let resource = effect_to_lsx(&effect, &registry);
        // compile_property returns None when data is empty → no Properties container
        let ec_node = &resource.regions[0].nodes[0].children[0].children[0];
        assert!(
            ec_node.children.is_empty(),
            "property with no data should be skipped"
        );
    }

    #[test]
    fn compile_empty_effect() {
        let registry = simple_registry();
        let effect = EffectResource::default();
        let resource = effect_to_lsx(&effect, &registry);
        assert_eq!(resource.regions.len(), 1);
        let effect_node = &resource.regions[0].nodes[0];
        let ec = &effect_node.children[0];
        assert_eq!(ec.id, "EffectComponents");
        assert!(ec.children.is_empty());
    }

    #[test]
    fn fullname_override_returns_correct_value() {
        assert_eq!(fullname_to_attribute_name("Center"), "Position");
        assert_eq!(fullname_to_attribute_name("Behavior.Rotation"), "RPM");
        assert_eq!(fullname_to_attribute_name("BoundingBox"), "");
        assert_eq!(
            fullname_to_attribute_name("Particle.Appearance.Random Initial Rotation"),
            ""
        );
    }

    #[test]
    fn fullname_no_override_returns_leaf() {
        assert_eq!(fullname_to_attribute_name("Radius"), "Radius");
        assert_eq!(
            fullname_to_attribute_name("Some.Nested.PropertyName"),
            "PropertyName"
        );
        assert_eq!(fullname_to_attribute_name(""), "");
    }

    #[test]
    fn decompile_compile_roundtrip_preserves_unknown_class() {
        // Exercise the full decompile → compile path with a class unknown to the registry
        let registry = simple_registry();
        let resource = LsxResource {
            regions: vec![LsxRegion {
                id: "Effect".to_string(),
                nodes: vec![LsxNode {
                    id: "EffectComponent".to_string(),
                    key_attribute: None,
                    attributes: vec![
                        lsx_attr("Type", "LSString", "FakeClass"),
                        lsx_attr("Name", "LSString", "FakeClass"),
                        lsx_attr("ID", "guid", "11111111-2222-3333-4444-555555555555"),
                        lsx_attr("StartTime", "float", "1"),
                        lsx_attr("EndTime", "float", "9"),
                        lsx_attr("Track", "uint32", "0"),
                    ],
                    children: Vec::new(),
                    commented: false,
                }],
            }],
        };

        let effect = lsx_to_effect(&resource, &registry);
        let compiled = effect_to_lsx(&effect, &registry);

        // Should survive round-trip without panicking
        let ec = &compiled.regions[0].nodes[0].children[0];
        assert_eq!(ec.children.len(), 1);
        let comp = &ec.children[0];
        let type_attr = comp.attributes.iter().find(|a| a.id == "Type").unwrap();
        assert_eq!(type_attr.value, "FakeClass");
    }
}
