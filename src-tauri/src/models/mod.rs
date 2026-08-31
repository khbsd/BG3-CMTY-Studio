pub mod effect;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
#[cfg(test)]
use ts_rs::TS;

/// Effect-related file kinds that the tool may encounter across runtime and toolkit workflows.
///
/// BG3 runtime content uses `.lsfx` binaries, and users may also work with
/// toolkit-authored `.lsefx` source files that compile into `.lsfx`. During
/// inspection workflows we may also encounter converted `.lsfx.lsx` XML output.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum EffectFileKind {
    /// Runtime binary consumed by the game.
    RuntimeLsfxBinary,
    /// XML converted from a runtime `.lsfx` file for inspection/parity workflows.
    RuntimeLsfxConvertedLsx,
    /// Toolkit source that must be compiled into `.lsfx` before runtime use.
    ToolkitLsefxSource,
}

impl EffectFileKind {
    pub fn from_path(path: &Path) -> Option<Self> {
        let file_name = path.file_name()?.to_string_lossy().to_ascii_lowercase();

        if file_name.ends_with(".lsfx.lsx") {
            Some(Self::RuntimeLsfxConvertedLsx)
        } else if file_name.ends_with(".lsfx") {
            Some(Self::RuntimeLsfxBinary)
        } else if file_name.ends_with(".lsefx") {
            Some(Self::ToolkitLsefxSource)
        } else {
            None
        }
    }

    pub fn requires_toolkit_compile_step(self) -> bool {
        matches!(self, Self::ToolkitLsefxSource)
    }

    pub fn is_runtime_artifact(self) -> bool {
        matches!(
            self,
            Self::RuntimeLsfxBinary | Self::RuntimeLsfxConvertedLsx
        )
    }
}

/// A full LSX resource preserving regions, nested nodes, and raw typed attributes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LsxResource {
    pub regions: Vec<LsxRegion>,
}

/// A single LSX region from the `<region id="...">` layer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LsxRegion {
    pub id: String,
    pub nodes: Vec<LsxNode>,
}

/// A recursive LSX node preserving attributes and children.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LsxNode {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_attribute: Option<String>,
    pub attributes: Vec<LsxNodeAttribute>,
    pub children: Vec<LsxNode>,
    #[serde(default)]
    pub commented: bool,
}

/// A named LSX attribute with its raw type and string value.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LsxNodeAttribute {
    pub id: String,
    pub attr_type: String,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u16>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arguments: Vec<LsxTranslatedFsArgument>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LsxTranslatedFsArgument {
    pub key: String,
    pub string: Box<LsxNodeAttribute>,
    pub value: String,
}

/// Represents a single entry parsed from an LSX file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export))]
pub struct LsxEntry {
    pub uuid: String,
    pub node_id: String,
    pub attributes: HashMap<String, LsxAttribute>,
    pub children: Vec<LsxChildGroup>,
    /// True if this entry was parsed from an XML comment (<!-- ... -->).
    #[serde(default)]
    pub commented: bool,
    /// The LSX region ID this entry belongs to (e.g. "AnimationBank", "GameplayVFXs").
    #[serde(default)]
    pub region_id: String,
}

/// A single attribute on an LSX node.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export))]
pub struct LsxAttribute {
    pub attr_type: String,
    pub value: String,
}

/// A group of child nodes sharing a parent element id (e.g. "SubClasses", "EyeColors").
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export))]
pub struct LsxChildGroup {
    pub group_id: String,
    pub entries: Vec<LsxChildEntry>,
}

/// A single child entry within a child group.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export))]
pub struct LsxChildEntry {
    pub node_id: String,
    pub object_guid: String,
}

/// A dependency entry from meta.lsx Dependencies section.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export))]
pub struct MetaDependency {
    pub uuid: String,
    pub name: String,
    pub folder: String,
    pub md5: String,
    pub version64: String,
}

/// Mod metadata extracted from meta.lsx.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export))]
pub struct ModMeta {
    pub uuid: String,
    pub folder: String,
    pub name: String,
    pub author: String,
    pub version: String,
    pub version64: String,
    pub description: String,
    pub md5: String,
    pub mod_type: String,
    pub tags: String,
    pub num_players: String,
    pub gm_template: String,
    pub char_creation_level: String,
    pub lobby_level: String,
    pub menu_level: String,
    pub startup_level: String,
    pub photo_booth: String,
    pub main_menu_bg_video: String,
    pub publish_version: String,
    pub target_mode: String,
    pub dependencies: Vec<MetaDependency>,
}

/// A single entry parsed from a Stats .txt file (Spell_*.txt, Status_*.txt).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export))]
pub struct StatsEntry {
    pub name: String,
    pub entry_type: String,
    pub parent: Option<String>,
    pub data: HashMap<String, String>,
}

/// Enum of all section / file-category types.
///
/// The original 11 CF-eligible sections come first (stable ordinals for msgpack cache).
/// Extended sections for broader LSX authoring follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export))]
pub enum Section {
    // ── CF-eligible sections (original 11) ──
    Races,
    Progressions,
    Lists,
    Feats,
    Origins,
    Backgrounds,
    BackgroundGoals,
    ActionResources,
    ActionResourceGroups,
    ClassDescriptions,
    Spells,
    // ── Extended sections (LSX authoring only) ──
    Gods,
    Tags,
    Visuals,
    CharacterCreation,
    CharacterCreationPresets,
    ColorDefinitions,
    FeatDescriptions,
    // ── Additional LSX data sections ──
    Animation,
    AnimationOverrides,
    Calendar,
    CinematicArenaFrequencyGroups,
    CombatCameraGroups,
    Content,
    CustomDice,
    DefaultValues,
    DifficultyClasses,
    Disturbances,
    Encumbrance,
    EquipmentTypes,
    Factions,
    Flags,
    FixedHotBarSlots,
    TextureAtlasInfo,
    IconUVList,
    ItemThrowParams,
    Levelmaps,
    LimbsMapping,
    /// Not a browsable section — used internally for mod meta.lsx files.
    Meta,
    MultiEffectInfos,
    ProjectileDefaults,
    RandomCasts,
    RootTemplates,
    Ruleset,
    Shapeshift,
    Sound,
    SpellMetadata,
    StatusMetadata,
    Surface,
    TooltipExtras,
    TrajectoryRules,
    Tutorials,
    VFX,
    Voices,
    WeaponAnimationSetData,
    ErrorDescriptions,
    // ── New data sections ──
    ProgressionDescriptions,
    CompanionPresets,
    SpellLists,
    SkillLists,
    PassiveLists,
    EquipmentLists,
    AbilityLists,
    ExperienceRewards,
    GoldValues,
    DeathEffects,
    SpeakerGroups,
    Gossips,
}

impl Section {
    /// Whether this section is eligible for CF config output (the original 11 sections).
    pub fn is_cf_eligible(&self) -> bool {
        matches!(
            self,
            Section::Races
                | Section::Progressions
                | Section::Lists
                | Section::Feats
                | Section::Origins
                | Section::Backgrounds
                | Section::BackgroundGoals
                | Section::ActionResources
                | Section::ActionResourceGroups
                | Section::ClassDescriptions
                | Section::Spells
        )
    }

    /// Returns the expected LSX node_id(s) for a section, if the section should
    /// filter entries by node_id.  Returns `None` if no filter is needed.
    pub fn expected_node_ids(&self) -> Option<&'static [&'static str]> {
        match self {
            Section::Races => Some(&["Race"]),
            Section::Progressions => Some(&["Progression"]),
            Section::Feats => Some(&["Feat"]),
            Section::Origins => Some(&["Origin"]),
            Section::Backgrounds => Some(&["Background"]),
            Section::BackgroundGoals => Some(&["BackgroundGoal"]),
            Section::Tags => Some(&["Tag"]),
            Section::Gods => Some(&["God"]),
            _ => None,
        }
    }

    pub fn region_id(&self) -> &'static str {
        match self {
            Section::Races => "Races",
            Section::Progressions => "Progressions",
            Section::Lists => "Lists",
            Section::Feats => "Feats",
            Section::Origins => "Origins",
            Section::Backgrounds => "Backgrounds",
            Section::BackgroundGoals => "BackgroundGoals",
            Section::ActionResources => "ActionResourceDefinitions",
            Section::ActionResourceGroups => "ActionResourceGroupDefinitions",
            Section::ClassDescriptions => "ClassDescriptions",
            Section::Spells => "Spells",
            Section::Gods => "Gods",
            Section::Tags => "Tags",
            Section::Visuals => "Visuals",
            Section::CharacterCreation => "CharacterCreation",
            Section::CharacterCreationPresets => "CharacterCreationPresets",
            Section::ColorDefinitions => "ColorDefinitions",
            Section::FeatDescriptions => "FeatDescriptions",
            Section::Animation => "Animation",
            Section::AnimationOverrides => "AnimationOverrides",
            Section::Calendar => "Calendar",
            Section::CinematicArenaFrequencyGroups => "CinematicArenaFrequencyGroups",
            Section::CombatCameraGroups => "CombatCameraGroups",
            Section::Content => "Content",
            Section::CustomDice => "CustomDice",
            Section::DefaultValues => "DefaultValues",
            Section::DifficultyClasses => "DifficultyClasses",
            Section::Disturbances => "DisturbanceProperties",
            Section::Encumbrance => "WeightCategories",
            Section::EquipmentTypes => "EquipmentTypes",
            Section::Factions => "FactionContainer",
            Section::Flags => "Flags",
            Section::FixedHotBarSlots => "FixedHotBarSlots",
            Section::TextureAtlasInfo => "TextureAtlasInfo",
            Section::IconUVList => "IconUVList",
            Section::ItemThrowParams => "ItemThrowParams",
            Section::Levelmaps => "LevelMapValues",
            Section::LimbsMapping => "LimbsMapping",
            Section::Meta => "Config",
            Section::MultiEffectInfos => "MultiEffectInfos",
            Section::ProjectileDefaults => "ProjectileDefaults",
            Section::RandomCasts => "RandomCasts",
            Section::RootTemplates => "Templates",
            Section::Ruleset => "Rulesets",
            Section::Shapeshift => "Shapeshift",
            Section::Sound => "Sound",
            Section::SpellMetadata => "Spell",
            Section::StatusMetadata => "Status",
            Section::Surface => "Surface",
            Section::TooltipExtras => "TooltipExtraTexts",
            Section::TrajectoryRules => "TrajectoryRules",
            Section::Tutorials => "Tutorials",
            Section::VFX => "VFX",
            Section::Voices => "Voices",
            Section::WeaponAnimationSetData => "WeaponAnimationSetData",
            Section::ErrorDescriptions => "ConditionErrors",
            Section::ProgressionDescriptions => "ProgressionDescriptions",
            Section::CompanionPresets => "CompanionPresets",
            Section::SpellLists => "SpellLists",
            Section::SkillLists => "SkillLists",
            Section::PassiveLists => "PassiveLists",
            Section::EquipmentLists => "EquipmentLists",
            Section::AbilityLists => "AbilityLists",
            Section::ExperienceRewards => "ExperienceRewards",
            Section::GoldValues => "GoldValues",
            Section::DeathEffects => "DeathEffects",
            Section::SpeakerGroups => "SpeakerGroups",
            Section::Gossips => "Gossips",
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            Section::Races => "Races",
            Section::Progressions => "Progressions",
            Section::Lists => "Lists",
            Section::Feats => "Feats",
            Section::Origins => "Origins",
            Section::Backgrounds => "Backgrounds",
            Section::BackgroundGoals => "Background Goals",
            Section::ActionResources => "Action Resources",
            Section::ActionResourceGroups => "Action Resource Groups",
            Section::ClassDescriptions => "Class Descriptions",
            Section::Spells => "Spells",
            Section::Gods => "Gods",
            Section::Tags => "Tags",
            Section::Visuals => "Visuals",
            Section::CharacterCreation => "Character Creation",
            Section::CharacterCreationPresets => "Character Creation Presets",
            Section::ColorDefinitions => "Color Definitions",
            Section::FeatDescriptions => "Feat Descriptions",
            Section::Animation => "Animation",
            Section::AnimationOverrides => "Animation Overrides",
            Section::Calendar => "Calendar",
            Section::CinematicArenaFrequencyGroups => "Cinematic Arena Groups",
            Section::CombatCameraGroups => "Combat Camera Groups",
            Section::Content => "Content",
            Section::CustomDice => "Custom Dice",
            Section::DefaultValues => "Default Values",
            Section::DifficultyClasses => "Difficulty Classes",
            Section::Disturbances => "Disturbances",
            Section::Encumbrance => "Encumbrance",
            Section::EquipmentTypes => "Equipment Types",
            Section::Factions => "Factions",
            Section::Flags => "Flags",
            Section::FixedHotBarSlots => "Fixed Hotbar Slots",
            Section::TextureAtlasInfo => "Texture Atlas Info",
            Section::IconUVList => "Icon UV List",
            Section::ItemThrowParams => "Item Throw Params",
            Section::Levelmaps => "Level Maps",
            Section::LimbsMapping => "Limbs Mapping",
            Section::Meta => "Meta",
            Section::MultiEffectInfos => "Multi-Effect Infos",
            Section::ProjectileDefaults => "Projectile Defaults",
            Section::RandomCasts => "Random Casts",
            Section::RootTemplates => "Root Templates",
            Section::Ruleset => "Ruleset",
            Section::Shapeshift => "Shapeshift",
            Section::Sound => "Sound",
            Section::SpellMetadata => "Spell Metadata",
            Section::StatusMetadata => "Status Metadata",
            Section::Surface => "Surface",
            Section::TooltipExtras => "Tooltip Extras",
            Section::TrajectoryRules => "Trajectory Rules",
            Section::Tutorials => "Tutorials",
            Section::VFX => "VFX",
            Section::Voices => "Voices",
            Section::WeaponAnimationSetData => "Weapon Animation Sets",
            Section::ErrorDescriptions => "Error Descriptions",
            Section::ProgressionDescriptions => "Progression Descriptions",
            Section::CompanionPresets => "Companion Presets",
            Section::SpellLists => "Spell Lists",
            Section::SkillLists => "Skill Lists",
            Section::PassiveLists => "Passive Lists",
            Section::EquipmentLists => "Equipment Lists",
            Section::AbilityLists => "Ability Lists",
            Section::ExperienceRewards => "Experience Rewards",
            Section::GoldValues => "Gold Values",
            Section::DeathEffects => "Death Effects",
            Section::SpeakerGroups => "Speaker Groups",
            Section::Gossips => "Gossips",
        }
    }

    /// All sections in canonical output ordering.
    /// CF-eligible sections first (in CF output order), then extended sections.
    pub fn all_ordered() -> &'static [Section] {
        &[
            // CF-eligible sections
            Section::Races,
            Section::Progressions,
            Section::Lists,
            Section::Backgrounds,
            Section::BackgroundGoals,
            Section::ActionResources,
            Section::ActionResourceGroups,
            Section::ClassDescriptions,
            Section::Origins,
            Section::Feats,
            Section::Spells,
            // Extended sections
            Section::Gods,
            Section::Tags,
            Section::Visuals,
            Section::CharacterCreation,
            Section::CharacterCreationPresets,
            Section::ColorDefinitions,
            Section::FeatDescriptions,
            // Additional LSX data sections
            Section::Animation,
            Section::AnimationOverrides,
            Section::Calendar,
            Section::CinematicArenaFrequencyGroups,
            Section::CombatCameraGroups,
            Section::Content,
            Section::CustomDice,
            Section::DefaultValues,
            Section::DifficultyClasses,
            Section::Disturbances,
            Section::Encumbrance,
            Section::EquipmentTypes,
            Section::Factions,
            Section::Flags,
            Section::FixedHotBarSlots,
            Section::TextureAtlasInfo,
            Section::IconUVList,
            Section::ItemThrowParams,
            Section::Levelmaps,
            Section::LimbsMapping,
            Section::Meta,
            Section::MultiEffectInfos,
            Section::ProjectileDefaults,
            Section::RandomCasts,
            Section::RootTemplates,
            Section::Ruleset,
            Section::Shapeshift,
            Section::Sound,
            Section::SpellMetadata,
            Section::StatusMetadata,
            Section::Surface,
            Section::TooltipExtras,
            Section::TrajectoryRules,
            Section::Tutorials,
            Section::VFX,
            Section::Voices,
            Section::WeaponAnimationSetData,
            Section::ErrorDescriptions,
            Section::ProgressionDescriptions,
            Section::CompanionPresets,
            Section::SpellLists,
            Section::SkillLists,
            Section::PassiveLists,
            Section::EquipmentLists,
            Section::AbilityLists,
            Section::ExperienceRewards,
            Section::GoldValues,
            Section::DeathEffects,
            Section::SpeakerGroups,
            Section::Gossips,
        ]
    }

    /// Only CF-eligible sections in canonical output ordering.
    pub fn cf_ordered() -> &'static [Section] {
        &[
            Section::Races,
            Section::Progressions,
            Section::Lists,
            Section::Backgrounds,
            Section::BackgroundGoals,
            Section::ActionResources,
            Section::ActionResourceGroups,
            Section::ClassDescriptions,
            Section::Origins,
            Section::Feats,
            Section::Spells,
        ]
    }

    /// Parse a section name string to Section.
    pub fn parse_name(s: &str) -> Option<Section> {
        match s {
            "Races" => Some(Section::Races),
            "Progressions" => Some(Section::Progressions),
            "Lists" => Some(Section::Lists),
            "Feats" => Some(Section::Feats),
            "Origins" => Some(Section::Origins),
            "Backgrounds" => Some(Section::Backgrounds),
            "BackgroundGoals" => Some(Section::BackgroundGoals),
            "ActionResources" => Some(Section::ActionResources),
            "ActionResourceGroups" => Some(Section::ActionResourceGroups),
            "ClassDescriptions" => Some(Section::ClassDescriptions),
            "Spells" => Some(Section::Spells),
            "Gods" => Some(Section::Gods),
            "Tags" => Some(Section::Tags),
            "Visuals" => Some(Section::Visuals),
            "CharacterCreation" => Some(Section::CharacterCreation),
            "CharacterCreationPresets" => Some(Section::CharacterCreationPresets),
            "ColorDefinitions" => Some(Section::ColorDefinitions),
            "FeatDescriptions" => Some(Section::FeatDescriptions),
            "Animation" => Some(Section::Animation),
            "AnimationOverrides" => Some(Section::AnimationOverrides),
            "Calendar" => Some(Section::Calendar),
            "CinematicArenaFrequencyGroups" => Some(Section::CinematicArenaFrequencyGroups),
            "CombatCameraGroups" => Some(Section::CombatCameraGroups),
            "Content" => Some(Section::Content),
            "CustomDice" => Some(Section::CustomDice),
            "DefaultValues" => Some(Section::DefaultValues),
            "DifficultyClasses" => Some(Section::DifficultyClasses),
            "Disturbances" => Some(Section::Disturbances),
            "Encumbrance" => Some(Section::Encumbrance),
            "EquipmentTypes" => Some(Section::EquipmentTypes),
            "Factions" => Some(Section::Factions),
            "Flags" => Some(Section::Flags),
            "FixedHotBarSlots" => Some(Section::FixedHotBarSlots),
            "TextureAtlasInfo" => Some(Section::TextureAtlasInfo),
            "IconUVList" => Some(Section::IconUVList),
            "ItemThrowParams" => Some(Section::ItemThrowParams),
            "Levelmaps" => Some(Section::Levelmaps),
            "LimbsMapping" => Some(Section::LimbsMapping),
            "Meta" => Some(Section::Meta),
            "MultiEffectInfos" => Some(Section::MultiEffectInfos),
            "ProjectileDefaults" => Some(Section::ProjectileDefaults),
            "RandomCasts" => Some(Section::RandomCasts),
            "RootTemplates" => Some(Section::RootTemplates),
            "Ruleset" => Some(Section::Ruleset),
            "Shapeshift" => Some(Section::Shapeshift),
            "Sound" => Some(Section::Sound),
            "SpellMetadata" => Some(Section::SpellMetadata),
            "StatusMetadata" => Some(Section::StatusMetadata),
            "Surface" => Some(Section::Surface),
            "TooltipExtras" => Some(Section::TooltipExtras),
            "TrajectoryRules" => Some(Section::TrajectoryRules),
            "Tutorials" => Some(Section::Tutorials),
            "VFX" => Some(Section::VFX),
            "Voices" => Some(Section::Voices),
            "WeaponAnimationSetData" => Some(Section::WeaponAnimationSetData),
            "ErrorDescriptions" => Some(Section::ErrorDescriptions),
            "ProgressionDescriptions" => Some(Section::ProgressionDescriptions),
            "CompanionPresets" => Some(Section::CompanionPresets),
            "SpellLists" => Some(Section::SpellLists),
            "SkillLists" => Some(Section::SkillLists),
            "PassiveLists" => Some(Section::PassiveLists),
            "EquipmentLists" => Some(Section::EquipmentLists),
            "AbilityLists" => Some(Section::AbilityLists),
            "ExperienceRewards" => Some(Section::ExperienceRewards),
            "GoldValues" => Some(Section::GoldValues),
            "DeathEffects" => Some(Section::DeathEffects),
            "SpeakerGroups" => Some(Section::SpeakerGroups),
            "Gossips" => Some(Section::Gossips),
            _ => None,
        }
    }

    /// The filename for the consolidated per-type YAML file (e.g. "Progressions.yaml").
    pub fn consolidated_filename(&self) -> String {
        format!("{}.yaml", self.region_id())
    }

    /// Return the LSX region IDs that this section covers.
    /// Umbrella sections return multiple region IDs.
    pub fn region_ids(&self) -> &'static [&'static str] {
        match self {
            Section::Lists => &[
                "SpellLists",
                "SkillLists",
                "PassiveLists",
                "EquipmentLists",
                "AbilityLists",
            ],
            Section::CharacterCreation => &[
                "CharacterCreationPresets",
                "CharacterCreationAppearanceVisuals",
                "CharacterCreationEyeColors",
                "CharacterCreationHairColors",
                "CharacterCreationSkinColors",
                "CharacterCreationSharedVisuals",
                "CharacterCreationAccessorySets",
                "CharacterCreationVOLines",
            ],
            Section::Animation => &["AnimationSetBank", "AnimationSetPriorities"],
            Section::Sound => &["SoundBank", "SoundPreloadSettings"],
            Section::VFX => &["GameplayVFXs", "PassivesVFX", "ManagedStatusVFX"],
            Section::Visuals => &["CharacterVisualBank", "VisualBank"],
            Section::Ruleset => &["Rulesets", "RulesetModifiers", "RulesetValues"],
            Section::Surface => &["SurfaceCursorMessages"],
            Section::ActionResources => &["ActionResourceDefinitions"],
            Section::ActionResourceGroups => &["ActionResourceGroupDefinitions"],
            Section::SpellMetadata => &["Spell"],
            Section::StatusMetadata => &["Status"],
            Section::Meta => &["Config"],
            Section::Factions => &["FactionContainer"],
            Section::Disturbances => &["DisturbanceProperties"],
            Section::Levelmaps => &["LevelMapValues"],
            Section::TooltipExtras => &["TooltipExtraTexts"],
            Section::Encumbrance => &["WeightCategories"],
            Section::ErrorDescriptions => &["ConditionErrors"],
            Section::RootTemplates => &["Templates"],
            Section::Races => &["Races"],
            Section::Progressions => &["Progressions"],
            Section::Feats => &["Feats"],
            Section::Origins => &["Origins"],
            Section::Backgrounds => &["Backgrounds"],
            Section::BackgroundGoals => &["BackgroundGoals"],
            Section::ClassDescriptions => &["ClassDescriptions"],
            Section::Spells => &["Spells"],
            Section::Gods => &["Gods"],
            Section::Tags => &["Tags"],
            Section::CharacterCreationPresets => &["CharacterCreationPresets"],
            Section::ColorDefinitions => &["ColorDefinitions"],
            Section::FeatDescriptions => &["FeatDescriptions"],
            Section::AnimationOverrides => &["AnimationOverrides"],
            Section::Calendar => &["Calendar"],
            Section::CinematicArenaFrequencyGroups => &["CinematicArenaFrequencyGroups"],
            Section::CombatCameraGroups => &["CombatCameraGroups"],
            Section::Content => &[
                "AnimationBank",
                "AnimationBlueprintBank",
                "AnimationSetBank",
                "AtmosphereBank",
                "BlendSpaceBank",
                "ClothColliderBank",
                "ColorListBank",
                "DialogBank",
                "DiffusionProfileBank",
                "EffectBank",
                "FCurveBank",
                "IKRigBank",
                "LightCookieBank",
                "LightingBank",
                "MaterialBank",
                "MaterialPresetBank",
                "MeshProxyBank",
                "PhysicsBank",
                "ScriptBank",
                "SkeletonBank",
                "SoundBank",
                "SplineSetBank",
                "TerrainBrushBank",
                "TextureBank",
                "TileSetBank",
                "TimelineBank",
                "TimelineSceneBank",
                "VirtualTextureBank",
            ],
            Section::CustomDice => &["CustomDice"],
            Section::DefaultValues => &["DefaultValues"],
            Section::DifficultyClasses => &["DifficultyClasses"],
            Section::EquipmentTypes => &["EquipmentTypes"],
            Section::Flags => &["Flags"],
            Section::FixedHotBarSlots => &["FixedHotBarSlots"],
            Section::TextureAtlasInfo => &["TextureAtlasInfo"],
            Section::IconUVList => &["IconUVList"],
            Section::ItemThrowParams => &["ItemThrowParams"],
            Section::LimbsMapping => &["LimbsMapping"],
            Section::MultiEffectInfos => &["MultiEffectInfos"],
            Section::ProjectileDefaults => &["ProjectileDefaults"],
            Section::RandomCasts => &["RandomCasts"],
            Section::Shapeshift => &["Shapeshift"],
            Section::TrajectoryRules => &["TrajectoryRules"],
            Section::Tutorials => &["Tutorials"],
            Section::Voices => &["Voices"],
            Section::WeaponAnimationSetData => &["WeaponAnimationSetData"],
            Section::ProgressionDescriptions => &["ProgressionDescriptions"],
            Section::CompanionPresets => &["CompanionPresets"],
            Section::SpellLists => &["SpellLists"],
            Section::SkillLists => &["SkillLists"],
            Section::PassiveLists => &["PassiveLists"],
            Section::EquipmentLists => &["EquipmentLists"],
            Section::AbilityLists => &["AbilityLists"],
            Section::ExperienceRewards => &["ExperienceRewards"],
            Section::GoldValues => &["GoldValues"],
            Section::DeathEffects => &["DeathEffects"],
            Section::SpeakerGroups => &["SpeakerGroups"],
            Section::Gossips => &["Gossips"],
        }
    }
}

/// Result of scanning a mod folder.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export))]
pub struct ScanResult {
    pub mod_meta: ModMeta,
    pub sections: Vec<SectionResult>,
    pub existing_config_path: Option<String>,
}

/// All diff entries for a single CF section.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export))]
pub struct SectionResult {
    pub section: Section,
    pub entries: Vec<DiffEntry>,
}

/// A single diffed entry with its changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export))]
pub struct DiffEntry {
    pub uuid: String,
    pub display_name: String,
    pub source_file: String,
    pub entry_kind: EntryKind,
    pub changes: Vec<Change>,
    /// The LSX node_id for this entry (e.g. "SpellList", "PassiveList", "Progression").
    #[serde(default)]
    pub node_id: String,
    /// The LSX region ID this entry belongs to (e.g. "AnimationBank", "GameplayVFXs").
    #[serde(default)]
    pub region_id: String,
    /// Raw attributes from the mod's LSX entry (key → value).
    #[serde(default)]
    pub raw_attributes: HashMap<String, String>,
    /// LSX attribute types for each raw_attribute key (key → type string, e.g. "FixedString", "guid").
    #[serde(default)]
    pub raw_attribute_types: HashMap<String, String>,
    /// Raw children groups from the mod's LSX entry (group_id → list of object GUIDs).
    #[serde(default)]
    pub raw_children: HashMap<String, Vec<String>>,
    /// True if this entry was commented out in the source .lsx file.
    #[serde(default)]
    pub commented: bool,
}

/// Classification of an entry relative to vanilla data.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export))]
pub enum EntryKind {
    Modified,
    New,
}

/// A single change detected within a diff entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export))]
pub struct Change {
    pub change_type: ChangeType,
    pub field: String,
    pub added_values: Vec<String>,
    pub removed_values: Vec<String>,
    pub vanilla_value: Option<String>,
    pub mod_value: Option<String>,
}

/// Type of change detected.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export))]
pub enum ChangeType {
    StringAdded,
    StringRemoved,
    SelectorAdded,
    SelectorRemoved,
    BooleanChanged,
    FieldChanged,
    ChildAdded,
    ChildRemoved,
    SpellFieldChanged,
    EntireEntryNew,
}

/// An entry selected for inclusion in the config output.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export))]
pub struct SelectedEntry {
    pub section: Section,
    pub uuid: String,
    pub display_name: String,
    pub changes: Vec<Change>,
    pub manual: bool,
    /// For Lists entries: the actual list type (e.g. "SpellList", "PassiveList",
    /// "SkillList", "AbilityList", "EquipmentList"). Falls back to "SpellList"
    /// when absent for backward compatibility.
    #[serde(default)]
    pub list_type: Option<String>,
}

/// Options for YAML/JSON serialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export))]
pub struct SerializeOptions {
    pub format: OutputFormat,
    pub use_anchors: bool,
    pub include_comments: bool,
}

/// Output format enum.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export))]
pub enum OutputFormat {
    Yaml,
    Json,
}

/// A manually-created entry from the form UI.
/// `fields` uses structured keys (e.g. "UUID", "Selector:0:Function", "String:1:Values").
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export))]
pub struct ManualEntry {
    pub section: String,
    pub fields: HashMap<String, String>,
    #[serde(default)]
    pub comment: Option<String>,
}

/// Represents a detected anchor opportunity.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(test, derive(TS))]
#[cfg_attr(test, ts(export))]
pub struct AnchorGroup {
    pub anchor_name: String,
    pub shared_changes: Vec<Change>,
    pub entry_uuids: Vec<String>,
    pub lines_saved: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Cross-language validation: ensures Section::all_ordered() matches the
    /// canonical list. A matching TS test checks SECTIONS_ORDERED against
    /// the same expected order. If either side changes, one test will fail.
    #[test]
    fn section_order_matches_canonical() {
        let expected = [
            // CF-eligible sections
            Section::Races,
            Section::Progressions,
            Section::Lists,
            Section::Backgrounds,
            Section::BackgroundGoals,
            Section::ActionResources,
            Section::ActionResourceGroups,
            Section::ClassDescriptions,
            Section::Origins,
            Section::Feats,
            Section::Spells,
            // Extended sections
            Section::Gods,
            Section::Tags,
            Section::Visuals,
            Section::CharacterCreation,
            Section::CharacterCreationPresets,
            Section::ColorDefinitions,
            Section::FeatDescriptions,
            // Additional LSX data sections
            Section::Animation,
            Section::AnimationOverrides,
            Section::Calendar,
            Section::CinematicArenaFrequencyGroups,
            Section::CombatCameraGroups,
            Section::Content,
            Section::CustomDice,
            Section::DefaultValues,
            Section::DifficultyClasses,
            Section::Disturbances,
            Section::Encumbrance,
            Section::EquipmentTypes,
            Section::Factions,
            Section::Flags,
            Section::FixedHotBarSlots,
            Section::TextureAtlasInfo,
            Section::IconUVList,
            Section::ItemThrowParams,
            Section::Levelmaps,
            Section::LimbsMapping,
            Section::Meta,
            Section::MultiEffectInfos,
            Section::ProjectileDefaults,
            Section::RandomCasts,
            Section::RootTemplates,
            Section::Ruleset,
            Section::Shapeshift,
            Section::Sound,
            Section::SpellMetadata,
            Section::StatusMetadata,
            Section::Surface,
            Section::TooltipExtras,
            Section::TrajectoryRules,
            Section::Tutorials,
            Section::VFX,
            Section::Voices,
            Section::WeaponAnimationSetData,
            Section::ErrorDescriptions,
            Section::ProgressionDescriptions,
            Section::CompanionPresets,
            Section::SpellLists,
            Section::SkillLists,
            Section::PassiveLists,
            Section::EquipmentLists,
            Section::AbilityLists,
            Section::ExperienceRewards,
            Section::GoldValues,
            Section::DeathEffects,
            Section::SpeakerGroups,
            Section::Gossips,
        ];
        assert_eq!(
            Section::all_ordered(),
            &expected,
            "Section::all_ordered() diverged from canonical section order"
        );
    }

    #[test]
    fn all_ordered_covers_all_variants() {
        let ordered = Section::all_ordered();
        let all_variants = [
            // CF-eligible sections
            Section::Races,
            Section::Progressions,
            Section::Lists,
            Section::Feats,
            Section::Origins,
            Section::Backgrounds,
            Section::BackgroundGoals,
            Section::ActionResources,
            Section::ActionResourceGroups,
            Section::ClassDescriptions,
            Section::Spells,
            // Extended sections
            Section::Gods,
            Section::Tags,
            Section::Visuals,
            Section::CharacterCreation,
            Section::CharacterCreationPresets,
            Section::ColorDefinitions,
            Section::FeatDescriptions,
            // Additional LSX data sections
            Section::Animation,
            Section::AnimationOverrides,
            Section::Calendar,
            Section::CinematicArenaFrequencyGroups,
            Section::CombatCameraGroups,
            Section::Content,
            Section::CustomDice,
            Section::DefaultValues,
            Section::DifficultyClasses,
            Section::Disturbances,
            Section::Encumbrance,
            Section::EquipmentTypes,
            Section::Factions,
            Section::Flags,
            Section::FixedHotBarSlots,
            Section::TextureAtlasInfo,
            Section::IconUVList,
            Section::ItemThrowParams,
            Section::Levelmaps,
            Section::LimbsMapping,
            Section::Meta,
            Section::MultiEffectInfos,
            Section::ProjectileDefaults,
            Section::RandomCasts,
            Section::RootTemplates,
            Section::Ruleset,
            Section::Shapeshift,
            Section::Sound,
            Section::SpellMetadata,
            Section::StatusMetadata,
            Section::Surface,
            Section::TooltipExtras,
            Section::TrajectoryRules,
            Section::Tutorials,
            Section::VFX,
            Section::Voices,
            Section::WeaponAnimationSetData,
            Section::ErrorDescriptions,
            Section::ProgressionDescriptions,
            Section::CompanionPresets,
            Section::SpellLists,
            Section::SkillLists,
            Section::PassiveLists,
            Section::EquipmentLists,
            Section::AbilityLists,
            Section::ExperienceRewards,
            Section::GoldValues,
            Section::DeathEffects,
            Section::SpeakerGroups,
            Section::Gossips,
        ];
        for variant in &all_variants {
            assert!(
                ordered.contains(variant),
                "Section::all_ordered() is missing {variant:?}"
            );
        }
        assert_eq!(
            ordered.len(),
            all_variants.len(),
            "Section::all_ordered() length doesn't match variant count"
        );
    }

    #[test]
    fn classify_runtime_lsfx_binary() {
        let path = Path::new(
            r"Public/Spjammer_4511f6b4-7422-efd2-8db1-31f6352c8a9a/Assets/Effects/Effects_Banks/VFX_Spjm_AstralStrikeSpell_Impact_01.lsfx",
        );
        assert_eq!(
            EffectFileKind::from_path(path),
            Some(EffectFileKind::RuntimeLsfxBinary)
        );
        assert!(EffectFileKind::RuntimeLsfxBinary.is_runtime_artifact());
        assert!(!EffectFileKind::RuntimeLsfxBinary.requires_toolkit_compile_step());
    }

    #[test]
    fn classify_runtime_lsfx_converted_lsx() {
        let path = Path::new(
            r"Public/Spjammer_4511f6b4-7422-efd2-8db1-31f6352c8a9a/Assets/Effects/Effects_Banks/VFX_Spjm_AstralStrikeSpell_Impact_01.lsfx.lsx",
        );
        assert_eq!(
            EffectFileKind::from_path(path),
            Some(EffectFileKind::RuntimeLsfxConvertedLsx)
        );
        assert!(EffectFileKind::RuntimeLsfxConvertedLsx.is_runtime_artifact());
        assert!(!EffectFileKind::RuntimeLsfxConvertedLsx.requires_toolkit_compile_step());
    }

    #[test]
    fn classify_toolkit_lsefx_source() {
        let path = Path::new(
            r"Public/Spjammer_4511f6b4-7422-efd2-8db1-31f6352c8a9a/Content/Assets/Effects/Effects/Actions/[PAK]_Cast/VFX_Spjm_AstralStrikeSpell_Impact_01.lsefx",
        );
        assert_eq!(
            EffectFileKind::from_path(path),
            Some(EffectFileKind::ToolkitLsefxSource)
        );
        assert!(EffectFileKind::ToolkitLsefxSource.requires_toolkit_compile_step());
        assert!(!EffectFileKind::ToolkitLsefxSource.is_runtime_artifact());
    }
}
