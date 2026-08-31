//! Foreign key pattern definitions for the reference DB.
//!
//! Three categories of FK rules:
//! 1. Column-name regex patterns (FK_PATTERNS) — matches any table
//! 2. Object-column context rules (OBJECT_FK_BY_NODE) — node_id determines target
//! 3. Table-specific hardcoded FKs (TABLE_SPECIFIC_FKS)
//!
//! ## Known FK violations (accepted, 186 remaining)
//!
//! These are genuine data gaps in BG3's shipped data that cannot be resolved
//! by adjusting FK rules. They fall into five categories:
//!
//! | Category | Count | Details |
//! |---|---|---|
//! | **Tags** | 143 | Tag UUIDs referenced by FaceExpressions.TagsFilter (77), Templates.Tag (64), and MultiEffectInfos.TargetIgnoreTag (2) that don't appear in any ingested Tag table. Likely defined in level data or hardcoded engine tags. |
//! | **GameObjects** | 27 | Stats entries (Object 20, StatusData 4, Armor 2) and LimbsMapping (1) referencing GameObjects.MapKey UUIDs for level-placed templates not in Public dirs. Includes `ALTER_SELF_HALFDROW` TemplateID. |
//! | **PhysicsBank** | 13 | PhysicsTemplate UUIDs with no match in PhysicsBank (99.9% valid rate overall; 15,171/15,184). |
//! | **MultiEffectInfos** | 3 | CastEffect UUIDs orphaned from MultiEffectInfos (99.8% valid rate; 1,379/1,382). |
//!
//! ## Intentionally non-FK columns
//!
//! These columns look like FKs but are deliberately left as plain strings:
//!
//! - **GlobalTemplate**: References level-placed template UUIDs, not GameObjects.MapKey (0/25 match).
//! - **VisualTemplate**: Polymorphic — targets VisualBank OR EffectBank (398/427 orphans were in EffectBank).
//! - **MaterialPresetUUID / Active / Inactive**: 0/22 valid matches in any ingested table.
//! - **SpellContainerID**: Grouping label that often but not always corresponds to a SpellData entry name.
//! - **AddidionalObjects.Object** (AnimationSetPriorities): Polymorphic — references both ClassDescription and Race UUIDs.

use regex::Regex;
use std::sync::LazyLock;

/// A column-name-based FK pattern.
pub struct FkPattern {
    pub regex: Regex,
    pub target_table: Option<&'static str>,
    pub target_column: Option<&'static str>,
    pub fk_type: FkType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FkType {
    SelfPk,
    Uuid,
    UuidSelf,
    Name,
    ContentUid,
    ContentUidVersioned,
    UuidUnresolved,
    TemplateUnresolved,
}

macro_rules! fk {
    ($pat:expr, $tbl:expr, $col:expr, $ty:expr) => {
        FkPattern {
            regex: Regex::new($pat).unwrap(),
            target_table: $tbl,
            target_column: $col,
            fk_type: $ty,
        }
    };
}

/// All column-name FK patterns, ordered by specificity (most specific first).
/// Fallback patterns (UUID$, Template$) must be at the end.
pub static FK_PATTERNS: LazyLock<Vec<FkPattern>> = LazyLock::new(|| {
    use FkType::*;
    vec![
        // Self-PKs
        fk!(r"(?i)^UUID$", None, None, SelfPk),
        fk!(r"(?i)^MapKey$", None, None, SelfPk),
        // Templates / GameObjects
        fk!(
            r"(?i)^ParentTemplateId$",
            Some("lsx__Templates__GameObjects"),
            Some("MapKey"),
            Uuid
        ),
        fk!(
            r"(?i)^RootTemplate$",
            Some("lsx__Templates__GameObjects"),
            Some("MapKey"),
            Uuid
        ),
        fk!(
            r"(?i)^TemplateId$",
            Some("lsx__Templates__GameObjects"),
            Some("MapKey"),
            Uuid
        ),
        // GlobalTemplate references level-placed templates (Levels/), not Public/ GameObjects.
        // 0/25 Origins.GlobalTemplate values found in GameObjects — leave unresolved.
        fk!(r"(?i)^GlobalTemplate$", None, None, UuidUnresolved),
        fk!(
            r"(?i)^CharacterTemplateUUID$",
            Some("lsx__Templates__GameObjects"),
            Some("MapKey"),
            Uuid
        ),
        fk!(
            r"(?i)^RootTemplateId$",
            Some("lsx__Templates__GameObjects"),
            Some("MapKey"),
            Uuid
        ),
        fk!(
            r"(?i)^DefaultsTemplate$",
            Some("lsx__Templates__GameObjects"),
            Some("MapKey"),
            Uuid
        ),
        fk!(
            r"(?i)^FemaleRootTemplate$",
            Some("lsx__Templates__GameObjects"),
            Some("MapKey"),
            Uuid
        ),
        fk!(
            r"(?i)^MaleRootTemplate$",
            Some("lsx__Templates__GameObjects"),
            Some("MapKey"),
            Uuid
        ),
        fk!(
            r"(?i)^DragonbornFemaleRootTemplate$",
            Some("lsx__Templates__GameObjects"),
            Some("MapKey"),
            Uuid
        ),
        fk!(
            r"(?i)^DragonbornMaleRootTemplate$",
            Some("lsx__Templates__GameObjects"),
            Some("MapKey"),
            Uuid
        ),
        fk!(
            r"(?i)^EquipmentTemplate$",
            Some("lsx__Templates__GameObjects"),
            Some("MapKey"),
            Uuid
        ),
        fk!(
            r"(?i)^ProjectileTemplateId$",
            Some("lsx__Templates__GameObjects"),
            Some("MapKey"),
            Uuid
        ),
        // Visual / Physics banks
        // VisualTemplate is polymorphic: can reference VisualBank OR EffectBank.
        // SQLite doesn't support multi-target FKs, so leave unresolved.
        fk!(r"(?i)^VisualTemplate$", None, None, UuidUnresolved),
        fk!(
            r"(?i)^PhysicsTemplate$",
            Some("lsx__PhysicsBank__Resource"),
            Some("ID"),
            Uuid
        ),
        // Races
        fk!(
            r"(?i)^RaceUUID$",
            Some("lsx__Races__Race"),
            Some("UUID"),
            Uuid
        ),
        fk!(
            r"(?i)^SubRaceUUID$",
            Some("lsx__Races__Race"),
            Some("UUID"),
            Uuid
        ),
        fk!(
            r"(?i)^DisplayTypeUUID$",
            Some("lsx__Races__Race"),
            Some("UUID"),
            Uuid
        ),
        // Classes
        fk!(
            r"(?i)^ClassUUID$",
            Some("lsx__ClassDescriptions__ClassDescription"),
            Some("UUID"),
            Uuid
        ),
        fk!(
            r"(?i)^SubclassUUID$",
            Some("lsx__ClassDescriptions__ClassDescription"),
            Some("UUID"),
            Uuid
        ),
        fk!(
            r"(?i)^SubClassUUID$",
            Some("lsx__ClassDescriptions__ClassDescription"),
            Some("UUID"),
            Uuid
        ),
        fk!(
            r"(?i)^PreferredClassUUID$",
            Some("lsx__ClassDescriptions__ClassDescription"),
            Some("UUID"),
            Uuid
        ),
        fk!(
            r"(?i)^ClassId$",
            Some("lsx__ClassDescriptions__ClassDescription"),
            Some("UUID"),
            Uuid
        ),
        fk!(
            r"(?i)^DummyClass$",
            Some("lsx__ClassDescriptions__ClassDescription"),
            Some("UUID"),
            Uuid
        ),
        // Backgrounds
        fk!(
            r"(?i)^BackgroundUUID$",
            Some("lsx__Backgrounds__Background"),
            Some("UUID"),
            Uuid
        ),
        // Progressions
        fk!(
            r"(?i)^ProgressionTableUUID$",
            Some("lsx__Progressions__Progression"),
            Some("TableUUID"),
            Uuid
        ),
        fk!(
            r"(?i)^ProgressionTableId$",
            Some("lsx__Progressions__Progression"),
            Some("TableUUID"),
            Uuid
        ),
        fk!(
            r"(?i)^ProgressionId$",
            Some("lsx__Progressions__Progression"),
            Some("UUID"),
            Uuid
        ),
        fk!(
            r"(?i)^TableUUID$",
            Some("lsx__Progressions__Progression"),
            Some("TableUUID"),
            Uuid
        ),
        // Feats
        fk!(r"(?i)^FeatId$", Some("lsx__Feats"), Some("UUID"), Uuid),
        // Gods
        fk!(r"(?i)^GodUUID$", Some("lsx__Gods__God"), Some("UUID"), Uuid),
        // Origins
        fk!(
            r"(?i)^OriginUUID$",
            Some("lsx__Origins__Origin"),
            Some("UUID"),
            Uuid
        ),
        fk!(
            r"(?i)^ExcludesOriginUUID$",
            Some("lsx__Origins__Origin"),
            Some("UUID"),
            Uuid
        ),
        // Tags
        fk!(
            r"(?i)^TagUUID$",
            Some("lsx__Tags__Tags"),
            Some("UUID"),
            Uuid
        ),
        // Spell lists
        fk!(
            r"(?i)^SpellList$",
            Some("lsx__SpellLists"),
            Some("UUID"),
            Uuid
        ),
        // Material presets — 0% valid matches in CharacterCreationMaterialOverrides;
        // these UUIDs don't resolve to any MaterialPresetBank entries in unpacked data.
        fk!(r"(?i)^MaterialPresetUUID$", None, None, UuidUnresolved),
        fk!(
            r"(?i)^ActiveMaterialPresetUUID$",
            None,
            None,
            UuidUnresolved
        ),
        fk!(
            r"(?i)^InactiveMaterialPresetUUID$",
            None,
            None,
            UuidUnresolved
        ),
        // Character creation
        fk!(
            r"(?i)^HeadAppearanceUUID$",
            Some("lsx__CharacterCreationAppearanceVisuals__CharacterCreationAppearanceVisual"),
            Some("UUID"),
            Uuid
        ),
        fk!(r"(?i)^CharacterCreationPose$", None, None, UuidUnresolved),
        // Voices — VoiceTableUUID references Voice.TableUUID (non-unique junction),
        // so we leave it as an unresolved UUID rather than a concrete FK.
        fk!(r"(?i)^VoiceTableUUID$", None, None, UuidUnresolved),
        fk!(
            r"(?i)^VOLinesTableUUID$",
            Some("lsx__CharacterCreationVOLines"),
            Some("UUID"),
            Uuid
        ),
        fk!(
            r"(?i)^SpeakerUUID$",
            Some("lsx__SpeakerGroups"),
            Some("UUID"),
            Uuid
        ),
        // Flags
        fk!(r"(?i)^FlagUUID$", Some("lsx__Flags"), Some("UUID"), Uuid),
        // Gold values
        fk!(
            r"(?i)^ValueUUID$",
            Some("lsx__GoldValues"),
            Some("UUID"),
            Uuid
        ),
        // Experience
        fk!(
            r"(?i)^ExperienceReward$",
            Some("lsx__ExperienceRewards"),
            Some("UUID"),
            Uuid
        ),
        fk!(
            r"(?i)^ExplorationReward$",
            Some("lsx__ExperienceRewards"),
            Some("UUID"),
            Uuid
        ),
        // Self-references
        fk!(r"(?i)^ParentUUID$", None, Some("UUID"), UuidSelf),
        fk!(r"(?i)^ParentGuid$", None, Some("UUID"), UuidSelf),
        fk!(r"(?i)^ParentGUID$", None, Some("MapKey"), UuidSelf),
        // Action resources
        fk!(
            r"(?i)^ActionResource$",
            Some("lsx__ActionResourceDefinitions"),
            Some("UUID"),
            Uuid
        ),
        // Death effects
        fk!(
            r"(?i)^DeathEffect$",
            Some("lsx__DeathEffects"),
            Some("UUID"),
            Uuid
        ),
        // Difficulty classes
        fk!(
            r"(?i)^DisarmDifficultyClassID$",
            Some("lsx__DifficultyClasses"),
            Some("UUID"),
            Uuid
        ),
        fk!(
            r"(?i)^LockDifficultyClassID$",
            Some("lsx__DifficultyClasses"),
            Some("UUID"),
            Uuid
        ),
        // Equipment types
        fk!(
            r"(?i)^EquipmentTypeID$",
            Some("lsx__EquipmentTypes"),
            Some("UUID"),
            Uuid
        ),
        // Factions
        fk!(
            r"(?i)^Faction$",
            Some("lsx__FactionContainer"),
            Some("UUID"),
            Uuid
        ),
        // Rulesets
        fk!(
            r"(?i)^Modifier$",
            Some("lsx__RulesetModifiers"),
            Some("UUID"),
            Uuid
        ),
        fk!(r"(?i)^Ruleset$", Some("lsx__Rulesets"), Some("UUID"), Uuid),
        fk!(r"(?i)^Rulesets$", Some("lsx__Rulesets"), Some("UUID"), Uuid),
        // Short name categories
        fk!(
            r"(?i)^CategoryGuid$",
            Some("lsx__ShortNameCategories"),
            Some("UUID"),
            Uuid
        ),
        // VFX / Effects → MultiEffectInfos
        fk!(
            r"(?i)^CastEffect$",
            Some("lsx__MultiEffectInfos__MultiEffectInfos"),
            Some("UUID"),
            Uuid
        ),
        fk!(
            r"(?i)^PrepareEffect$",
            Some("lsx__MultiEffectInfos__MultiEffectInfos"),
            Some("UUID"),
            Uuid
        ),
        fk!(
            r"(?i)^MixedEffect$",
            Some("lsx__MultiEffectInfos__MultiEffectInfos"),
            Some("UUID"),
            Uuid
        ),
        fk!(
            r"(?i)^NegativeEffect$",
            Some("lsx__MultiEffectInfos__MultiEffectInfos"),
            Some("UUID"),
            Uuid
        ),
        fk!(
            r"(?i)^PositiveEffect$",
            Some("lsx__MultiEffectInfos__MultiEffectInfos"),
            Some("UUID"),
            Uuid
        ),
        // Stats cross-refs
        // SpellContainerID intentionally omitted — values often but not always
        // correspond to a SpellData entry (e.g. Target_DistractingStrike_Container
        // exists only as a grouping label, not as its own entry).
        fk!(
            r"(?i)^RootSpellID$",
            Some("stats__SpellData"),
            Some("_entry_name"),
            Name
        ),
        fk!(
            r"(?i)^ConcentrationSpellID$",
            Some("stats__SpellData"),
            Some("_entry_name"),
            Name
        ),
        // Fallback patterns (must be last)
        fk!(r"UUID$", None, None, UuidUnresolved),
        fk!(r"(?i)Template$", None, None, TemplateUnresolved),
    ]
});

/// Object column FK targets by node_id.
pub static OBJECT_FK_BY_NODE: &[(&str, &str, &str)] = &[
    ("Tags", "lsx__Tags__Tags", "UUID"),
    ("Tag", "lsx__Tags__Tags", "UUID"),
    ("ExcludedTags", "lsx__Tags__Tags", "UUID"),
    ("TagsFilter", "lsx__Tags__Tags", "UUID"),
    ("TagsAdd", "lsx__Tags__Tags", "UUID"),
    ("TagsRemove", "lsx__Tags__Tags", "UUID"),
    ("ReallyTags", "lsx__Tags__Tags", "UUID"),
    ("AppearanceTags", "lsx__Tags__Tags", "UUID"),
    ("InteractionFilter", "lsx__Tags__Tags", "UUID"),
    ("ExcludedGods", "lsx__Gods__God", "UUID"),
    ("Gods", "lsx__Gods__God", "UUID"),
    (
        "SubClass",
        "lsx__ClassDescriptions__ClassDescription",
        "UUID",
    ),
    // AddidionalObjects is polymorphic: contains ClassDescription UUIDs AND Race UUIDs.
    // Can't express multi-target FK in SQLite, so leave Object column unresolved.
    ("VisualUUIDs", "lsx__CharacterCreationSharedVisuals", "UUID"),
    (
        "AccessorySetUUIDs",
        "lsx__CharacterCreationAccessorySets__CharacterCreationAccessorySet",
        "UUID",
    ),
    (
        "AppearanceMaterialUUIDs",
        "lsx__CharacterCreationAppearanceMaterials",
        "UUID",
    ),
    (
        "ColorMaterialUUIDs",
        "lsx__CharacterCreationAppearanceMaterials",
        "UUID",
    ),
    ("ClothsColors", "lsx__CrowdCharacterMaterialPresets", "UUID"),
    (
        "ClothsPatterns",
        "lsx__CrowdCharacterMaterialPresets",
        "UUID",
    ),
    ("EyeColors", "lsx__CharacterCreationEyeColors", "UUID"),
    ("SkinColors", "lsx__CharacterCreationSkinColors", "UUID"),
    ("HairColors", "lsx__CharacterCreationHairColors", "UUID"),
    ("HairGrayingColors", "lsx__ColorDefinitions", "UUID"),
    ("HairHighlightColors", "lsx__ColorDefinitions", "UUID"),
    ("HornColors", "lsx__ColorDefinitions", "UUID"),
    ("HornTipColors", "lsx__ColorDefinitions", "UUID"),
    ("LipsMakeupColors", "lsx__ColorDefinitions", "UUID"),
    ("MakeupColors", "lsx__ColorDefinitions", "UUID"),
    ("TattooColors", "lsx__ColorDefinitions", "UUID"),
    ("Visuals", "lsx__CharacterCreationSharedVisuals", "UUID"),
    ("AnimationsList", "lsx__EmoteAnimations", "UUID"),
    ("PosesList", "lsx__EmotePoses", "UUID"),
    (
        "FaceExpressionsList",
        "lsx__FaceExpressions__FaceExpression",
        "UUID",
    ),
    (
        "ParameterUUIDs",
        "lsx__ScriptMaterialOverrideParameters",
        "UUID",
    ),
    (
        "PrerequisiteUUIDs",
        "lsx__TadpolePowersTree__TadpolePowerNode",
        "UUID",
    ),
];

/// Table-specific FK overrides for columns too generic for name-based patterns.
pub static TABLE_SPECIFIC_FKS: &[(&str, &str, &str, &str)] = &[
    (
        "lsx__FactionManager",
        "Source",
        "lsx__FactionContainer",
        "UUID",
    ),
    (
        "lsx__FactionManager",
        "Target",
        "lsx__FactionContainer",
        "UUID",
    ),
    (
        "lsx__Crime__Property",
        "Properties",
        "lsx__DisturbanceProperties",
        "UUID",
    ),
    ("lsx__Reactions", "id", "lsx__Origins__Origin", "UUID"),
    (
        "lsx__MultiEffectInfos__SourceIgnoreTag",
        "Value",
        "lsx__Tags__Tags",
        "UUID",
    ),
    (
        "lsx__MultiEffectInfos__SourceTag",
        "Value",
        "lsx__Tags__Tags",
        "UUID",
    ),
    (
        "lsx__MultiEffectInfos__TargetIgnoreTag",
        "Value",
        "lsx__Tags__Tags",
        "UUID",
    ),
    (
        "lsx__MultiEffectInfos__TargetTag",
        "Value",
        "lsx__Tags__Tags",
        "UUID",
    ),
    (
        "lsx__DeathEffects",
        "Acid",
        "lsx__MultiEffectInfos__MultiEffectInfos",
        "UUID",
    ),
    (
        "lsx__DeathEffects",
        "Chasm",
        "lsx__MultiEffectInfos__MultiEffectInfos",
        "UUID",
    ),
    (
        "lsx__DeathEffects",
        "CinematicDeath",
        "lsx__MultiEffectInfos__MultiEffectInfos",
        "UUID",
    ),
    (
        "lsx__DeathEffects",
        "Cold",
        "lsx__MultiEffectInfos__MultiEffectInfos",
        "UUID",
    ),
    (
        "lsx__DeathEffects",
        "Disintegrate",
        "lsx__MultiEffectInfos__MultiEffectInfos",
        "UUID",
    ),
    (
        "lsx__DeathEffects",
        "DoT",
        "lsx__MultiEffectInfos__MultiEffectInfos",
        "UUID",
    ),
    (
        "lsx__DeathEffects",
        "Electrocution",
        "lsx__MultiEffectInfos__MultiEffectInfos",
        "UUID",
    ),
    (
        "lsx__DeathEffects",
        "Explode",
        "lsx__MultiEffectInfos__MultiEffectInfos",
        "UUID",
    ),
    (
        "lsx__DeathEffects",
        "Fallback",
        "lsx__MultiEffectInfos__MultiEffectInfos",
        "UUID",
    ),
    (
        "lsx__DeathEffects",
        "Falling",
        "lsx__MultiEffectInfos__MultiEffectInfos",
        "UUID",
    ),
    (
        "lsx__DeathEffects",
        "Incinerate",
        "lsx__MultiEffectInfos__MultiEffectInfos",
        "UUID",
    ),
    (
        "lsx__DeathEffects",
        "KnockedDown",
        "lsx__MultiEffectInfos__MultiEffectInfos",
        "UUID",
    ),
    (
        "lsx__DeathEffects",
        "Lifetime",
        "lsx__MultiEffectInfos__MultiEffectInfos",
        "UUID",
    ),
    (
        "lsx__DeathEffects",
        "Necrotic",
        "lsx__MultiEffectInfos__MultiEffectInfos",
        "UUID",
    ),
    (
        "lsx__DeathEffects",
        "Physical",
        "lsx__MultiEffectInfos__MultiEffectInfos",
        "UUID",
    ),
    (
        "lsx__DeathEffects",
        "Psychic",
        "lsx__MultiEffectInfos__MultiEffectInfos",
        "UUID",
    ),
    (
        "lsx__DeathEffects",
        "Radiant",
        "lsx__MultiEffectInfos__MultiEffectInfos",
        "UUID",
    ),
];

/// Look up the FK pattern for a given column name and BG3 type.
pub fn match_fk_pattern(
    col_name: &str,
    bg3_type: &str,
) -> Option<(Option<&'static str>, Option<&'static str>, FkType)> {
    for pat in FK_PATTERNS.iter() {
        if pat.regex.is_match(col_name) {
            if pat.fk_type == FkType::TemplateUnresolved && bg3_type != "guid" {
                return None;
            }
            return Some((pat.target_table, pat.target_column, pat.fk_type));
        }
    }
    None
}

/// Look up the Object column FK target for a given node_id.
pub fn object_fk_for_node(node_id: &str) -> Option<(&'static str, &'static str)> {
    OBJECT_FK_BY_NODE
        .iter()
        .find(|(nid, _, _)| *nid == node_id)
        .map(|(_, tbl, col)| (*tbl, *col))
}

/// Look up table-specific FK for a given (table_name, column_name).
pub fn table_specific_fk(table_name: &str, col_name: &str) -> Option<(&'static str, &'static str)> {
    TABLE_SPECIFIC_FKS
        .iter()
        .find(|(src, col, _, _)| *src == table_name && *col == col_name)
        .map(|(_, _, tgt_tbl, tgt_col)| (*tgt_tbl, *tgt_col))
}
