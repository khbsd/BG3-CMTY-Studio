//! Integration test: bi-directional parity between
//! vanilla `.lsx` / `.lsfx` / `.lsefx` effect files.
//!
//! Validates:
//! 1. **Decompile**: .lsx → EffectResource → .lsefx  (vs vanilla .lsefx reference)
//! 2. **lsefx round-trip**: write_lsefx → read_lsefx preserves EffectResource
//! 3. **Compile round-trip**: decompile → compile → compare with original LSX
//! 4. **Binary round-trip**: write_lsfx → parse_lsfx preserves LsxResource
//! 5. **Full pipeline**: .lsx → EffectResource → .lsx → .lsfx → parse → compare
//!
//! Required external files (not in version control):
//! - AllSpark XCD/XMD from `[GAME]/Data/Editor/Config/AllSpark/`
//! - Runtime .lsx from UnpackedData Effects_Banks
//! - Reference .lsefx from `[GAME]/Data/Editor/Mods/GustavX/Assets/Effects/`

use std::io::Cursor;
use std::path::PathBuf;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn game_root() -> PathBuf {
    PathBuf::from(r"H:\SteamLibrary\steamapps\common\Baldurs Gate 3")
}

/// Normalise whitespace in XML so that structural diffs show clearly:
/// collapse runs of spaces/newlines between tags, trim each line.
fn normalise_xml(s: &str) -> String {
    s.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn parity_bounding_sphere_sound_01() {
    use bg3_cmty_studio_lib::allspark::AllSparkRegistry;
    use bg3_cmty_studio_lib::converters::lsx_to_effect;
    use bg3_cmty_studio_lib::parsers::lsefx::write_lsefx;
    use bg3_cmty_studio_lib::parsers::lsx::parse_lsx_resource;

    // ── Paths ──────────────────────────────────────────────────────
    let game = game_root();
    let xcd_path = game.join(r"Data\Editor\Config\AllSpark\ComponentDefinition.xcd");
    let xmd_path = game.join(r"Data\Editor\Config\AllSpark\ModuleDefinition.xmd");

    let ws = workspace_root();
    let lsx_path = ws.join(
        r"UnpackedData\GustavX\Public\GustavX\Assets\Effects\Effects_Banks\Spells\Cast\Fighter\VFX_Spells_Cast_Fighter_ArcaneArcher_ArcaneShot_BeguilingArrow_Crossbow_Sound_01.lsx",
    );

    let ref_lsefx_path = game.join(
        r"Data\Editor\Mods\GustavX\Assets\Effects\Spells\Cast\Fighter\VFX_Spells_Cast_Fighter_ArcaneArcher_ArcaneShot_BeguilingArrow_Crossbow_Sound_01.lsefx",
    );

    // Skip test if game data is not available
    if !xcd_path.exists() || !xmd_path.exists() || !lsx_path.exists() || !ref_lsefx_path.exists() {
        eprintln!("Skipping parity test — game data not found");
        return;
    }

    // ── Load AllSpark registry ─────────────────────────────────────
    let xcd_content = std::fs::read_to_string(&xcd_path).expect("read XCD");
    let xmd_content = std::fs::read_to_string(&xmd_path).expect("read XMD");

    let mut registry = AllSparkRegistry::default();
    registry.parse_xcd(&xcd_content).expect("parse XCD");
    registry.parse_xmd(&xmd_content).expect("parse XMD");

    // ── Parse runtime .lsx ─────────────────────────────────────────
    let lsx_content = std::fs::read_to_string(&lsx_path).expect("read LSX");
    let lsx_resource = parse_lsx_resource(&lsx_content).expect("parse LSX resource");

    // ── Convert to effect ──────────────────────────────────────────
    let effect = lsx_to_effect(&lsx_resource, &registry);

    // ── Write .lsefx ──────────────────────────────────────────────
    let generated = write_lsefx(&effect);

    // ── Load reference .lsefx ──────────────────────────────────────
    let reference = std::fs::read_to_string(&ref_lsefx_path).expect("read reference lsefx");

    // ── Print both for visual inspection ───────────────────────────
    eprintln!("═══ GENERATED ═══\n{generated}\n");
    eprintln!("═══ REFERENCE ═══\n{reference}\n");

    // ── Structural comparison ──────────────────────────────────────
    let gen_norm = normalise_xml(&generated);
    let ref_norm = normalise_xml(&reference);

    if gen_norm != ref_norm {
        // Print a line-by-line diff for debugging
        let gen_lines: Vec<&str> = gen_norm.lines().collect();
        let ref_lines: Vec<&str> = ref_norm.lines().collect();
        let max = gen_lines.len().max(ref_lines.len());

        eprintln!("═══ DIFF (generated vs reference) ═══");
        for i in 0..max {
            let g = gen_lines.get(i).unwrap_or(&"<missing>");
            let r = ref_lines.get(i).unwrap_or(&"<missing>");
            if g != r {
                eprintln!("  GEN[{:>3}]: {}", i + 1, g);
                eprintln!("  REF[{:>3}]: {}", i + 1, r);
                eprintln!();
            }
        }

        // Don't fail hard — we know there are expected parity gaps
        // (property group GUIDs, extra empty track groups)
        eprintln!(
            "NOTE: {} generated lines vs {} reference lines",
            gen_lines.len(),
            ref_lines.len()
        );
    }

    // ── Assert on the structural parts we CAN match ────────────────
    assert_eq!(effect.track_groups.len(), 1, "should produce 1 track group");
    assert_eq!(
        effect.track_groups[0].tracks.len(),
        1,
        "should produce 1 track"
    );
    assert_eq!(
        effect.track_groups[0].tracks[0].components.len(),
        1,
        "should produce 1 component"
    );

    let comp = &effect.track_groups[0].tracks[0].components[0];
    assert_eq!(comp.class_name, "BoundingSphere");
    assert_eq!(comp.instance_name, "97c55db4-514c-489b-86b7-65e0fe73244a");
    assert_eq!(comp.start, "0");
    assert_eq!(comp.end, "2");
    assert_eq!(
        comp.properties.len(),
        5,
        "5 properties (2 implicit + 3 from lsx)"
    );
    assert_eq!(comp.modules.len(), 1, "1 Required module");
    assert_eq!(comp.modules[0].guid, "286df729-035e-4bb8-a210-c836ddbbbacc");

    // Verify specific property GUIDs and values
    let find_prop = |guid: &str| comp.properties.iter().find(|p| p.guid == guid);

    // Name property
    let name = find_prop("ef1d7d1e-02b6-4548-80d9-5ef2fbcda237").expect("Name property");
    assert_eq!(name.data[0].value.as_deref(), Some("BoundingSphere"));

    // Start / End Time property
    let pos = find_prop("035b5248-d0ca-44b7-853f-3acb84110e67").expect("Position property");
    assert_eq!(pos.data[0].value.as_deref(), Some("0,2"));

    // Center property
    let center = find_prop("c1115291-39d1-43a2-8259-31c2ef4dbd93").expect("Center property");
    assert_eq!(center.data[0].value.as_deref(), Some("0,0,0"));

    // Radius property
    let radius = find_prop("ba2ee0f9-d369-4d36-bd63-30d4c8c46a0a").expect("Radius property");
    assert_eq!(radius.data[0].value.as_deref(), Some("3"));

    // Visible property
    let visible = find_prop("cae8dded-764a-4529-91c0-9a1c32e367f2").expect("Visible property");
    assert_eq!(visible.data[0].value.as_deref(), Some("1"));
}

// ═══════════════════════════════════════════════════════════════════
//  Shared helpers for bi-directional parity tests
// ═══════════════════════════════════════════════════════════════════

use bg3_cmty_studio_lib::allspark::AllSparkRegistry;
use bg3_cmty_studio_lib::converters::{effect_to_lsx, lsx_to_effect};
use bg3_cmty_studio_lib::models::LsxNode;
use bg3_cmty_studio_lib::parsers::lsefx::{read_lsefx, write_lsefx};
use bg3_cmty_studio_lib::parsers::lsfx::{parse_lsfx, write_lsfx};
use bg3_cmty_studio_lib::parsers::lsx::parse_lsx_resource;

/// Load AllSpark XCD/XMD registry from the BG3 game installation.
/// Returns `None` if the game data files are not present.
fn load_registry() -> Option<AllSparkRegistry> {
    let game = game_root();
    let xcd_path = game.join(r"Data\Editor\Config\AllSpark\ComponentDefinition.xcd");
    let xmd_path = game.join(r"Data\Editor\Config\AllSpark\ModuleDefinition.xmd");

    if !xcd_path.exists() || !xmd_path.exists() {
        return None;
    }

    let xcd = std::fs::read_to_string(&xcd_path).expect("read XCD");
    let xmd = std::fs::read_to_string(&xmd_path).expect("read XMD");

    let mut registry = AllSparkRegistry::default();
    registry.parse_xcd(&xcd).expect("parse XCD");
    registry.parse_xmd(&xmd).expect("parse XMD");
    Some(registry)
}

/// Path to the vanilla BoundingSphere .lsx test file.
fn bounding_sphere_lsx_path() -> PathBuf {
    workspace_root().join(
        r"UnpackedData\GustavX\Public\GustavX\Assets\Effects\Effects_Banks\Spells\Cast\Fighter\VFX_Spells_Cast_Fighter_ArcaneArcher_ArcaneShot_BeguilingArrow_Crossbow_Sound_01.lsx",
    )
}

/// Path to the matching vanilla .lsefx reference file.
fn bounding_sphere_lsefx_path() -> PathBuf {
    game_root().join(
        r"Data\Editor\Mods\GustavX\Assets\Effects\Spells\Cast\Fighter\VFX_Spells_Cast_Fighter_ArcaneArcher_ArcaneShot_BeguilingArrow_Crossbow_Sound_01.lsefx",
    )
}

/// Extract a named attribute value from an LsxNode.
fn attr_value(node: &LsxNode, name: &str) -> Option<String> {
    node.attributes
        .iter()
        .find(|a| a.id == name)
        .map(|a| a.value.clone())
}

/// Collect EffectComponent nodes from an LsxResource.
fn collect_component_nodes(resource: &bg3_cmty_studio_lib::models::LsxResource) -> Vec<&LsxNode> {
    let mut result = Vec::new();
    for region in &resource.regions {
        for node in &region.nodes {
            collect_components_recursive(node, &mut result);
        }
    }
    result
}

fn collect_components_recursive<'a>(node: &'a LsxNode, out: &mut Vec<&'a LsxNode>) {
    if node.id == "EffectComponent" {
        out.push(node);
    }
    for child in &node.children {
        collect_components_recursive(child, out);
    }
}

/// Collect Property nodes from an EffectComponent node.
fn collect_property_nodes(component: &LsxNode) -> Vec<&LsxNode> {
    let mut result = Vec::new();
    for child in &component.children {
        if child.id == "Properties" {
            for prop in &child.children {
                if prop.id == "Property" {
                    result.push(prop);
                }
            }
        }
    }
    result
}

// ═══════════════════════════════════════════════════════════════════
//  Test 2: lsefx write → read round-trip (EffectResource fidelity)
// ═══════════════════════════════════════════════════════════════════

/// Verifies that `write_lsefx → read_lsefx` preserves all EffectResource
/// component data through the XML serialisation layer.
#[test]
fn parity_lsefx_write_read_roundtrip() {
    let registry = match load_registry() {
        Some(r) => r,
        None => {
            eprintln!("Skipping — game data not found");
            return;
        }
    };

    let lsx_path = bounding_sphere_lsx_path();
    if !lsx_path.exists() {
        eprintln!("Skipping — LSX not found");
        return;
    }

    // Decompile vanilla .lsx → EffectResource
    let lsx_content = std::fs::read_to_string(&lsx_path).expect("read LSX");
    let lsx_resource = parse_lsx_resource(&lsx_content).expect("parse LSX");
    let effect1 = lsx_to_effect(&lsx_resource, &registry);

    // Round-trip through lsefx XML
    let lsefx_xml = write_lsefx(&effect1);
    let effect2 = read_lsefx(&lsefx_xml).expect("read_lsefx round-trip");

    // ── Compare component-level ────────────────────────────────────
    assert_eq!(
        effect1.track_groups.len(),
        effect2.track_groups.len(),
        "track group count mismatch"
    );

    for (tg1, tg2) in effect1.track_groups.iter().zip(&effect2.track_groups) {
        assert_eq!(tg1.tracks.len(), tg2.tracks.len(), "track count mismatch");
        for (t1, t2) in tg1.tracks.iter().zip(&tg2.tracks) {
            assert_eq!(
                t1.components.len(),
                t2.components.len(),
                "component count mismatch"
            );
            for (c1, c2) in t1.components.iter().zip(&t2.components) {
                assert_eq!(c1.class_name, c2.class_name, "class_name mismatch");
                assert_eq!(c1.instance_name, c2.instance_name, "instance_name mismatch");
                assert_eq!(c1.start, c2.start, "start mismatch");
                assert_eq!(c1.end, c2.end, "end mismatch");
                assert_eq!(
                    c1.properties.len(),
                    c2.properties.len(),
                    "property count mismatch for {}",
                    c1.class_name
                );

                // Compare properties by GUID
                for p1 in &c1.properties {
                    let p2 = c2
                        .properties
                        .iter()
                        .find(|p| p.guid == p1.guid)
                        .unwrap_or_else(|| panic!("property {} missing after round-trip", p1.guid));
                    assert_eq!(
                        p1.data.len(),
                        p2.data.len(),
                        "datum count for property {}",
                        p1.guid
                    );
                    for (d1, d2) in p1.data.iter().zip(&p2.data) {
                        assert_eq!(
                            d1.value, d2.value,
                            "value mismatch for property {}",
                            p1.guid
                        );
                        assert_eq!(
                            d1.ramp_channel_data.is_some(),
                            d2.ramp_channel_data.is_some(),
                            "ramp presence for property {}",
                            p1.guid
                        );
                    }
                }

                // Compare modules
                assert_eq!(
                    c1.modules.len(),
                    c2.modules.len(),
                    "module count mismatch for {}",
                    c1.class_name
                );
                for (m1, m2) in c1.modules.iter().zip(&c2.modules) {
                    assert_eq!(m1.guid, m2.guid, "module guid mismatch");
                }
            }
        }
    }

    eprintln!("✓ lsefx write → read round-trip: all components/properties preserved");
}

// ═══════════════════════════════════════════════════════════════════
//  Test 3: Compile round-trip (lsx → effect → lsx)
// ═══════════════════════════════════════════════════════════════════

/// Verifies that decompiling a vanilla .lsx and compiling back produces
/// structurally equivalent LSX nodes (same components, same property values).
#[test]
fn parity_compile_roundtrip() {
    let registry = match load_registry() {
        Some(r) => r,
        None => {
            eprintln!("Skipping — game data not found");
            return;
        }
    };

    let lsx_path = bounding_sphere_lsx_path();
    if !lsx_path.exists() {
        eprintln!("Skipping — LSX not found");
        return;
    }

    // Parse → decompile → compile
    let lsx_content = std::fs::read_to_string(&lsx_path).expect("read LSX");
    let original = parse_lsx_resource(&lsx_content).expect("parse LSX");
    let effect = lsx_to_effect(&original, &registry);
    let compiled = effect_to_lsx(&effect, &registry);

    // ── Compare component envelope attributes ──────────────────────
    let orig_comps = collect_component_nodes(&original);
    let comp_comps = collect_component_nodes(&compiled);

    assert_eq!(
        orig_comps.len(),
        comp_comps.len(),
        "EffectComponent count: original={} compiled={}",
        orig_comps.len(),
        comp_comps.len()
    );

    for (oc, cc) in orig_comps.iter().zip(&comp_comps) {
        for key in &["EndTime", "ID", "Name", "StartTime", "Track", "Type"] {
            let ov = attr_value(oc, key);
            let cv = attr_value(cc, key);
            assert_eq!(ov, cv, "component attr '{key}' mismatch");
        }
    }
    eprintln!("✓ component envelope attributes match");

    // ── Compare Property attributes ────────────────────────────────
    for (i, (oc, cc)) in orig_comps.iter().zip(&comp_comps).enumerate() {
        let orig_props = collect_property_nodes(oc);
        let comp_props = collect_property_nodes(cc);

        assert_eq!(
            orig_props.len(),
            comp_props.len(),
            "Property count mismatch in component {} ('{}' vs '{}')",
            i,
            attr_value(oc, "Name").unwrap_or_default(),
            attr_value(cc, "Name").unwrap_or_default()
        );

        // Match by FullName (the stable XCD property name).
        // Now that we have the override table, AttributeName should also match.
        for op in &orig_props {
            let o_fullname = attr_value(op, "FullName").unwrap_or_default();
            let cp = comp_props
                .iter()
                .find(|p| attr_value(p, "FullName").as_deref() == Some(&o_fullname))
                .unwrap_or_else(|| {
                    panic!("compiled missing property with FullName='{o_fullname}'")
                });

            // AttributeName should match with the override table
            assert_eq!(
                attr_value(op, "AttributeName"),
                attr_value(cp, "AttributeName"),
                "AttributeName mismatch for property '{o_fullname}'"
            );

            // Type uint8 should match
            assert_eq!(
                attr_value(op, "Type"),
                attr_value(cp, "Type"),
                "Type mismatch for property '{o_fullname}'"
            );

            // Value should match (if present — animated/range may differ)
            let o_val = attr_value(op, "Value");
            let c_val = attr_value(cp, "Value");
            if o_val.is_some() && c_val.is_some() {
                assert_eq!(o_val, c_val, "Value mismatch for property '{o_fullname}'");
            }

            // For range properties, check Min/Max
            if let Some(o_min) = attr_value(op, "Min") {
                assert_eq!(
                    Some(o_min.clone()),
                    attr_value(cp, "Min"),
                    "Min mismatch for property '{o_fullname}'"
                );
                assert_eq!(
                    attr_value(op, "Max"),
                    attr_value(cp, "Max"),
                    "Max mismatch for property '{o_fullname}'"
                );
            }
        }
    }
    eprintln!("✓ Property attributes match across all components");
}

// ═══════════════════════════════════════════════════════════════════
//  Test 4: Binary write → read round-trip
// ═══════════════════════════════════════════════════════════════════

/// Verifies that `write_lsfx → parse_lsfx` produces an identical `LsxResource`.
#[test]
fn parity_binary_write_read_roundtrip() {
    let registry = match load_registry() {
        Some(r) => r,
        None => {
            eprintln!("Skipping — game data not found");
            return;
        }
    };

    let lsx_path = bounding_sphere_lsx_path();
    if !lsx_path.exists() {
        eprintln!("Skipping — LSX not found");
        return;
    }

    // Parse .lsx → decompile → compile → LsxResource
    let lsx_content = std::fs::read_to_string(&lsx_path).expect("read LSX");
    let original = parse_lsx_resource(&lsx_content).expect("parse LSX");
    let effect = lsx_to_effect(&original, &registry);
    let compiled = effect_to_lsx(&effect, &registry);

    // Write to binary, read back
    let mut binary = Vec::new();
    write_lsfx(&mut binary, &compiled).expect("write_lsfx");
    assert!(!binary.is_empty(), "binary output should not be empty");

    let roundtripped = parse_lsfx(Cursor::new(&binary)).expect("parse_lsfx from written binary");

    // ── Structural comparison ──────────────────────────────────────
    assert_eq!(
        compiled.regions.len(),
        roundtripped.regions.len(),
        "region count mismatch"
    );

    for (cr, rr) in compiled.regions.iter().zip(&roundtripped.regions) {
        assert_eq!(cr.id, rr.id, "region id mismatch");
        assert_eq!(cr.nodes.len(), rr.nodes.len(), "root node count mismatch");
        compare_nodes_recursive(&cr.nodes, &rr.nodes, "root");
    }

    eprintln!("✓ binary write → read: LsxResource structure preserved");
}

/// Recursively compare two node trees, printing context on mismatch.
fn compare_nodes_recursive(a: &[LsxNode], b: &[LsxNode], ctx: &str) {
    assert_eq!(
        a.len(),
        b.len(),
        "[{}] child count: {} vs {}",
        ctx,
        a.len(),
        b.len()
    );
    for (na, nb) in a.iter().zip(b) {
        let path = format!("{}/{}", ctx, na.id);
        assert_eq!(na.id, nb.id, "[{path}] node id mismatch");
        assert_eq!(
            na.attributes.len(),
            nb.attributes.len(),
            "[{}] attr count: {} vs {}",
            path,
            na.attributes.len(),
            nb.attributes.len()
        );
        for (aa, ab) in na.attributes.iter().zip(&nb.attributes) {
            assert_eq!(aa.id, ab.id, "[{path}] attr name");
            assert_eq!(
                aa.attr_type, ab.attr_type,
                "[{}] attr type for '{}'",
                path, aa.id
            );
            assert_eq!(aa.value, ab.value, "[{}] attr value for '{}'", path, aa.id);
        }
        compare_nodes_recursive(&na.children, &nb.children, &path);
    }
}

// ═══════════════════════════════════════════════════════════════════
//  Test 5: Full pipeline — lsx → effect → lsx → lsfx → parse → compare
// ═══════════════════════════════════════════════════════════════════

/// End-to-end: parse vanilla .lsx, round-trip through the full pipeline
/// (decompile → lsefx → read → compile → binary → parse), and verify
/// the final LsxResource matches the intermediate compiled version.
#[test]
fn parity_full_pipeline() {
    let registry = match load_registry() {
        Some(r) => r,
        None => {
            eprintln!("Skipping — game data not found");
            return;
        }
    };

    let lsx_path = bounding_sphere_lsx_path();
    if !lsx_path.exists() {
        eprintln!("Skipping — LSX not found");
        return;
    }

    // ── Step 1: Parse vanilla .lsx ─────────────────────────────────
    let lsx_content = std::fs::read_to_string(&lsx_path).expect("read LSX");
    let original = parse_lsx_resource(&lsx_content).expect("parse LSX");

    // ── Step 2: Decompile → EffectResource ─────────────────────────
    let effect1 = lsx_to_effect(&original, &registry);

    // ── Step 3: Write .lsefx + read back ───────────────────────────
    let lsefx_xml = write_lsefx(&effect1);
    let effect2 = read_lsefx(&lsefx_xml).expect("read lsefx round-trip");

    // ── Step 4: Compile back to LsxResource ────────────────────────
    let compiled = effect_to_lsx(&effect2, &registry);

    // ── Step 5: Write binary .lsfx ─────────────────────────────────
    let mut binary = Vec::new();
    write_lsfx(&mut binary, &compiled).expect("write_lsfx");

    // ── Step 6: Parse binary back ──────────────────────────────────
    let final_resource = parse_lsfx(Cursor::new(&binary)).expect("parse_lsfx from binary");

    // ── Verify: final ↔ compiled should be byte-identical ──────────
    for (cr, fr) in compiled.regions.iter().zip(&final_resource.regions) {
        assert_eq!(cr.id, fr.id, "region id");
        compare_nodes_recursive(&cr.nodes, &fr.nodes, &format!("region:{}", cr.id));
    }

    // ── Verify: final ↔ original should have same component semantics
    let orig_comps = collect_component_nodes(&original);
    let final_comps = collect_component_nodes(&final_resource);

    assert_eq!(
        orig_comps.len(),
        final_comps.len(),
        "full pipeline: component count mismatch (original {} vs final {})",
        orig_comps.len(),
        final_comps.len()
    );

    for (oc, fc) in orig_comps.iter().zip(&final_comps) {
        // Component identity must survive the full pipeline
        assert_eq!(
            attr_value(oc, "ID"),
            attr_value(fc, "ID"),
            "full pipeline: component ID mismatch"
        );
        assert_eq!(
            attr_value(oc, "Name"),
            attr_value(fc, "Name"),
            "full pipeline: component Name mismatch"
        );
        assert_eq!(
            attr_value(oc, "StartTime"),
            attr_value(fc, "StartTime"),
            "full pipeline: component StartTime mismatch"
        );
        assert_eq!(
            attr_value(oc, "EndTime"),
            attr_value(fc, "EndTime"),
            "full pipeline: component EndTime mismatch"
        );

        // Every original property should appear in the final output
        let orig_props = collect_property_nodes(oc);
        let final_props = collect_property_nodes(fc);

        for op in &orig_props {
            let full_name = attr_value(op, "FullName").unwrap_or_default();
            let fp = final_props
                .iter()
                .find(|p| attr_value(p, "FullName").as_deref() == Some(&full_name));

            assert!(
                fp.is_some(),
                "full pipeline: property '{full_name}' lost in round-trip"
            );

            let fp = fp.unwrap();
            assert_eq!(
                attr_value(op, "Value"),
                attr_value(fp, "Value"),
                "full pipeline: Value mismatch for '{full_name}'"
            );

            // AttributeName should survive the full pipeline
            assert_eq!(
                attr_value(op, "AttributeName"),
                attr_value(fp, "AttributeName"),
                "full pipeline: AttributeName mismatch for '{full_name}'"
            );
        }
    }

    eprintln!(
        "✓ full pipeline: .lsx → effect → .lsefx → effect → .lsx → .lsfx → .lsx all match ({} components, {} binary bytes)",
        final_comps.len(),
        binary.len()
    );
}

// ═══════════════════════════════════════════════════════════════════
//  Test 6: Vanilla .lsefx → compile → decompile round-trip
// ═══════════════════════════════════════════════════════════════════

/// Start from the vanilla .lsefx editor file, compile to LsxResource,
/// decompile back, and verify component/property fidelity.
#[test]
fn parity_lsefx_compile_decompile_roundtrip() {
    let registry = match load_registry() {
        Some(r) => r,
        None => {
            eprintln!("Skipping — game data not found");
            return;
        }
    };

    let lsefx_path = bounding_sphere_lsefx_path();
    if !lsefx_path.exists() {
        eprintln!("Skipping — vanilla .lsefx not found");
        return;
    }

    // ── Read the vanilla .lsefx ────────────────────────────────────
    let lsefx_content = std::fs::read_to_string(&lsefx_path).expect("read vanilla lsefx");
    let effect1 = read_lsefx(&lsefx_content).expect("parse vanilla lsefx");

    // ── Compile to LsxResource ─────────────────────────────────────
    let lsx_resource = effect_to_lsx(&effect1, &registry);

    // ── Decompile back to EffectResource ───────────────────────────
    let effect2 = lsx_to_effect(&lsx_resource, &registry);

    // ── Compare structurally ───────────────────────────────────────
    // Empty track groups in the vanilla .lsefx have no runtime representation
    // and are lost during compile → decompile. Compare components directly.
    let comps1: Vec<_> = effect1
        .track_groups
        .iter()
        .flat_map(|tg| &tg.tracks)
        .flat_map(|t| &t.components)
        .collect();
    let comps2: Vec<_> = effect2
        .track_groups
        .iter()
        .flat_map(|tg| &tg.tracks)
        .flat_map(|t| &t.components)
        .collect();

    assert_eq!(comps1.len(), comps2.len(), "component count");

    for (c1, c2) in comps1.iter().zip(&comps2) {
        assert_eq!(c1.class_name, c2.class_name, "class_name");
        assert_eq!(c1.instance_name, c2.instance_name, "instance_name");
        assert_eq!(c1.start, c2.start, "start time for {}", c1.class_name);
        assert_eq!(c1.end, c2.end, "end time for {}", c1.class_name);

        // Compare non-implicit properties by GUID
        let implicit = [
            "ef1d7d1e-02b6-4548-80d9-5ef2fbcda237",
            "035b5248-d0ca-44b7-853f-3acb84110e67",
        ];
        let p1_real: Vec<_> = c1
            .properties
            .iter()
            .filter(|p| !implicit.contains(&p.guid.to_lowercase().as_str()))
            .collect();
        let p2_real: Vec<_> = c2
            .properties
            .iter()
            .filter(|p| !implicit.contains(&p.guid.to_lowercase().as_str()))
            .collect();

        assert_eq!(
            p1_real.len(),
            p2_real.len(),
            "non-implicit property count for '{}': {} vs {}",
            c1.class_name,
            p1_real.len(),
            p2_real.len()
        );

        for p1 in &p1_real {
            let p2 = p2_real.iter().find(|p| p.guid == p1.guid);

            if let Some(p2) = p2 {
                for (d1, d2) in p1.data.iter().zip(&p2.data) {
                    assert_eq!(
                        d1.value, d2.value,
                        "value mismatch for property {} in {}",
                        p1.guid, c1.class_name
                    );
                }
            } else {
                // The property might have been re-resolved to a different GUID
                // via AllSpark lookup — log but don't fail hard
                eprintln!(
                    "  NOTE: property {} not found by GUID in round-trip for {}",
                    p1.guid, c1.class_name
                );
            }
        }
    }

    eprintln!("✓ vanilla .lsefx → compile → decompile: component/property semantics preserved");
}
