//! Parser for AllSpark definition files (ComponentDefinition.xcd / ModuleDefinition.xmd).
//!
//! These files ship with the BG3 Toolkit and provide the authoritative GUID → name
//! mapping for every property and module type used in `.lsefx` effect files.
//!
//! # Coverage
//!
//! The XCD defines 23 component types with 442 property definitions (434 unique GUIDs).
//! The XMD defines 51 module types. Together they provide 100% coverage of all property
//! GUIDs observed in sample `.lsefx` files (299 XCD + 24 XMD = 323/323).
//!
//! Note: the XMD uses **uppercase** GUIDs, so all lookups are case-insensitive.

use quick_xml::events::Event;
use quick_xml::reader::Reader;
use std::collections::HashMap;
use std::path::Path;

/// Convert a quick-xml tag name to an owned String.
fn tag_name(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw).to_string()
}

/// Metadata for a single property definition from the XCD.
#[derive(Debug, Clone)]
pub struct PropertyDef {
    pub name: String,
    pub guid: String, // lowercase
    pub type_name: String,
    pub specializable: bool,
    pub tooltip: String,
    pub default_value: String,
}

/// A property group definition from the XCD.
#[derive(Debug, Clone)]
pub struct PropertyGroupDef {
    pub guid: String, // lowercase
    pub name: String,
    pub collapsed: String,
    /// Ordered list of property GUIDs in this group (lowercase)
    pub property_refs: Vec<String>,
}

/// Metadata for a single component type from the XCD.
#[derive(Debug, Clone)]
pub struct ComponentDef {
    pub name: String,
    pub tooltip: String,
    pub color: String,
    /// guid (lowercase) → PropertyDef
    pub properties: HashMap<String, PropertyDef>,
    /// Property groups for this component (ordered)
    pub property_groups: Vec<PropertyGroupDef>,
}

/// Metadata for a single module type from the XMD.
#[derive(Debug, Clone)]
pub struct ModuleDef {
    pub name: String,
    pub guid: String, // lowercase
    /// guid (lowercase) → property name
    pub properties: HashMap<String, String>,
}

/// Holds the merged property registry from XCD + XMD files.
///
/// Provides bidirectional GUID ↔ name lookups for properties and modules.
#[derive(Debug, Clone, Default)]
pub struct AllSparkRegistry {
    /// component_name → ComponentDef
    pub components: HashMap<String, ComponentDef>,
    /// guid (lowercase) → property name (global across all components)
    pub guid_to_name: HashMap<String, String>,
    /// component_name → { property_name → guid }
    pub name_to_guid: HashMap<String, HashMap<String, String>>,
    /// module_name → ModuleDef
    pub modules: HashMap<String, ModuleDef>,
    /// guid (lowercase) → module name
    pub module_guid_to_name: HashMap<String, String>,
    /// module name → guid (lowercase)
    pub module_name_to_guid: HashMap<String, String>,
}

impl AllSparkRegistry {
    /// Load both definition files.
    pub fn load(xcd_path: &Path, xmd_path: &Path) -> Result<Self, String> {
        let mut registry = Self::default();
        registry.load_xcd(xcd_path)?;
        registry.load_xmd(xmd_path)?;
        Ok(registry)
    }

    /// Parse a ComponentDefinition.xcd file.
    pub fn load_xcd(&mut self, path: &Path) -> Result<(), String> {
        let meta = std::fs::metadata(path)
            .map_err(|e| format!("Cannot stat XCD file {}: {}", path.display(), e))?;
        if meta.len() > 50 * 1024 * 1024 {
            return Err(format!(
                "XCD file {} exceeds 50 MB size limit",
                path.display()
            ));
        }
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read XCD file {}: {}", path.display(), e))?;
        self.parse_xcd(&content)
    }

    /// Parse a ModuleDefinition.xmd file.
    pub fn load_xmd(&mut self, path: &Path) -> Result<(), String> {
        let meta = std::fs::metadata(path)
            .map_err(|e| format!("Cannot stat XMD file {}: {}", path.display(), e))?;
        if meta.len() > 50 * 1024 * 1024 {
            return Err(format!(
                "XMD file {} exceeds 50 MB size limit",
                path.display()
            ));
        }
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read XMD file {}: {}", path.display(), e))?;
        self.parse_xmd(&content)
    }

    /// Parse XCD content from a string.
    pub fn parse_xcd(&mut self, content: &str) -> Result<(), String> {
        let mut reader = Reader::from_str(content);
        let mut buf = Vec::new();

        // State tracking for nested parsing
        let mut current_component: Option<String> = None;
        let mut current_prop_name = String::new();
        let mut current_prop_guid = String::new();
        let mut in_property_with_def = false;

        // Definition-level state
        let mut def_type_name = String::new();
        let mut def_specializable = false;
        let mut def_tooltip = String::new();
        let mut def_default_value = String::new();
        let mut in_definition = false;
        let mut in_data = false;
        let mut data_depth: u32 = 0; // tracks nesting inside <data>
        const MAX_DATA_DEPTH: u32 = 2000;

        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Eof) => break,
                Ok(Event::Start(ref e)) => {
                    let tag = tag_name(e.name().as_ref());
                    match tag.as_str() {
                        "component" => {
                            let mut name = String::new();
                            let mut tooltip = String::new();
                            let mut color = String::new();
                            for attr in e.attributes().flatten() {
                                match attr.key.as_ref() {
                                    b"name" => name = attr_value(&attr),
                                    b"tooltip" => tooltip = attr_value(&attr),
                                    b"color" => color = attr_value(&attr),
                                    _ => {}
                                }
                            }
                            let comp_def = ComponentDef {
                                name: name.clone(),
                                tooltip,
                                color,
                                properties: HashMap::new(),
                                property_groups: Vec::new(),
                            };
                            self.components.insert(name.clone(), comp_def);
                            self.name_to_guid.entry(name.clone()).or_default();
                            current_component = Some(name);
                        }
                        "propertygroup" if current_component.is_some() => {
                            let pg = parse_propertygroup(e, &mut reader)?;
                            if let Some(ref comp_name) = current_component {
                                if let Some(comp) = self.components.get_mut(comp_name) {
                                    comp.property_groups.push(pg);
                                }
                            }
                        }
                        "property" if current_component.is_some() => {
                            let mut name = String::new();
                            let mut guid = String::new();
                            for attr in e.attributes().flatten() {
                                match attr.key.as_ref() {
                                    b"name" => name = attr_value(&attr),
                                    b"id" => guid = attr_value(&attr).to_lowercase(),
                                    _ => {}
                                }
                            }
                            if !guid.is_empty() && !name.is_empty() {
                                current_prop_name = name;
                                current_prop_guid = guid;
                                in_property_with_def = true;
                                // Reset definition state
                                def_type_name.clear();
                                def_specializable = false;
                                def_tooltip.clear();
                                def_default_value.clear();
                                in_definition = false;
                                in_data = false;
                                data_depth = 0;
                            }
                        }
                        "definition" if in_property_with_def => {
                            for attr in e.attributes().flatten() {
                                match attr.key.as_ref() {
                                    b"type" => def_type_name = attr_value(&attr),
                                    b"tooltip" => def_tooltip = attr_value(&attr),
                                    b"specializable" => {
                                        def_specializable =
                                            attr_value(&attr).eq_ignore_ascii_case("true");
                                    }
                                    _ => {}
                                }
                            }
                            in_definition = true;
                        }
                        "data" if in_definition => {
                            in_data = true;
                            data_depth += 1;
                            if data_depth > MAX_DATA_DEPTH {
                                return Err(format!(
                                    "XCD data nesting depth exceeds {MAX_DATA_DEPTH} limit"
                                ));
                            }
                        }
                        "datum" if in_data && data_depth == 1 => {
                            // First datum child of the <data> — grab default value
                            for attr in e.attributes().flatten() {
                                if attr.key.as_ref() == b"value" {
                                    def_default_value = attr_value(&attr);
                                }
                            }
                        }
                        _ => {
                            if in_data {
                                data_depth += 1;
                                if data_depth > MAX_DATA_DEPTH {
                                    return Err(format!(
                                        "XCD data nesting depth exceeds {MAX_DATA_DEPTH} limit"
                                    ));
                                }
                            }
                        }
                    }
                }
                Ok(Event::Empty(ref e)) => {
                    let tag = tag_name(e.name().as_ref());
                    match tag.as_str() {
                        "property" if current_component.is_some() => {
                            // Self-closing <property id="..."/> — propertygroup reference, skip
                        }
                        "datum" if in_data && data_depth == 1 => {
                            for attr in e.attributes().flatten() {
                                if attr.key.as_ref() == b"value" {
                                    def_default_value = attr_value(&attr);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Ok(Event::End(ref e)) => {
                    let tag = tag_name(e.name().as_ref());
                    match tag.as_str() {
                        "component" => {
                            current_component = None;
                        }
                        "property" if in_property_with_def => {
                            // Flush the property definition
                            if let Some(ref comp_name) = current_component {
                                let prop_def = PropertyDef {
                                    name: current_prop_name.clone(),
                                    guid: current_prop_guid.clone(),
                                    type_name: def_type_name.clone(),
                                    specializable: def_specializable,
                                    tooltip: def_tooltip.clone(),
                                    default_value: def_default_value.clone(),
                                };
                                if let Some(comp) = self.components.get_mut(comp_name) {
                                    comp.properties.insert(current_prop_guid.clone(), prop_def);
                                }
                                self.guid_to_name
                                    .insert(current_prop_guid.clone(), current_prop_name.clone());
                                if let Some(map) = self.name_to_guid.get_mut(comp_name) {
                                    map.insert(
                                        current_prop_name.clone(),
                                        current_prop_guid.clone(),
                                    );
                                }
                            }
                            in_property_with_def = false;
                        }
                        "definition" => {
                            in_definition = false;
                        }
                        "data" if in_data => {
                            data_depth = data_depth.saturating_sub(1);
                            if data_depth == 0 {
                                in_data = false;
                            }
                        }
                        _ => {
                            if in_data && data_depth > 0 {
                                data_depth = data_depth.saturating_sub(1);
                            }
                        }
                    }
                }
                Err(e) => return Err(format!("XCD parse error: {e}")),
                _ => {}
            }
            buf.clear();
        }

        Ok(())
    }

    /// Parse XMD content from a string.
    pub fn parse_xmd(&mut self, content: &str) -> Result<(), String> {
        let mut reader = Reader::from_str(content);
        let mut buf = Vec::new();

        let mut current_module_name = String::new();
        let mut current_module_guid = String::new();
        let mut in_module = false;
        let mut module_props: HashMap<String, String> = HashMap::new();

        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Eof) => break,
                Ok(Event::Start(ref e)) => {
                    let tag = tag_name(e.name().as_ref());
                    if tag.as_str() == "module" {
                        current_module_name.clear();
                        current_module_guid.clear();
                        module_props.clear();
                        for attr in e.attributes().flatten() {
                            match attr.key.as_ref() {
                                b"name" => current_module_name = attr_value(&attr),
                                b"id" => {
                                    current_module_guid = attr_value(&attr).to_lowercase();
                                }
                                _ => {}
                            }
                        }
                        in_module = true;
                    }
                }
                Ok(Event::Empty(ref e)) => {
                    let tag = tag_name(e.name().as_ref());
                    if tag == "property" && in_module {
                        let mut name = String::new();
                        let mut guid = String::new();
                        for attr in e.attributes().flatten() {
                            match attr.key.as_ref() {
                                b"name" => name = attr_value(&attr),
                                b"id" => guid = attr_value(&attr).to_lowercase(),
                                _ => {}
                            }
                        }
                        if !guid.is_empty() {
                            module_props.insert(guid.clone(), name.clone());
                            self.guid_to_name.insert(guid, name);
                        }
                    }
                }
                Ok(Event::End(ref e)) => {
                    let tag = tag_name(e.name().as_ref());
                    if tag == "module" && in_module {
                        if !current_module_guid.is_empty() {
                            let mod_def = ModuleDef {
                                name: current_module_name.clone(),
                                guid: current_module_guid.clone(),
                                properties: module_props.clone(),
                            };
                            self.modules.insert(current_module_name.clone(), mod_def);
                            self.module_guid_to_name
                                .insert(current_module_guid.clone(), current_module_name.clone());
                            self.module_name_to_guid
                                .insert(current_module_name.clone(), current_module_guid.clone());
                        }
                        in_module = false;
                    }
                }
                Err(e) => return Err(format!("XMD parse error: {e}")),
                _ => {}
            }
            buf.clear();
        }

        Ok(())
    }

    /// Look up a property GUID (case-insensitive) and return its name.
    pub fn resolve_property_guid(&self, guid: &str) -> Option<&str> {
        self.guid_to_name
            .get(&guid.to_lowercase())
            .map(|s| s.as_str())
    }

    /// Look up a property name within a component class and return its GUID.
    ///
    /// Falls back to a global search across all components if the name
    /// is not found in the specified component.
    pub fn resolve_property_name(&self, component_class: &str, name: &str) -> Option<&str> {
        if let Some(comp_map) = self.name_to_guid.get(component_class) {
            if let Some(guid) = comp_map.get(name) {
                return Some(guid.as_str());
            }
        }
        // Fallback: global search (module properties are not component-scoped)
        for map in self.name_to_guid.values() {
            if let Some(guid) = map.get(name) {
                return Some(guid.as_str());
            }
        }
        None
    }

    /// Look up a module GUID and return its name.
    pub fn resolve_module_guid(&self, guid: &str) -> Option<&str> {
        self.module_guid_to_name
            .get(&guid.to_lowercase())
            .map(|s| s.as_str())
    }

    /// Look up a module name and return its GUID.
    pub fn resolve_module_name(&self, name: &str) -> Option<&str> {
        self.module_name_to_guid.get(name).map(|s| s.as_str())
    }
}

/// Extract a quick-xml attribute value as an owned String.
fn attr_value(attr: &quick_xml::events::attributes::Attribute) -> String {
    String::from_utf8_lossy(&attr.value).to_string()
}

/// Parse a `<propertygroup>` element from the XCD, collecting property ID refs.
fn parse_propertygroup(
    start: &quick_xml::events::BytesStart,
    reader: &mut Reader<&[u8]>,
) -> Result<PropertyGroupDef, String> {
    let mut pg = PropertyGroupDef {
        guid: String::new(),
        name: String::new(),
        collapsed: "False".to_string(),
        property_refs: Vec::new(),
    };
    for attr in start.attributes().flatten() {
        match attr.key.as_ref() {
            b"id" => pg.guid = attr_value(&attr).to_lowercase(),
            b"name" => pg.name = attr_value(&attr),
            b"collapsed" => pg.collapsed = attr_value(&attr),
            _ => {}
        }
    }

    let mut buf = Vec::new();
    let mut depth: u32 = 1;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref _e)) => {
                depth += 1;
            }
            Ok(Event::Empty(ref e)) => {
                let t = tag_name(e.name().as_ref());
                if t == "property" {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"id" {
                            pg.property_refs.push(attr_value(&attr).to_lowercase());
                        }
                    }
                }
            }
            Ok(Event::End(_)) => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Ok(Event::Eof) => return Err("Unexpected EOF in <propertygroup>".into()),
            Err(e) => return Err(format!("XCD parse error in propertygroup: {e}")),
            _ => {}
        }
        buf.clear();
    }
    Ok(pg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_xcd_bounding_sphere() {
        let xcd = r#"
<root version="0.0">
  <components>
    <component name="BoundingSphere" tooltip="Bounding sphere" color="-16711681">
      <properties>
        <property name="Center" id="c1115291-39d1-43a2-8259-31c2ef4dbd93">
          <definition typeid="321d4c50" type="Vector3" specializable="True" tooltip="Center of sphere">
            <data><datum platform="00000000-0000-0000-0000-000000000000" value="0,0,0"/></data>
          </definition>
        </property>
        <property name="Radius" id="ba2ee0f9-d369-4d36-bd63-30d4c8c46a0a">
          <definition typeid="1a0cc0c6" type="FloatSlider" specializable="True" tooltip="Radius">
            <data><datum platform="00000000-0000-0000-0000-000000000000" value="1"/></data>
          </definition>
        </property>
        <propertygroup id="88322fa2" name="Property Group" collapsed="False">
          <properties>
            <property id="c1115291-39d1-43a2-8259-31c2ef4dbd93"/>
            <property id="ba2ee0f9-d369-4d36-bd63-30d4c8c46a0a"/>
          </properties>
        </propertygroup>
      </properties>
    </component>
  </components>
</root>"#;

        let mut reg = AllSparkRegistry::default();
        reg.parse_xcd(xcd).unwrap();

        assert_eq!(reg.components.len(), 1);
        assert!(reg.components.contains_key("BoundingSphere"));

        let comp = &reg.components["BoundingSphere"];
        assert_eq!(comp.properties.len(), 2);

        // Check Center property
        let center = &comp.properties["c1115291-39d1-43a2-8259-31c2ef4dbd93"];
        assert_eq!(center.name, "Center");
        assert_eq!(center.type_name, "Vector3");
        assert!(center.specializable);
        assert_eq!(center.default_value, "0,0,0");

        // Check Radius property
        let radius = &comp.properties["ba2ee0f9-d369-4d36-bd63-30d4c8c46a0a"];
        assert_eq!(radius.name, "Radius");
        assert_eq!(radius.type_name, "FloatSlider");

        // Check lookups
        assert_eq!(
            reg.resolve_property_guid("c1115291-39d1-43a2-8259-31c2ef4dbd93"),
            Some("Center")
        );
        assert_eq!(
            reg.resolve_property_guid("C1115291-39D1-43A2-8259-31C2EF4DBD93"),
            Some("Center")
        );
        assert_eq!(
            reg.resolve_property_name("BoundingSphere", "Radius"),
            Some("ba2ee0f9-d369-4d36-bd63-30d4c8c46a0a")
        );
    }

    #[test]
    fn parse_xmd_modules() {
        let xmd = r#"
<root version="0.0">
  <modules>
    <module name="Alpha By Effect Distance" id="3CC91D05-19EB-4472-A94B-E622BF1574B4" required="False">
      <properties>
        <property name="Min Distance" id="758F33A5-C5A2-4149-BC97-1E888D58C564" component="ParticleSystem"/>
        <property name="Max Distance" id="92BBDF71-31D0-476B-9646-EA5346038B23" component="ParticleSystem"/>
      </properties>
    </module>
  </modules>
</root>"#;

        let mut reg = AllSparkRegistry::default();
        reg.parse_xmd(xmd).unwrap();

        assert_eq!(reg.modules.len(), 1);
        assert!(reg.modules.contains_key("Alpha By Effect Distance"));

        let m = &reg.modules["Alpha By Effect Distance"];
        assert_eq!(m.guid, "3cc91d05-19eb-4472-a94b-e622bf1574b4");
        assert_eq!(m.properties.len(), 2);

        // Case-insensitive GUID lookup
        assert_eq!(
            reg.resolve_module_guid("3CC91D05-19EB-4472-A94B-E622BF1574B4"),
            Some("Alpha By Effect Distance")
        );
        assert_eq!(
            reg.resolve_module_name("Alpha By Effect Distance"),
            Some("3cc91d05-19eb-4472-a94b-e622bf1574b4")
        );

        // Module properties are in the global GUID→name map
        assert_eq!(
            reg.resolve_property_guid("758F33A5-C5A2-4149-BC97-1E888D58C564"),
            Some("Min Distance")
        );
    }

    // ── Unhappy-path tests ─────────────────────────────────────────

    #[test]
    fn parse_xcd_empty_string() {
        let mut reg = AllSparkRegistry::default();
        // Empty string is valid XML (just EOF) — should parse without error
        let result = reg.parse_xcd("");
        assert!(result.is_ok());
        assert!(reg.components.is_empty());
    }

    #[test]
    fn parse_xcd_malformed_xml() {
        let mut reg = AllSparkRegistry::default();
        // quick_xml errors on mismatched end tags
        let result = reg.parse_xcd("<root></wrong>");
        assert!(result.is_err(), "malformed XML should error");
        assert!(result.unwrap_err().contains("XCD parse error"));
    }

    #[test]
    fn parse_xmd_empty_string() {
        let mut reg = AllSparkRegistry::default();
        let result = reg.parse_xmd("");
        assert!(result.is_ok());
        assert!(reg.modules.is_empty());
    }

    #[test]
    fn parse_xmd_malformed_xml() {
        let mut reg = AllSparkRegistry::default();
        // quick_xml errors on mismatched end tags
        let result = reg.parse_xmd("<root></wrong>");
        assert!(result.is_err(), "malformed XML should error");
        assert!(result.unwrap_err().contains("XMD parse error"));
    }

    #[test]
    fn resolve_nonexistent_property_guid() {
        let reg = AllSparkRegistry::default();
        assert_eq!(reg.resolve_property_guid("nonexistent-guid"), None);
    }

    #[test]
    fn resolve_nonexistent_property_name() {
        let reg = AllSparkRegistry::default();
        assert_eq!(
            reg.resolve_property_name("NoSuchComponent", "NoSuchProp"),
            None
        );
    }

    #[test]
    fn resolve_nonexistent_module_guid() {
        let reg = AllSparkRegistry::default();
        assert_eq!(reg.resolve_module_guid("nonexistent-guid"), None);
    }

    #[test]
    fn resolve_nonexistent_module_name() {
        let reg = AllSparkRegistry::default();
        assert_eq!(reg.resolve_module_name("NoSuchModule"), None);
    }

    #[test]
    fn xcd_component_with_no_properties() {
        let xcd = r#"
<root version="0.0">
  <components>
    <component name="EmptyComp" tooltip="" color="">
      <properties/>
    </component>
  </components>
</root>"#;
        let mut reg = AllSparkRegistry::default();
        reg.parse_xcd(xcd).unwrap();
        assert!(reg.components.contains_key("EmptyComp"));
        assert!(reg.components["EmptyComp"].properties.is_empty());
    }

    #[test]
    fn xcd_property_missing_guid_is_skipped() {
        let xcd = r#"
<root version="0.0">
  <components>
    <component name="Comp" tooltip="" color="">
      <properties>
        <property name="NoGuid">
          <definition type="Boolean" specializable="False" tooltip="">
            <data><datum value="0"/></data>
          </definition>
        </property>
      </properties>
    </component>
  </components>
</root>"#;
        let mut reg = AllSparkRegistry::default();
        reg.parse_xcd(xcd).unwrap();
        // Property without id= should be skipped (no guid)
        assert!(reg.components["Comp"].properties.is_empty());
    }

    #[test]
    fn xmd_module_missing_guid_is_skipped() {
        let xmd = r#"
<root version="0.0">
  <modules>
    <module name="NoGuidModule">
      <properties/>
    </module>
  </modules>
</root>"#;
        let mut reg = AllSparkRegistry::default();
        reg.parse_xmd(xmd).unwrap();
        // Module without id= should not be registered
        assert!(reg.modules.is_empty());
    }

    #[test]
    fn guid_case_insensitive_lookup() {
        let xmd = r#"
<root version="0.0">
  <modules>
    <module name="TestMod" id="AABBCCDD-1122-3344-5566-778899AABBCC">
      <properties>
        <property name="Prop1" id="11223344-AABB-CCDD-EEFF-001122334455"/>
      </properties>
    </module>
  </modules>
</root>"#;
        let mut reg = AllSparkRegistry::default();
        reg.parse_xmd(xmd).unwrap();

        // Lookups should work with any case
        assert_eq!(
            reg.resolve_module_guid("aabbccdd-1122-3344-5566-778899aabbcc"),
            Some("TestMod")
        );
        assert_eq!(
            reg.resolve_module_guid("AABBCCDD-1122-3344-5566-778899AABBCC"),
            Some("TestMod")
        );
        assert_eq!(
            reg.resolve_property_guid("11223344-aabb-ccdd-eeff-001122334455"),
            Some("Prop1")
        );
        assert_eq!(
            reg.resolve_property_guid("11223344-AABB-CCDD-EEFF-001122334455"),
            Some("Prop1")
        );
    }
}
