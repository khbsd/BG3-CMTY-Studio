use std::fs::File;
use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::string::ToString;

use strum::{EnumCount, IntoEnumIterator, VariantArray};
use strum_macros::{Display, EnumIter};

use flate2::read::ZlibDecoder;
use lz4_flex::{block, frame::FrameDecoder};
use uuid::Uuid;

use crate::models::{LsxNode, LsxNodeAttribute, LsxRegion, LsxResource};
use crate::pak::format::PakCompression;
use crate::pak::reader::{
    bytes_read_f32, bytes_read_f64, bytes_read_i16, bytes_read_i32, bytes_read_i64, bytes_read_i8,
    bytes_read_u16, bytes_read_u32, bytes_read_u32_opt, bytes_read_u64, bytes_read_u8,
};

const LSF_SIGNATURE: u32 = u32::from_le_bytes(*b"LSOF");
const MIN_BG3_VERSION: u32 = 2;
const MAX_KNOWN_BG3_VERSION: u32 = 7;
const NAME_HASH_BUCKET_LIMIT: usize = 4096;
const MAX_SECTION_BYTES: usize = 256 * 1024 * 1024;
const TYPE_ID_MASK: u32 = 0x3f;
const TYPE_LENGTH_MASK: u32 = 6;

#[allow(non_camel_case_types)]
#[derive(Debug, EnumIter, Display, EnumCount, Clone, VariantArray, Copy)]
pub enum AttrTypes {
    None,
    uint8,
    int16,
    uint16,
    int32,
    uint32,
    float,
    double,
    ivec2,
    ivec3,
    ivec4,
    fvec2,
    fvec3,
    fvec4,
    mat2x2,
    mat3x3,
    mat3x4,
    mat4x3,
    mat4x4,
    bool,
    string,
    path,
    FixedString,
    LSString,
    uint64,
    ScratchBuffer,
    old_int64,
    int8,
    TranslatedString,
    WString,
    LSWString,
    guid,
    int64,
    TranslatedFSString,
}

pub enum MatrixExtract {
    Columns,
    Rows,
}

// Type::t as u32 and Type::t.to_string()

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LsfMetadataFormat {
    None,
    KeysAndAdjacency,
    None2,
    Unknown(u32),
}

impl LsfMetadataFormat {
    fn from_raw(raw: u32) -> Self {
        match raw {
            0 => Self::None,
            1 => Self::KeysAndAdjacency,
            2 => Self::None2,
            value => Self::Unknown(value),
        }
    }
}

#[derive(Debug, Clone)]
struct LsfMetadata {
    strings_uncompressed_size: u32,
    strings_size_on_disk: u32,
    keys_uncompressed_size: u32,
    keys_size_on_disk: u32,
    nodes_uncompressed_size: u32,
    nodes_size_on_disk: u32,
    attributes_uncompressed_size: u32,
    attributes_size_on_disk: u32,
    values_uncompressed_size: u32,
    values_size_on_disk: u32,
    compression_flags: u8,
    metadata_format: LsfMetadataFormat,
}

#[derive(Debug, Clone)]
struct LsfNodeInfo {
    parent_index: i32,
    name_index: usize,
    name_offset: usize,
    first_attribute_index: i32,
    key_attribute: Option<String>,
}

#[derive(Debug, Clone)]
struct LsfAttributeInfo {
    name_index: usize,
    name_offset: usize,
    type_id: AttrTypes,
    length: u32,
    data_offset: u32,
    next_attribute_index: i32,
}

#[derive(Debug)]
struct ArenaNode {
    id: String,
    key_attribute: Option<String>,
    attributes: Vec<LsxNodeAttribute>,
    children: Vec<usize>,
}

pub fn parse_lsf_file(path: &Path) -> Result<LsxResource, String> {
    let file = File::open(path).map_err(|e| format!("Failed to open {}: {}", path.display(), e))?;
    parse_lsf(file)
}

pub fn parse_lsf<R: Read + Seek>(mut reader: R) -> Result<LsxResource, String> {
    let file_len = reader
        .seek(SeekFrom::End(0))
        .map_err(|e| format!("Failed to inspect lsf stream: {e}"))?;
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|e| format!("Failed to rewind lsf stream: {e}"))?;

    if file_len < 16 {
        return Err("LSF file too small to contain a header".into());
    }

    let signature = bytes_read_u32(&mut reader)?;
    if signature != LSF_SIGNATURE {
        return Err(format!(
            "Incorrect signature in LSF file: expected {LSF_SIGNATURE:08X}, got {signature:08X}"
        ));
    }

    let version = bytes_read_u32(&mut reader)?;
    if !(MIN_BG3_VERSION..=MAX_KNOWN_BG3_VERSION).contains(&version) {
        return Err(format!("Unsupported BG3 LSF version {version}"));
    }

    if version >= 5 {
        let _engine_version = bytes_read_i64(&mut reader)?;
    } else {
        let _engine_version = bytes_read_i32(&mut reader)?;
    }

    let metadata = read_metadata(&mut reader, version)?;

    let names_bytes = read_section(
        &mut reader,
        metadata.strings_size_on_disk,
        metadata.strings_uncompressed_size,
        metadata.compression_flags,
        false,
    )?;
    let names = read_names(Cursor::new(names_bytes))?;

    let nodes_bytes = read_section(
        &mut reader,
        metadata.nodes_size_on_disk,
        metadata.nodes_uncompressed_size,
        metadata.compression_flags,
        true,
    )?;
    let nodes_section_len = nodes_bytes.len();
    let has_adjacency =
        version >= 3 && metadata.metadata_format == LsfMetadataFormat::KeysAndAdjacency;
    let mut nodes = read_nodes(Cursor::new(nodes_bytes), &names, has_adjacency)?;

    let node_record_size = if has_adjacency { 16usize } else { 12 };
    let expected_nodes_bytes = nodes.len() * node_record_size;
    if expected_nodes_bytes != nodes_section_len {
        return Err(format!(
            "LSF node section size mismatch: expected {} bytes for {} nodes, but section contains {} bytes (possible truncation)",
            expected_nodes_bytes, nodes.len(), nodes_section_len
        ));
    }

    let attributes_bytes = read_section(
        &mut reader,
        metadata.attributes_size_on_disk,
        metadata.attributes_uncompressed_size,
        metadata.compression_flags,
        true,
    )?;
    let attrs_section_len = attributes_bytes.len();
    let attributes = if has_adjacency {
        read_attributes_v3(Cursor::new(attributes_bytes), &names)?
    } else {
        read_attributes_v2(Cursor::new(attributes_bytes), &names)?
    };

    let attr_record_size = if has_adjacency { 16usize } else { 12 };
    let expected_attrs_bytes = attributes.len() * attr_record_size;
    if expected_attrs_bytes != attrs_section_len {
        return Err(format!(
            "LSF attribute section size mismatch: expected {} bytes for {} attributes, but section contains {} bytes (possible truncation)",
            expected_attrs_bytes, attributes.len(), attrs_section_len
        ));
    }

    let values = read_section(
        &mut reader,
        metadata.values_size_on_disk,
        metadata.values_uncompressed_size,
        metadata.compression_flags,
        true,
    )?;

    if metadata.metadata_format == LsfMetadataFormat::KeysAndAdjacency {
        let keys_bytes = read_section(
            &mut reader,
            metadata.keys_size_on_disk,
            metadata.keys_uncompressed_size,
            metadata.compression_flags,
            true,
        )?;
        read_keys(Cursor::new(keys_bytes), &names, &mut nodes)?;
    }

    build_resource(&names, &nodes, &attributes, &values)
}

fn read_metadata<R: Read>(reader: &mut R, version: u32) -> Result<LsfMetadata, String> {
    let strings_uncompressed_size = bytes_read_u32(reader)?;
    let strings_size_on_disk = bytes_read_u32(reader)?;

    let (keys_uncompressed_size, keys_size_on_disk) = if version >= 6 {
        (bytes_read_u32(reader)?, bytes_read_u32(reader)?)
    } else {
        (0, 0)
    };

    let nodes_uncompressed_size = bytes_read_u32(reader)?;
    let nodes_size_on_disk = bytes_read_u32(reader)?;
    let attributes_uncompressed_size = bytes_read_u32(reader)?;
    let attributes_size_on_disk = bytes_read_u32(reader)?;
    let values_uncompressed_size = bytes_read_u32(reader)?;
    let values_size_on_disk = bytes_read_u32(reader)?;
    let compression_flags = bytes_read_u8(reader)?;
    let _unknown2 = bytes_read_u8(reader)?;
    let _unknown3 = bytes_read_u16(reader)?;
    let metadata_format = LsfMetadataFormat::from_raw(bytes_read_u32(reader)?);

    Ok(LsfMetadata {
        strings_uncompressed_size,
        strings_size_on_disk,
        keys_uncompressed_size,
        keys_size_on_disk,
        nodes_uncompressed_size,
        nodes_size_on_disk,
        attributes_uncompressed_size,
        attributes_size_on_disk,
        values_uncompressed_size,
        values_size_on_disk,
        compression_flags,
        metadata_format,
    })
}

fn read_section<R: Read>(
    reader: &mut R,
    size_on_disk: u32,
    uncompressed_size: u32,
    compression_flags: u8,
    allow_chunked: bool,
) -> Result<Vec<u8>, String> {
    if uncompressed_size as usize > MAX_SECTION_BYTES {
        return Err(format!(
            "LSF section exceeds size limit: {uncompressed_size} bytes"
        ));
    }

    if size_on_disk == 0 && uncompressed_size == 0 {
        return Ok(Vec::new());
    }

    if size_on_disk == 0 {
        let mut bytes = vec![0u8; uncompressed_size as usize];
        reader
            .read_exact(&mut bytes)
            .map_err(|e| format!("Failed to read uncompressed LSF section: {e}"))?;
        return Ok(bytes);
    }

    let compression = PakCompression::from_method_bits(compression_flags);
    let compressed_len = size_on_disk as usize;

    let mut compressed = vec![0u8; compressed_len];
    reader
        .read_exact(&mut compressed)
        .map_err(|e| format!("Failed to read compressed LSF section: {e}"))?;

    match compression {
        PakCompression::None => Ok(compressed),
        PakCompression::Zlib => {
            let mut decoder = ZlibDecoder::new(Cursor::new(compressed));
            let mut output = Vec::with_capacity(uncompressed_size as usize);
            decoder
                .read_to_end(&mut output)
                .map_err(|e| format!("Failed to decompress LSF zlib section: {e}"))?;
            Ok(output)
        }
        PakCompression::Lz4 => {
            if allow_chunked {
                let mut decoder = FrameDecoder::new(Cursor::new(compressed));
                let mut output = Vec::with_capacity(uncompressed_size as usize);
                decoder
                    .read_to_end(&mut output)
                    .map_err(|e| format!("Failed to decompress LSF LZ4 frame section: {e}"))?;
                Ok(output)
            } else {
                block::decompress(&compressed, uncompressed_size as usize)
                    .map_err(|e| format!("Failed to decompress LSF LZ4 block section: {e}"))
            }
        }
        PakCompression::Zstd => Err("Zstd-compressed LSF sections are not implemented yet".into()),
        PakCompression::Unknown(value) => Err(format!("Unknown LSF compression method {value}")),
    }
}

fn read_names<R: Read>(mut reader: R) -> Result<Vec<Vec<String>>, String> {
    let bucket_count = bytes_read_u32(&mut reader)? as usize;
    if bucket_count > NAME_HASH_BUCKET_LIMIT {
        return Err(format!(
            "LSF name table bucket count too large: {bucket_count}"
        ));
    }

    let mut names = Vec::with_capacity(bucket_count);
    for _ in 0..bucket_count {
        let string_count = bytes_read_u16(&mut reader)? as usize;
        let mut bucket = Vec::with_capacity(string_count);
        for _ in 0..string_count {
            let len = bytes_read_u16(&mut reader)? as usize;
            let mut bytes = vec![0u8; len];
            reader
                .read_exact(&mut bytes)
                .map_err(|e| format!("Failed to read LSF name bytes: {e}"))?;
            let value = String::from_utf8(bytes)
                .map_err(|e| format!("Invalid UTF-8 in LSF name table: {e}"))?;
            bucket.push(value);
        }
        names.push(bucket);
    }

    Ok(names)
}

fn read_nodes<R: Read>(
    mut reader: R,
    names: &[Vec<String>],
    long_nodes: bool,
) -> Result<Vec<LsfNodeInfo>, String> {
    let mut nodes = Vec::new();

    while let Some(name_hash) = bytes_read_u32_opt(&mut reader)? {
        let (parent_index, first_attribute_index) = if long_nodes {
            let parent_index = bytes_read_i32(&mut reader)?;
            let _next_sibling_index = bytes_read_i32(&mut reader)?;
            let first_attribute_index = bytes_read_i32(&mut reader)?;
            (parent_index, first_attribute_index)
        } else {
            let first_attribute_index = bytes_read_i32(&mut reader)?;
            let parent_index = bytes_read_i32(&mut reader)?;
            (parent_index, first_attribute_index)
        };

        let (name_index, name_offset) = split_name_hash(name_hash);
        ensure_name_exists(names, name_index, name_offset)?;

        nodes.push(LsfNodeInfo {
            parent_index,
            name_index,
            name_offset,
            first_attribute_index,
            key_attribute: None,
        });
    }

    Ok(nodes)
}

fn read_attributes_v2<R: Read>(
    mut reader: R,
    names: &[Vec<String>],
) -> Result<Vec<LsfAttributeInfo>, String> {
    let mut attributes: Vec<LsfAttributeInfo> = Vec::new();
    let mut prev_attribute_refs: Vec<i32> = Vec::new();
    let mut data_offset = 0u32;
    let types = AttrTypes::VARIANTS;

    while let Some(name_hash) = bytes_read_u32_opt(&mut reader)? {
        let type_and_length = bytes_read_u32(&mut reader)?;
        let node_index = bytes_read_i32(&mut reader)?;
        let (name_index, name_offset) = split_name_hash(name_hash);
        ensure_name_exists(names, name_index, name_offset)?;

        let index = attributes.len() as i32;
        let resolved = LsfAttributeInfo {
            name_index,
            name_offset,
            type_id: types[(type_and_length & 0x3f) as usize],
            length: type_and_length >> 6,
            data_offset,
            next_attribute_index: -1,
        };

        let chain_index = node_index + 1;
        if chain_index < 0 {
            return Err(format!(
                "Invalid negative node index {node_index} in LSF attribute table"
            ));
        }

        let chain_index = chain_index as usize;
        if prev_attribute_refs.len() <= chain_index {
            prev_attribute_refs.resize(chain_index + 1, -1);
        }
        if prev_attribute_refs[chain_index] != -1 {
            attributes[prev_attribute_refs[chain_index] as usize].next_attribute_index = index;
        }
        prev_attribute_refs[chain_index] = index;

        data_offset = data_offset
            .checked_add(resolved.length)
            .ok_or_else(|| "LSF attribute value offset overflowed".to_string())?;
        attributes.push(resolved);
    }

    Ok(attributes)
}

fn read_attributes_v3<R: Read>(
    mut reader: R,
    names: &[Vec<String>],
) -> Result<Vec<LsfAttributeInfo>, String> {
    let mut attributes = Vec::new();
    let types = AttrTypes::VARIANTS;

    while let Some(name_hash) = bytes_read_u32_opt(&mut reader)? {
        let type_and_length = bytes_read_u32(&mut reader)?;
        let next_attribute_index = bytes_read_i32(&mut reader)?;
        let data_offset = bytes_read_u32(&mut reader)?;
        let (name_index, name_offset) = split_name_hash(name_hash);
        ensure_name_exists(names, name_index, name_offset)?;

        attributes.push(LsfAttributeInfo {
            name_index,
            name_offset,
            type_id: types[(type_and_length & 0x3f) as usize],
            length: type_and_length >> 6,
            data_offset,
            next_attribute_index,
        });
    }

    Ok(attributes)
}

fn read_keys<R: Read>(
    mut reader: R,
    names: &[Vec<String>],
    nodes: &mut [LsfNodeInfo],
) -> Result<(), String> {
    while let Some(node_index_raw) = bytes_read_u32_opt(&mut reader)? {
        let node_index = node_index_raw as usize;
        let key_name = bytes_read_u32(&mut reader)?;
        let (name_index, name_offset) = split_name_hash(key_name);
        let key = resolve_name(names, name_index, name_offset)?.to_string();

        let node = nodes
            .get_mut(node_index)
            .ok_or_else(|| format!("LSF key references missing node index {node_index}"))?;
        node.key_attribute = Some(key);
    }

    Ok(())
}

fn build_resource(
    names: &[Vec<String>],
    nodes: &[LsfNodeInfo],
    attributes: &[LsfAttributeInfo],
    values: &[u8],
) -> Result<LsxResource, String> {
    let mut arena: Vec<ArenaNode> = Vec::with_capacity(nodes.len());
    let mut region_roots = Vec::new();

    for node in nodes {
        let id = resolve_name(names, node.name_index, node.name_offset)?.to_string();
        let attributes = read_node_attributes(names, node, attributes, values)?;
        arena.push(ArenaNode {
            id,
            key_attribute: node.key_attribute.clone(),
            attributes,
            children: Vec::new(),
        });
        let index = arena.len() - 1;

        if node.parent_index < 0 {
            region_roots.push(index);
        } else {
            let parent = arena.get_mut(node.parent_index as usize).ok_or_else(|| {
                format!(
                    "LSF node references missing parent index {}",
                    node.parent_index
                )
            })?;
            parent.children.push(index);
        }
    }

    let mut regions = Vec::with_capacity(region_roots.len());
    for root_index in region_roots {
        let root = arena_to_node(&arena, root_index);
        let region_nodes = if root.id == "root" {
            root.children.clone()
        } else {
            vec![root.clone()]
        };
        regions.push(LsxRegion {
            id: root.id,
            nodes: region_nodes,
        });
    }

    Ok(LsxResource { regions })
}

fn read_node_attributes(
    names: &[Vec<String>],
    node: &LsfNodeInfo,
    attributes: &[LsfAttributeInfo],
    values: &[u8],
) -> Result<Vec<LsxNodeAttribute>, String> {
    let mut parsed = Vec::new();
    let mut next_index = node.first_attribute_index;

    while next_index != -1 {
        let info = attributes
            .get(next_index as usize)
            .ok_or_else(|| format!("LSF node references missing attribute index {next_index}"))?;
        parsed.push(read_attribute_value(names, info, values)?);
        next_index = info.next_attribute_index;
    }

    Ok(parsed)
}

fn read_attribute_value(
    names: &[Vec<String>],
    info: &LsfAttributeInfo,
    values: &[u8],
) -> Result<LsxNodeAttribute, String> {
    let start = info.data_offset as usize;
    let end = start
        .checked_add(info.length as usize)
        .ok_or_else(|| "LSF attribute slice overflowed".to_string())?;
    if end > values.len() {
        return Err(format!(
            "LSF attribute value exceeds values section: {}..{} of {}",
            start,
            end,
            values.len()
        ));
    }

    let mut cursor = Cursor::new(&values[start..end]);
    let attr_type: String = info.type_id.to_string();
    let (value, handle, version, arguments) =
        read_typed_value(&mut cursor, info.type_id, info.length)?;

    Ok(LsxNodeAttribute {
        id: resolve_name(names, info.name_index, info.name_offset)?.to_string(),
        attr_type: attr_type.to_string(),
        value,
        handle,
        version,
        arguments,
    })
}

#[allow(clippy::type_complexity)]
fn read_typed_value<R: Read>(
    reader: &mut R,
    type_id: AttrTypes,
    length: u32,
) -> Result<
    (
        String,
        Option<String>,
        Option<u16>,
        Vec<crate::models::LsxTranslatedFsArgument>,
    ),
    String,
> {
    match type_id {
        AttrTypes::None => Ok((String::new(), None, None, Vec::new())),
        AttrTypes::uint8 => Ok((bytes_read_u8(reader)?.to_string(), None, None, Vec::new())),
        AttrTypes::int16 => Ok((bytes_read_i16(reader)?.to_string(), None, None, Vec::new())),
        AttrTypes::uint16 => Ok((bytes_read_u16(reader)?.to_string(), None, None, Vec::new())),
        AttrTypes::int32 => Ok((bytes_read_i32(reader)?.to_string(), None, None, Vec::new())),
        AttrTypes::uint32 => Ok((bytes_read_u32(reader)?.to_string(), None, None, Vec::new())),
        AttrTypes::float => Ok((bytes_read_f32(reader)?.to_string(), None, None, Vec::new())),
        AttrTypes::double => Ok((bytes_read_f64(reader)?.to_string(), None, None, Vec::new())),
        AttrTypes::ivec2 | AttrTypes::ivec3 | AttrTypes::ivec4 => Ok((
            bytes_read_i32_vector(reader, vector_columns(type_id)?)?.join(" "),
            None,
            None,
            Vec::new(),
        )),
        AttrTypes::fvec2 | AttrTypes::fvec3 | AttrTypes::fvec4 => Ok((
            bytes_read_f32_vector(reader, vector_columns(type_id)?)?.join(" "),
            None,
            None,
            Vec::new(),
        )),
        AttrTypes::mat2x2
        | AttrTypes::mat3x3
        | AttrTypes::mat3x4
        | AttrTypes::mat4x3
        | AttrTypes::mat4x4 => Ok((
            bytes_read_f32_vector(reader, matrix_rows(type_id)? * matrix_columns(type_id)?)?
                .join(" "),
            None,
            None,
            Vec::new(),
        )),
        AttrTypes::bool => Ok((
            if bytes_read_u8(reader)? != 0 {
                "True"
            } else {
                "False"
            }
            .to_string(),
            None,
            None,
            Vec::new(),
        )),
        AttrTypes::string | AttrTypes::path | AttrTypes::FixedString | AttrTypes::LSString => Ok((
            read_lsf_utf8_string(reader, length as usize)?,
            None,
            None,
            Vec::new(),
        )),
        AttrTypes::uint64 => Ok((bytes_read_u64(reader)?.to_string(), None, None, Vec::new())),
        AttrTypes::ScratchBuffer => Ok((
            bytes_to_hex(&read_bytes_exact(reader, length as usize)?),
            None,
            None,
            Vec::new(),
        )),
        AttrTypes::old_int64 | AttrTypes::int64 => {
            Ok((bytes_read_i64(reader)?.to_string(), None, None, Vec::new()))
        }
        AttrTypes::int8 => Ok((bytes_read_i8(reader)?.to_string(), None, None, Vec::new())),
        AttrTypes::TranslatedString => {
            let (handle, version) = read_translated_string_payload(reader)?;
            Ok((handle.clone(), Some(handle), Some(version), Vec::new()))
        }
        AttrTypes::WString | AttrTypes::LSWString => Ok((
            read_lsf_wide_string(reader, length as usize)?,
            None,
            None,
            Vec::new(),
        )),
        AttrTypes::guid => {
            let bytes = read_bytes_exact(reader, 16)?;
            let guid = read_lsf_guid(&bytes)?;
            Ok((guid.to_string(), None, None, Vec::new()))
        }
        AttrTypes::TranslatedFSString => {
            let (handle, version) = read_translated_string_payload(reader)?;
            let arg_count = bytes_read_i32(reader)?;
            let mut arguments = Vec::with_capacity(arg_count.max(0) as usize);
            for _ in 0..arg_count.max(0) {
                let key_len = bytes_read_i32(reader)?;
                let key = read_lsf_utf8_string(reader, key_len.max(0) as usize)?;
                let (nested_handle, nested_version) = read_translated_string_payload(reader)?;
                let value_len = bytes_read_i32(reader)?;
                let value = read_lsf_utf8_string(reader, value_len.max(0) as usize)?;
                arguments.push(crate::models::LsxTranslatedFsArgument {
                    key,
                    string: Box::new(LsxNodeAttribute {
                        id: String::new(),
                        attr_type: "TranslatedString".to_string(),
                        value: nested_handle.clone(),
                        handle: Some(nested_handle),
                        version: Some(nested_version),
                        arguments: Vec::new(),
                    }),
                    value,
                });
            }
            Ok((handle.clone(), Some(handle), Some(version), arguments))
        }
        //_ => Err(format!("Unsupported LSF attribute type {type_id}")),
    }
}

fn arena_to_node(arena: &[ArenaNode], index: usize) -> LsxNode {
    let node = &arena[index];
    LsxNode {
        id: node.id.clone(),
        key_attribute: node.key_attribute.clone(),
        attributes: node.attributes.clone(),
        children: node
            .children
            .iter()
            .map(|child| arena_to_node(arena, *child))
            .collect(),
        commented: false,
    }
}

fn split_name_hash(raw: u32) -> (usize, usize) {
    ((raw >> 16) as usize, (raw & 0xFFFF) as usize)
}

fn ensure_name_exists(
    names: &[Vec<String>],
    name_index: usize,
    name_offset: usize,
) -> Result<(), String> {
    let _ = resolve_name(names, name_index, name_offset)?;
    Ok(())
}

fn resolve_name(
    names: &[Vec<String>],
    name_index: usize,
    name_offset: usize,
) -> Result<&str, String> {
    names
        .get(name_index)
        .and_then(|bucket| bucket.get(name_offset))
        .map(|value| value.as_str())
        .ok_or_else(|| format!("Missing LSF name table entry {name_index}/{name_offset}"))
}

fn vector_columns(type_id: AttrTypes) -> Result<usize, String> {
    match type_id {
        AttrTypes::ivec2 | AttrTypes::fvec2 => Ok(2),
        AttrTypes::ivec3 | AttrTypes::fvec3 => Ok(3),
        AttrTypes::ivec4 | AttrTypes::fvec4 => Ok(4),
        _ => Err(format!("Attribute type {type_id} is not a vector")),
    }
}

fn matrix_rows(type_id: AttrTypes) -> Result<usize, String> {
    match type_id {
        AttrTypes::mat2x2 => Ok(2),
        AttrTypes::mat3x3 | AttrTypes::mat3x4 => Ok(3),
        AttrTypes::mat4x3 | AttrTypes::mat4x4 => Ok(4),
        _ => Err(format!("Attribute type {type_id} is not a matrix")),
    }
}

fn matrix_columns(type_id: AttrTypes) -> Result<usize, String> {
    match type_id {
        AttrTypes::mat2x2 => Ok(2),
        AttrTypes::mat3x3 | AttrTypes::mat4x3 => Ok(3),
        AttrTypes::mat3x4 | AttrTypes::mat4x4 => Ok(4),
        _ => Err(format!("Attribute type {type_id} is not a matrix")),
    }
}

fn bytes_read_i32_vector<R: Read>(reader: &mut R, count: usize) -> Result<Vec<String>, String> {
    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        values.push(bytes_read_i32(reader)?.to_string());
    }
    Ok(values)
}

fn bytes_read_f32_vector<R: Read>(reader: &mut R, count: usize) -> Result<Vec<String>, String> {
    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        values.push(bytes_read_f32(reader)?.to_string());
    }
    Ok(values)
}

fn read_bytes_exact<R: Read>(reader: &mut R, len: usize) -> Result<Vec<u8>, String> {
    let mut bytes = vec![0u8; len];
    reader
        .read_exact(&mut bytes)
        .map_err(|e| format!("Failed to read LSF bytes: {e}"))?;
    Ok(bytes)
}

fn read_lsf_wide_string<R: Read>(reader: &mut R, len: usize) -> Result<String, String> {
    let bytes = read_bytes_exact(reader, len)?;

    let looks_utf16 = bytes.len() % 2 == 0
        && bytes.chunks_exact(2).any(|chunk| chunk[1] == 0)
        && !bytes.is_empty();

    if looks_utf16 {
        let mut words: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect();

        while words.last().copied() == Some(0) {
            words.pop();
        }

        return String::from_utf16(&words).map_err(|e| format!("Invalid UTF-16 in LSF value: {e}"));
    }

    let trimmed_len = bytes
        .iter()
        .rposition(|byte| *byte != 0)
        .map(|index| index + 1)
        .unwrap_or(0);
    String::from_utf8(bytes[..trimmed_len].to_vec())
        .map_err(|e| format!("Invalid UTF-8 in LSF wide string value: {e}"))
}

fn read_translated_string_payload<R: Read>(reader: &mut R) -> Result<(String, u16), String> {
    let version = bytes_read_u16(reader)?;
    let handle_len = bytes_read_i32(reader)?;
    let handle = read_lsf_utf8_string(reader, handle_len.max(0) as usize)?;
    Ok((handle, version))
}

fn read_lsf_utf8_string<R: Read>(reader: &mut R, len: usize) -> Result<String, String> {
    let bytes = read_bytes_exact(reader, len)?;
    let trimmed_len = bytes
        .iter()
        .rposition(|byte| *byte != 0)
        .map(|index| index + 1)
        .unwrap_or(0);
    String::from_utf8(bytes[..trimmed_len].to_vec())
        .map_err(|e| format!("Invalid UTF-8 in LSF value: {e}"))
}

fn read_lsf_guid(bytes: &[u8]) -> Result<Uuid, String> {
    if bytes.len() != 16 {
        return Err(format!("Invalid UUID byte length {}", bytes.len()));
    }

    let mut swapped = [0u8; 16];
    swapped.copy_from_slice(bytes);
    for index in (8..16).step_by(2) {
        swapped.swap(index, index + 1);
    }

    Ok(Uuid::from_bytes_le(swapped))
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

// ═══════════════════════════════════════════════════════════════════
//  LsxResource → binary LSF writer
// ═══════════════════════════════════════════════════════════════════

const WRITE_VERSION: u32 = 7;

/// Pack a BG3 engine version into the i64 format used by LSlib/divine (v5+ header).
/// Layout: major(bits 55-62) | minor(bits 47-54) | revision(bits 31-46) | build(bits 0-30)
const fn pack_engine_version(major: u32, minor: u32, revision: u32, build: u32) -> i64 {
    ((major as i64) << 55) | ((minor as i64) << 47) | ((revision as i64) << 31) | (build as i64)
}

/// Default engine version: 4.0.9.328 (BG3 Patch 7 HF6)
const WRITE_ENGINE_VERSION: i64 = pack_engine_version(4, 0, 9, 328);

/// Write an `LsxResource` as a BG3 binary `.lsf` / `.lsfx` file.
///
/// Uses version 7 with no compression and `MetadataFormat::None` (V2 12-byte
/// node/attribute records), matching divine.exe / LSlib output.
pub fn write_lsf<W: Write>(writer: &mut W, resource: &LsxResource) -> Result<(), String> {
    // ── 1. Flatten the tree into parallel node/attribute/value arrays ───

    let mut flat_nodes: Vec<FlatNode> = Vec::new();
    let mut flat_attrs: Vec<FlatAttr> = Vec::new();
    let mut values_buf: Vec<u8> = Vec::new();
    let mut name_table = NameTable::new();

    for region in &resource.regions {
        for node in &region.nodes {
            flatten_node(
                node,
                -1,
                &mut flat_nodes,
                &mut flat_attrs,
                &mut values_buf,
                &mut name_table,
            )?;
        }
    }

    // Wire up first_attribute_index for each node
    for (attr_idx, attr) in flat_attrs.iter().enumerate() {
        let node = &mut flat_nodes[attr.owner_node as usize];
        if node.first_attribute_index == -1 {
            node.first_attribute_index = attr_idx as i32;
        }
    }

    // ── 2. Serialize sections (V2 format — 12-byte records, no adjacency) ──

    let names_raw = name_table.serialize();
    let nodes_raw = serialize_nodes_v2(&flat_nodes);
    let attrs_raw = serialize_attrs_v2(&flat_attrs);
    let values_raw = values_buf;

    // ── 3. Write header ────────────────────────────────────────────

    write_u32(writer, LSF_SIGNATURE)?;
    write_u32(writer, WRITE_VERSION)?;
    write_i64(writer, WRITE_ENGINE_VERSION)?;

    // Metadata (LSFMetadataV6 layout — uncompressed, no keys, no adjacency)
    write_u32(writer, names_raw.len() as u32)?; // strings_uncompressed
    write_u32(writer, 0)?; // strings_size_on_disk (0 = uncompressed)
    write_u32(writer, 0)?; // keys_uncompressed
    write_u32(writer, 0)?; // keys_size_on_disk
    write_u32(writer, nodes_raw.len() as u32)?; // nodes_uncompressed
    write_u32(writer, 0)?; // nodes_size_on_disk
    write_u32(writer, attrs_raw.len() as u32)?; // attrs_uncompressed
    write_u32(writer, 0)?; // attrs_size_on_disk
    write_u32(writer, values_raw.len() as u32)?; // values_uncompressed
    write_u32(writer, 0)?; // values_size_on_disk
    write_u8_val(writer, 0)?; // compression_flags = None
    write_u8_val(writer, 0)?; // unknown2
    write_u16_val(writer, 0)?; // unknown3
    write_u32(writer, 0)?; // metadata_format = None

    // Sections (order: names, nodes, attrs, values — all raw, no keys)
    writer
        .write_all(&names_raw)
        .map_err(|e| format!("Failed to write names: {e}"))?;
    writer
        .write_all(&nodes_raw)
        .map_err(|e| format!("Failed to write nodes: {e}"))?;
    writer
        .write_all(&attrs_raw)
        .map_err(|e| format!("Failed to write attrs: {e}"))?;
    writer
        .write_all(&values_raw)
        .map_err(|e| format!("Failed to write values: {e}"))?;

    Ok(())
}

pub fn write_lsf_file(path: &Path, resource: &LsxResource) -> Result<(), String> {
    let file = std::fs::File::create(path)
        .map_err(|e| format!("Failed to create {}: {}", path.display(), e))?;
    let mut buf = std::io::BufWriter::new(file);
    write_lsf(&mut buf, resource)?;
    buf.flush()
        .map_err(|e| format!("Failed to flush {}: {}", path.display(), e))?;
    Ok(())
}

// ── Flattened representations ──────────────────────────────────────

struct FlatNode {
    name_hash: u32,
    parent_index: i32,
    first_attribute_index: i32,
}

struct FlatAttr {
    name_hash: u32,
    type_id: AttrTypes,
    owner_node: i32,
    data_length: u32,
}

// ── Name table builder (512-bucket hash map matching LSlib/divine) ──

/// Number of hash buckets for the string table, matching LSlib's `StringHashMapSize`.
const NAME_HASH_BUCKET_COUNT: usize = 0x200; // 512

struct NameTable {
    /// 512-entry hash map: each bucket holds a list of (string, global_index) pairs.
    buckets: Vec<Vec<String>>,
    /// Maps string → packed name_hash (bucket << 16 | offset)
    lookup: std::collections::HashMap<String, u32>,
}

impl NameTable {
    fn new() -> Self {
        Self {
            buckets: vec![Vec::new(); NAME_HASH_BUCKET_COUNT],
            lookup: std::collections::HashMap::new(),
        }
    }

    /// Intern a string and return its name_hash (bucket_index << 16 | offset).
    fn intern(&mut self, name: &str) -> u32 {
        if let Some(&hash) = self.lookup.get(name) {
            return hash;
        }
        let bucket = Self::compute_bucket(name);
        let offset = self.buckets[bucket].len();
        self.buckets[bucket].push(name.to_string());
        let hash = ((bucket as u32) << 16) | (offset as u32);
        self.lookup.insert(name.to_string(), hash);
        hash
    }

    /// Hash a string name to a bucket index (0..511).
    /// Uses FNV-1a then folds to 9 bits via LSlib's xor-fold pattern.
    fn compute_bucket(name: &str) -> usize {
        let mut h: u32 = 0x811c_9dc5; // FNV offset basis
        for b in name.as_bytes() {
            h ^= *b as u32;
            h = h.wrapping_mul(0x0100_0193); // FNV prime
        }
        ((h & 0x1ff) ^ ((h >> 9) & 0x1ff) ^ ((h >> 18) & 0x1ff) ^ ((h >> 27) & 0x1ff)) as usize
    }

    /// Serialize as the name table binary format:
    /// bucket_count (u32), then for each bucket: string_count (u16),
    /// then for each string: len (u16) + bytes.
    fn serialize(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(NAME_HASH_BUCKET_COUNT as u32).to_le_bytes());
        for bucket in &self.buckets {
            buf.extend_from_slice(&(bucket.len() as u16).to_le_bytes());
            for s in bucket {
                let bytes = s.as_bytes();
                buf.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
                buf.extend_from_slice(bytes);
            }
        }
        buf
    }
}

// ── Tree flattening ────────────────────────────────────────────────

fn flatten_node(
    node: &LsxNode,
    parent_index: i32,
    flat_nodes: &mut Vec<FlatNode>,
    flat_attrs: &mut Vec<FlatAttr>,
    values_buf: &mut Vec<u8>,
    names: &mut NameTable,
) -> Result<(), String> {
    let my_index = flat_nodes.len() as i32;
    flat_nodes.push(FlatNode {
        name_hash: names.intern(&node.id),
        parent_index,
        first_attribute_index: -1,
    });

    // Serialize attributes
    for attr in &node.attributes {
        let type_id = type_name_to_id(&attr.attr_type)?;
        let len_before = values_buf.len() as u32;
        write_typed_value(values_buf, type_id, attr)?;
        let data_length = values_buf.len() as u32 - len_before;

        flat_attrs.push(FlatAttr {
            name_hash: names.intern(&attr.id),
            type_id: type_id,
            owner_node: my_index,
            data_length,
        });
    }

    // Recurse into children
    for child in &node.children {
        flatten_node(child, my_index, flat_nodes, flat_attrs, values_buf, names)?;
    }

    Ok(())
}

// ── Section serializers (V2 — 12-byte records, no adjacency) ───────

/// V2 node entry: name_hash (u32), first_attribute_index (i32), parent_index (i32) = 12 bytes.
/// Note: field order differs from V3 — first_attribute comes before parent in V2.
fn serialize_nodes_v2(nodes: &[FlatNode]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(nodes.len() * 12);
    for node in nodes {
        buf.extend_from_slice(&node.name_hash.to_le_bytes());
        buf.extend_from_slice(&node.first_attribute_index.to_le_bytes());
        buf.extend_from_slice(&node.parent_index.to_le_bytes());
    }
    buf
}

/// V2 attribute entry: name_hash (u32), type_and_length (u32), node_index (i32) = 12 bytes.
/// The owning node index lets the reader reconstruct which attributes belong to which node.
fn serialize_attrs_v2(attrs: &[FlatAttr]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(attrs.len() * 12);
    for attr in attrs {
        let type_and_length =
            ((attr.type_id as u32) & TYPE_ID_MASK) | (attr.data_length << TYPE_LENGTH_MASK);
        buf.extend_from_slice(&attr.name_hash.to_le_bytes());
        buf.extend_from_slice(&type_and_length.to_le_bytes());
        buf.extend_from_slice(&attr.owner_node.to_le_bytes());
    }
    buf
}

// ── Value serializer ───────────────────────────────────────────────

fn write_typed_value(
    buf: &mut Vec<u8>,
    type_id: AttrTypes,
    attr: &LsxNodeAttribute,
) -> Result<(), String> {
    let error_type_string = type_id.to_string();
    match type_id {
        AttrTypes::None => {} // None
        AttrTypes::uint8 => {
            // uint8

            let v: u8 = attr
                .value
                .parse()
                .map_err(|e| format!("{error_type_string} parse: {e}"))?;
            buf.push(v);
        }
        AttrTypes::int16 => {
            // int16
            let v: i16 = attr
                .value
                .parse()
                .map_err(|e| format!("{error_type_string} parse: {e}"))?;
            buf.extend_from_slice(&v.to_le_bytes());
        }
        AttrTypes::uint16 => {
            // uint16
            let v: u16 = attr
                .value
                .parse()
                .map_err(|e| format!("{error_type_string} parse: {e}"))?;
            buf.extend_from_slice(&v.to_le_bytes());
        }
        AttrTypes::int32 => {
            // int32
            let v: i32 = attr
                .value
                .parse()
                .map_err(|e| format!("{error_type_string} parse: {e}"))?;
            buf.extend_from_slice(&v.to_le_bytes());
        }
        AttrTypes::uint32 => {
            // uint32
            let v: u32 = attr
                .value
                .parse()
                .map_err(|e| format!("{error_type_string} parse: {e}"))?;
            buf.extend_from_slice(&v.to_le_bytes());
        }
        AttrTypes::float => {
            // float
            let v: f32 = attr
                .value
                .parse()
                .map_err(|e| format!("{error_type_string} parse: {e}"))?;
            buf.extend_from_slice(&v.to_le_bytes());
        }
        AttrTypes::double => {
            // double
            let v: f64 = attr
                .value
                .parse()
                .map_err(|e| format!("{error_type_string} parse: {e}"))?;
            buf.extend_from_slice(&v.to_le_bytes());
        }
        AttrTypes::ivec2 | AttrTypes::ivec3 | AttrTypes::ivec4 => {
            // ivec2/3/4
            let cols = match type_id {
                AttrTypes::ivec2 => 2,
                AttrTypes::ivec3 => 3,
                AttrTypes::ivec4 | _ => 4,
            };
            write_i32_values(buf, &attr.value, cols)?;
        }
        AttrTypes::fvec2 | AttrTypes::fvec3 | AttrTypes::fvec4 => {
            // fvec2/3/4
            let cols = match type_id {
                AttrTypes::fvec2 => 2,
                AttrTypes::fvec3 => 3,
                AttrTypes::fvec4 | _ => 4,
            };
            write_f32_values(buf, &attr.value, cols)?;
        }
        AttrTypes::mat2x2
        | AttrTypes::mat3x3
        | AttrTypes::mat3x4
        | AttrTypes::mat4x3
        | AttrTypes::mat4x4 => {
            // matrices
            let total = match type_id {
                AttrTypes::mat2x2 => 4,      // 2x2
                AttrTypes::mat3x3 => 9,      // 3x3
                AttrTypes::mat3x4 => 12,     // 3x4
                AttrTypes::mat4x3 => 12,     // 4x3
                AttrTypes::mat4x4 | _ => 16, // 4x4
            };
            write_f32_values(buf, &attr.value, total)?;
        }
        AttrTypes::bool => {
            // bool
            let v: u8 = if attr.value == "True" || attr.value == "true" || attr.value == "1" {
                1
            } else {
                0
            };
            buf.push(v);
        }
        AttrTypes::string | AttrTypes::path | AttrTypes::FixedString | AttrTypes::LSString => {
            // string / path / FixedString / LSString
            write_lsf_string(buf, &attr.value);
        }
        AttrTypes::uint64 => {
            // uint64
            let v: u64 = attr
                .value
                .parse()
                .map_err(|e| format!("{error_type_string} parse: {e}"))?;
            buf.extend_from_slice(&v.to_le_bytes());
        }
        AttrTypes::ScratchBuffer => {
            // ScratchBuffer
            let bytes = hex_to_bytes(&attr.value)?;
            buf.extend_from_slice(&bytes);
        }
        AttrTypes::old_int64 | AttrTypes::int64 => {
            // old_int64 / int64
            let v: i64 = attr
                .value
                .parse()
                .map_err(|e| format!("{error_type_string} parse: {e}"))?;
            buf.extend_from_slice(&v.to_le_bytes());
        }
        AttrTypes::int8 => {
            // int8
            let v: i8 = attr
                .value
                .parse()
                .map_err(|e| format!("{error_type_string} parse: {e}"))?;
            buf.extend_from_slice(&v.to_le_bytes());
        }
        AttrTypes::TranslatedString => {
            // TranslatedString
            let handle = attr.handle.as_deref().unwrap_or(&attr.value);
            let version = attr.version.unwrap_or(1);
            write_translated_string(buf, handle, version);
        }
        AttrTypes::WString | AttrTypes::LSWString => {
            // WString / LSWString (UTF-16 LE)
            write_lsf_wide(buf, &attr.value);
        }
        AttrTypes::guid => {
            // guid
            write_lsf_guid_bytes(buf, &attr.value)?;
        }
        AttrTypes::TranslatedFSString => {
            // TranslatedFSString
            let handle = attr.handle.as_deref().unwrap_or(&attr.value);
            let version = attr.version.unwrap_or(1);
            write_translated_string(buf, handle, version);
            let arg_count = attr.arguments.len() as i32;
            buf.extend_from_slice(&arg_count.to_le_bytes());
            for arg in &attr.arguments {
                let key_bytes = arg.key.as_bytes();
                buf.extend_from_slice(&((key_bytes.len() + 1) as i32).to_le_bytes());
                buf.extend_from_slice(key_bytes);
                buf.push(0); // null terminator
                let nested_handle = arg.string.handle.as_deref().unwrap_or(&arg.string.value);
                let nested_version = arg.string.version.unwrap_or(1);
                write_translated_string(buf, nested_handle, nested_version);
                let val_bytes = arg.value.as_bytes();
                buf.extend_from_slice(&((val_bytes.len() + 1) as i32).to_le_bytes());
                buf.extend_from_slice(val_bytes);
                buf.push(0); // null terminator
            }
        } // _ => return Err(format!("Unsupported write type_id {type_id}")),
    }
    Ok(())
}

fn write_lsf_string(buf: &mut Vec<u8>, value: &str) {
    let bytes = value.as_bytes();
    buf.extend_from_slice(bytes);
    buf.push(0); // null terminator
}

fn write_lsf_wide(buf: &mut Vec<u8>, value: &str) {
    for ch in value.encode_utf16() {
        buf.extend_from_slice(&ch.to_le_bytes());
    }
    buf.extend_from_slice(&0u16.to_le_bytes()); // null terminator
}

fn write_translated_string(buf: &mut Vec<u8>, handle: &str, version: u16) {
    buf.extend_from_slice(&version.to_le_bytes());
    let handle_bytes = handle.as_bytes();
    buf.extend_from_slice(&((handle_bytes.len() + 1) as i32).to_le_bytes());
    buf.extend_from_slice(handle_bytes);
    buf.push(0); // null terminator (required by LSLib/divine)
}

fn write_lsf_guid_bytes(buf: &mut Vec<u8>, value: &str) -> Result<(), String> {
    let uuid = Uuid::parse_str(value).map_err(|e| format!("Invalid GUID '{value}': {e}"))?;
    let mut bytes = uuid.to_bytes_le();
    // Reverse the byte-pair swap done in read_lsf_guid
    for index in (8..16).step_by(2) {
        bytes.swap(index, index + 1);
    }
    buf.extend_from_slice(&bytes);
    Ok(())
}

fn write_i32_values(buf: &mut Vec<u8>, value: &str, count: usize) -> Result<(), String> {
    let parts: Vec<&str> = value.split_whitespace().collect();
    if parts.len() != count {
        return Err(format!(
            "Expected {} int components, got {}",
            count,
            parts.len()
        ));
    }
    for p in parts {
        let v: i32 = p.parse().map_err(|e| format!("ivec parse: {e}"))?;
        buf.extend_from_slice(&v.to_le_bytes());
    }
    Ok(())
}

fn write_f32_values(buf: &mut Vec<u8>, value: &str, count: usize) -> Result<(), String> {
    let parts: Vec<&str> = value.split_whitespace().collect();
    if parts.len() != count {
        return Err(format!(
            "Expected {} float components, got {}",
            count,
            parts.len()
        ));
    }
    for p in parts {
        let v: f32 = p.parse().map_err(|e| format!("fvec parse: {e}"))?;
        buf.extend_from_slice(&v.to_le_bytes());
    }
    Ok(())
}

fn hex_to_bytes(hex: &str) -> Result<Vec<u8>, String> {
    (0..hex.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&hex[i..i + 2], 16)
                .map_err(|e| format!("Invalid hex in ScratchBuffer: {e}"))
        })
        .collect()
}

fn type_name_to_id(type_name: &str) -> Result<AttrTypes, String> {
    let types = AttrTypes::iter();
    let mut ret: AttrTypes = AttrTypes::None;
    for t in types {
        if type_name.to_string() == t.to_string() {
            ret = t;
            break;
        }
    }
    if (ret as usize) < AttrTypes::COUNT {
        return Ok(ret);
    }
    Err(format!("Unknown LSX attribute type '{type_name}'"))
}

// ── Write helpers ──────────────────────────────────────────────────

fn write_u8_val<W: Write>(writer: &mut W, val: u8) -> Result<(), String> {
    writer
        .write_all(&[val])
        .map_err(|e| format!("Failed to write u8: {e}"))
}

fn write_u16_val<W: Write>(writer: &mut W, val: u16) -> Result<(), String> {
    writer
        .write_all(&val.to_le_bytes())
        .map_err(|e| format!("Failed to write u16: {e}"))
}

fn write_u32<W: Write>(writer: &mut W, val: u32) -> Result<(), String> {
    writer
        .write_all(&val.to_le_bytes())
        .map_err(|e| format!("Failed to write u32: {e}"))
}

fn write_i64<W: Write>(writer: &mut W, val: i64) -> Result<(), String> {
    writer
        .write_all(&val.to_le_bytes())
        .map_err(|e| format!("Failed to write i64: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pak::PakReader;
    use crate::parsers::lsx::parse_lsx_resource;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn parses_synthetic_uncompressed_v6_lsf() {
        let bytes = build_test_lsf();
        let resource = parse_lsf(Cursor::new(bytes)).unwrap();

        assert_eq!(resource.regions.len(), 1);
        assert_eq!(resource.regions[0].id, "Progressions");
        assert_eq!(resource.regions[0].nodes.len(), 1);

        let region_root = &resource.regions[0].nodes[0];
        assert_eq!(region_root.id, "Progressions");
        assert_eq!(region_root.children.len(), 1);

        let entry = &region_root.children[0];
        assert_eq!(entry.id, "Progression");
        assert_eq!(entry.attributes.len(), 2);
        assert_eq!(entry.attributes[0].id, "UUID");
        assert_eq!(entry.attributes[1].id, "Name");
        assert_eq!(entry.attributes[1].value, "Barbarian");
    }

    #[test]
    fn parses_synthetic_uncompressed_v3_lsf() {
        let bytes = build_test_lsf_v3();
        let resource = parse_lsf(Cursor::new(bytes)).unwrap();

        assert_eq!(resource.regions.len(), 1);
        assert_eq!(resource.regions[0].id, "Progressions");
        assert_eq!(resource.regions[0].nodes.len(), 1);

        let region_root = &resource.regions[0].nodes[0];
        assert_eq!(region_root.id, "Progressions");
        assert_eq!(region_root.children.len(), 1);

        let entry = &region_root.children[0];
        assert_eq!(entry.id, "Progression");
        assert_eq!(entry.attributes.len(), 2);
        assert_eq!(entry.attributes[0].id, "UUID");
        assert_eq!(entry.attributes[1].id, "Name");
        assert_eq!(entry.attributes[1].value, "Barbarian");
    }

    #[test]
    fn parses_synthetic_uncompressed_v2_lsf() {
        let bytes = build_test_lsf_v2();
        let resource = parse_lsf(Cursor::new(bytes)).unwrap();

        assert_eq!(resource.regions.len(), 1);
        assert_eq!(resource.regions[0].id, "Progressions");
        assert_eq!(resource.regions[0].nodes.len(), 1);

        let region_root = &resource.regions[0].nodes[0];
        assert_eq!(region_root.id, "Progressions");
        assert_eq!(region_root.children.len(), 1);

        let entry = &region_root.children[0];
        assert_eq!(entry.id, "Progression");
        assert_eq!(entry.attributes.len(), 2);
        assert_eq!(entry.attributes[0].id, "UUID");
        assert_eq!(entry.attributes[1].id, "Name");
        assert_eq!(entry.attributes[1].value, "Barbarian");
    }

    #[test]
    #[ignore = "requires workspace-root Gustav.pak copied into the repository"]
    fn parses_real_workspace_lsf_from_gustav_pak() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        let pak_path = root.join("Gustav.pak");
        assert!(pak_path.exists(), "missing {}", pak_path.display());

        let reader = PakReader::open(&pak_path).unwrap();
        for entry in reader.entries() {
            if entry.is_deleted()
                || !entry
                    .path
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("lsf"))
            {
                continue;
            }

            let mut pak_entry_reader = reader.open_entry(entry).unwrap();
            let bytes = pak_entry_reader
                .read_to_end_with_limit(64 * 1024 * 1024)
                .unwrap();
            let resource = parse_lsf(Cursor::new(bytes)).unwrap();
            if resource.regions.is_empty() {
                continue;
            }

            println!(
                "LSF_SAMPLE entry_path={} regions={} first_region={} top_nodes={}",
                entry.path.as_str(),
                resource.regions.len(),
                resource.regions[0].id,
                resource.regions[0].nodes.len()
            );
            return;
        }

        panic!("expected at least one .lsf entry in Gustav.pak");
    }

    #[test]
    #[ignore = "requires unpacked Gustav Tags .lsf/.lsx files in the workspace"]
    fn compare_unpacked_gustav_tag_lsf_against_sibling_lsx() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        compare_unpacked_pair(
            &root,
            &[
                "UnpackedData",
                "Gustav",
                "Public",
                "Gustav",
                "Tags",
                "3549f056-0826-45ee-a8ae-351449b70fe3.lsf",
            ],
            "LSF_TAG_COMPARE",
        );
    }

    #[test]
    #[ignore = "requires unpacked Gustav Flags .lsf/.lsx files in the workspace"]
    fn compare_unpacked_gustav_flag_lsf_against_sibling_lsx() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        compare_unpacked_pair(
            &root,
            &[
                "UnpackedData",
                "Gustav",
                "Public",
                "Gustav",
                "Flags",
                "000dcdd7-84cc-916d-40bc-cb04031130a9.lsf",
            ],
            "LSF_FLAG_COMPARE",
        );
    }

    #[test]
    #[ignore = "requires unpacked Gustav MultiEffectInfos .lsf/.lsx files in the workspace"]
    fn compare_unpacked_multieffectinfos_lsf_against_sibling_lsx() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        compare_unpacked_pair(
            &root,
            &[
                "UnpackedData",
                "Gustav",
                "Public",
                "Gustav",
                "MultiEffectInfos",
                "0ba6bf24-13ee-4819-8aad-fc9f153d4761.lsf",
            ],
            "LSF_MULTIEFFECTINFOS_COMPARE",
        );
    }

    #[test]
    #[ignore = "requires unpacked non-dialog .lsf/.lsx files in the workspace"]
    fn compare_unpacked_metadata_lsf_against_sibling_lsx() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        compare_unpacked_pair(
            &root,
            &[
                "UnpackedData",
                "DiceSet01",
                "Mods",
                "DiceSet_01",
                "GUI",
                "metadata.lsf",
            ],
            "LSF_METADATA_COMPARE",
        );
    }

    #[test]
    #[ignore = "requires unpacked Gustav CharacterVisuals merged .lsf/.lsx files in the workspace"]
    fn compare_unpacked_charactervisuals_merged_lsf_against_sibling_lsx() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        compare_unpacked_pair(
            &root,
            &[
                "UnpackedData",
                "Gustav",
                "Public",
                "Gustav",
                "Content",
                "[PAK]_CharacterVisuals",
                "_merged.lsf",
            ],
            "LSF_CHARACTERVISUALS_COMPARE",
        );
    }

    #[test]
    #[ignore = "requires unpacked Gustav RootTemplates merged .lsf/.lsx files in the workspace"]
    fn compare_unpacked_roottemplates_merged_lsf_against_sibling_lsx() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        compare_unpacked_pair(
            &root,
            &[
                "UnpackedData",
                "Gustav",
                "Public",
                "Gustav",
                "RootTemplates",
                "_merged.lsf",
            ],
            "LSF_ROOTTEMPLATES_COMPARE",
        );
    }

    #[test]
    #[ignore = "requires unpacked Engine Timeline config .lsf/.lsx files in the workspace"]
    fn compare_unpacked_timeline_systemconfig_lsf_against_sibling_lsx() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        compare_unpacked_pair(
            &root,
            &[
                "UnpackedData",
                "Engine",
                "Public",
                "Engine",
                "Timeline",
                "SystemConfig.lsf",
            ],
            "LSF_TIMELINE_SYSTEMCONFIG_COMPARE",
        );
    }

    #[test]
    #[ignore = "requires unpacked Engine Timeline material group .lsf/.lsx files in the workspace"]
    fn compare_unpacked_timeline_materialgroup_lsf_against_sibling_lsx() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        compare_unpacked_pair(
            &root,
            &[
                "UnpackedData",
                "Engine",
                "Public",
                "Engine",
                "Timeline",
                "MaterialGroups",
                "0055e802-a9e0-473c-9848-1b1d7fc998a2.lsf",
            ],
            "LSF_TIMELINE_MATERIALGROUP_COMPARE",
        );
    }

    #[test]
    #[ignore = "requires unpacked merged scenery .lsf/.lsx files in the workspace"]
    fn compare_unpacked_level_scenery_merged_lsf_against_sibling_lsx() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        compare_unpacked_pair(
            &root,
            &[
                "UnpackedData",
                "Gustav",
                "Mods",
                "Gustav",
                "Levels",
                "BGO_WyrmsCrossing_C",
                "Scenery",
                "_merged.lsf",
            ],
            "LSF_SCENERY_MERGED_COMPARE",
        );
    }

    #[test]
    #[ignore = "requires unpacked Game [PAK]_UI .lsf/.lsx files in the workspace"]
    fn compare_unpacked_game_ui_resource_lsf_against_sibling_lsx() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();

        compare_unpacked_pair(
            &root,
            &[
                "UnpackedData",
                "Game",
                "Public",
                "Game",
                "Content",
                "Assets",
                "[PAK]_UI",
                "f0613a03-6ebc-4d7d-a2c4-5a5fddd5b49a.lsf",
            ],
            "LSF_GAME_UI_RESOURCE_COMPARE",
        );
    }

    #[test]
    #[ignore = "requires unpacked Story Dialog .lsf/.lsx files in the workspace"]
    fn compare_unpacked_story_dialog_lsf_against_sibling_lsx() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        compare_unpacked_pair(
            &root,
            &[
                "UnpackedData",
                "Gustav",
                "Mods",
                "Gustav",
                "Story",
                "DialogsBinary",
                "Act1",
                "Chapel",
                "CHA_BronzePlaque_AD_FL1Mural.lsf",
            ],
            "LSF_DIALOG_COMPARE",
        );
    }

    #[test]
    #[ignore = "requires the known unpacked LSF/LSX parity sample set in the workspace"]
    fn compare_known_unpacked_lsf_pairs() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();

        let cases: [(&str, &[&str]); 10] = [
            (
                "LSF_TAG_COMPARE",
                &[
                    "UnpackedData",
                    "Gustav",
                    "Public",
                    "Gustav",
                    "Tags",
                    "3549f056-0826-45ee-a8ae-351449b70fe3.lsf",
                ],
            ),
            (
                "LSF_FLAG_COMPARE",
                &[
                    "UnpackedData",
                    "Gustav",
                    "Public",
                    "Gustav",
                    "Flags",
                    "000dcdd7-84cc-916d-40bc-cb04031130a9.lsf",
                ],
            ),
            (
                "LSF_MULTIEFFECTINFOS_COMPARE",
                &[
                    "UnpackedData",
                    "Gustav",
                    "Public",
                    "Gustav",
                    "MultiEffectInfos",
                    "0ba6bf24-13ee-4819-8aad-fc9f153d4761.lsf",
                ],
            ),
            (
                "LSF_METADATA_COMPARE",
                &[
                    "UnpackedData",
                    "DiceSet01",
                    "Mods",
                    "DiceSet_01",
                    "GUI",
                    "metadata.lsf",
                ],
            ),
            (
                "LSF_CHARACTERVISUALS_COMPARE",
                &[
                    "UnpackedData",
                    "Gustav",
                    "Public",
                    "Gustav",
                    "Content",
                    "[PAK]_CharacterVisuals",
                    "_merged.lsf",
                ],
            ),
            (
                "LSF_ROOTTEMPLATES_COMPARE",
                &[
                    "UnpackedData",
                    "Gustav",
                    "Public",
                    "Gustav",
                    "RootTemplates",
                    "_merged.lsf",
                ],
            ),
            (
                "LSF_TIMELINE_SYSTEMCONFIG_COMPARE",
                &[
                    "UnpackedData",
                    "Engine",
                    "Public",
                    "Engine",
                    "Timeline",
                    "SystemConfig.lsf",
                ],
            ),
            (
                "LSF_TIMELINE_MATERIALGROUP_COMPARE",
                &[
                    "UnpackedData",
                    "Engine",
                    "Public",
                    "Engine",
                    "Timeline",
                    "MaterialGroups",
                    "0055e802-a9e0-473c-9848-1b1d7fc998a2.lsf",
                ],
            ),
            (
                "LSF_SCENERY_MERGED_COMPARE",
                &[
                    "UnpackedData",
                    "Gustav",
                    "Mods",
                    "Gustav",
                    "Levels",
                    "BGO_WyrmsCrossing_C",
                    "Scenery",
                    "_merged.lsf",
                ],
            ),
            (
                "LSF_DIALOG_COMPARE",
                &[
                    "UnpackedData",
                    "Gustav",
                    "Mods",
                    "Gustav",
                    "Story",
                    "DialogsBinary",
                    "Act1",
                    "Chapel",
                    "CHA_BronzePlaque_AD_FL1Mural.lsf",
                ],
            ),
        ];

        for (label, parts) in cases {
            compare_unpacked_pair(&root, parts, label);
        }
    }

    fn compare_unpacked_pair(root: &Path, relative_parts: &[&str], label: &str) {
        let mut lsf_path = root.to_path_buf();
        for part in relative_parts {
            lsf_path.push(part);
        }
        let lsx_path = lsf_path.with_extension("lsx");

        assert!(lsf_path.exists(), "missing {}", lsf_path.display());
        assert!(lsx_path.exists(), "missing {}", lsx_path.display());

        let native_bytes = fs::read(&lsf_path).unwrap();
        let native_resource = parse_lsf(Cursor::new(native_bytes)).unwrap();
        let sibling_lsx = fs::read_to_string(&lsx_path).unwrap();
        let sibling_resource = parse_lsx_resource(&sibling_lsx).unwrap();

        let native_points = collect_resource_points(&native_resource);
        let sibling_points = collect_resource_points(&sibling_resource);

        let missing_in_native = multiset_difference(&sibling_points, &native_points);
        let missing_in_lsx = multiset_difference(&native_points, &sibling_points);

        println!(
            "{} file={} native_points={} sibling_points={} missing_in_native={} missing_in_lsx={}",
            label,
            lsf_path.display(),
            total_point_count(&native_points),
            total_point_count(&sibling_points),
            total_point_count(&missing_in_native),
            total_point_count(&missing_in_lsx)
        );

        for point in expand_points(&missing_in_native).into_iter().take(20) {
            println!("{label}_MISSING_IN_NATIVE {point}");
        }
        for point in expand_points(&missing_in_lsx).into_iter().take(20) {
            println!("{label}_MISSING_IN_LSX {point}");
        }

        assert!(
            !native_points.is_empty(),
            "expected native value inventory for {}",
            lsf_path.display()
        );
        assert!(
            !sibling_points.is_empty(),
            "expected sibling LSX value inventory for {}",
            lsx_path.display()
        );
        assert!(
            missing_in_native.is_empty() && missing_in_lsx.is_empty(),
            "{} mismatch for {}: missing_in_native={} missing_in_lsx={}",
            label,
            lsf_path.display(),
            total_point_count(&missing_in_native),
            total_point_count(&missing_in_lsx)
        );
    }

    fn collect_resource_points(resource: &LsxResource) -> BTreeMap<String, usize> {
        let mut points = BTreeMap::new();
        for region in &resource.regions {
            add_point(&mut points, format!("region|{}", region.id));
            for node in &region.nodes {
                collect_node_points(&mut points, node);
            }
        }
        points
    }

    fn collect_node_points(points: &mut BTreeMap<String, usize>, node: &LsxNode) {
        add_point(points, format!("node|{}", node.id));
        if let Some(key) = &node.key_attribute {
            add_point(points, format!("node-key|{}|{}", node.id, key));
        }

        for attr in &node.attributes {
            let normalized_value = normalize_inventory_value(&attr.value);
            add_point(
                points,
                format!(
                    "attr|{}|{}|{}|handle={}|version={}",
                    node.id,
                    attr.id,
                    normalized_value,
                    attr.handle
                        .as_deref()
                        .map(normalize_inventory_value)
                        .as_deref()
                        .unwrap_or(""),
                    attr.version
                        .map(|value| value.to_string())
                        .as_deref()
                        .unwrap_or("")
                ),
            );

            for (index, argument) in attr.arguments.iter().enumerate() {
                let normalized_argument_value = normalize_inventory_value(&argument.value);
                let normalized_string_value = normalize_inventory_value(&argument.string.value);
                add_point(
                    points,
                    format!(
                        "arg|{}|{}|{}|{}|{}|handle={}|version={}|{}",
                        node.id,
                        attr.id,
                        index,
                        argument.key,
                        normalized_argument_value,
                        normalized_string_value,
                        argument
                            .string
                            .handle
                            .as_deref()
                            .map(normalize_inventory_value)
                            .as_deref()
                            .unwrap_or(""),
                        argument
                            .string
                            .version
                            .map(|value| value.to_string())
                            .as_deref()
                            .unwrap_or("")
                    ),
                );
            }
        }

        for child in &node.children {
            collect_node_points(points, child);
        }
    }

    fn add_point(points: &mut BTreeMap<String, usize>, point: String) {
        *points.entry(point).or_insert(0) += 1;
    }

    fn normalize_inventory_value(value: &str) -> String {
        let unescaped = xml_unescape(value);
        let collapsed = unescaped.split_whitespace().collect::<Vec<_>>().join(" ");
        if collapsed.contains(' ') {
            let normalized_tokens: Vec<String> = collapsed
                .split(' ')
                .filter(|token| !token.is_empty())
                .map(normalize_numeric_token)
                .collect();
            return normalized_tokens.join(" ");
        }

        normalize_numeric_token(&collapsed)
    }

    fn normalize_numeric_token(token: &str) -> String {
        match token.parse::<f64>() {
            Ok(number) => format_float_for_inventory(number),
            Err(_) => token.to_string(),
        }
    }

    fn format_float_for_inventory(number: f64) -> String {
        if number == 0.0 {
            return "0".to_string();
        }

        let magnitude = number.abs().log10().floor();
        let scale = 10f64.powf(6.0 - magnitude);
        let rounded = (number * scale).round() / scale;
        rounded.to_string()
    }

    fn xml_unescape(value: &str) -> String {
        value
            .replace("&#xD;&#xA;", " ")
            .replace("&#13;&#10;", " ")
            .replace("\r\n", " ")
            .replace(['\n', '\r'], " ")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&")
    }

    fn multiset_difference(
        left: &BTreeMap<String, usize>,
        right: &BTreeMap<String, usize>,
    ) -> BTreeMap<String, usize> {
        let mut diff = BTreeMap::new();
        for (point, left_count) in left {
            let right_count = right.get(point).copied().unwrap_or(0);
            if *left_count > right_count {
                diff.insert(point.clone(), left_count - right_count);
            }
        }
        diff
    }

    fn total_point_count(points: &BTreeMap<String, usize>) -> usize {
        points.values().sum()
    }

    fn expand_points(points: &BTreeMap<String, usize>) -> Vec<String> {
        let mut expanded = Vec::new();
        for (point, count) in points {
            for _ in 0..*count {
                expanded.push(point.clone());
            }
        }
        expanded
    }

    fn build_test_lsf() -> Vec<u8> {
        let names = build_names_section(&[&["Progressions", "Progression", "UUID", "Name"]]);

        let mut values = Vec::new();
        let uuid_offset = values.len() as u32;
        values.extend_from_slice(
            Uuid::parse_str("aaaaaaaa-1111-2222-3333-bbbbbbbbbbbb")
                .unwrap()
                .to_bytes_le()
                .as_slice(),
        );
        let name_offset = values.len() as u32;
        values.extend_from_slice(b"Barbarian");

        let mut attributes = Vec::new();
        attributes.extend_from_slice(&make_attr_v3(0, 2, 31, 16, 1, uuid_offset));
        attributes.extend_from_slice(&make_attr_v3(0, 3, 23, 9, -1, name_offset));

        let mut nodes = Vec::new();
        nodes.extend_from_slice(&make_node_v3(0, 0, -1, -1, -1));
        nodes.extend_from_slice(&make_node_v3(0, 1, 0, -1, 0));

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"LSOF");
        bytes.extend_from_slice(&6u32.to_le_bytes());
        bytes.extend_from_slice(&0x0002_0000_0000_0000i64.to_le_bytes());
        bytes.extend_from_slice(&(names.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&(nodes.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&(attributes.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&(values.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.push(0);
        bytes.push(0);
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&names);
        bytes.extend_from_slice(&nodes);
        bytes.extend_from_slice(&attributes);
        bytes.extend_from_slice(&values);
        bytes
    }

    fn build_test_lsf_v3() -> Vec<u8> {
        let names = build_names_section(&[&["Progressions", "Progression", "UUID", "Name"]]);

        let mut values = Vec::new();
        let uuid_offset = values.len() as u32;
        values.extend_from_slice(
            Uuid::parse_str("aaaaaaaa-1111-2222-3333-bbbbbbbbbbbb")
                .unwrap()
                .to_bytes_le()
                .as_slice(),
        );
        let name_offset = values.len() as u32;
        values.extend_from_slice(b"Barbarian");

        let mut attributes = Vec::new();
        attributes.extend_from_slice(&make_attr_v3(0, 2, 31, 16, 1, uuid_offset));
        attributes.extend_from_slice(&make_attr_v3(0, 3, 23, 9, -1, name_offset));

        let mut nodes = Vec::new();
        nodes.extend_from_slice(&make_node_v3(0, 0, -1, -1, -1));
        nodes.extend_from_slice(&make_node_v3(0, 1, 0, -1, 0));

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"LSOF");
        bytes.extend_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(&0i32.to_le_bytes());
        bytes.extend_from_slice(&(names.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&(nodes.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&(attributes.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&(values.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.push(0);
        bytes.push(0);
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&names);
        bytes.extend_from_slice(&nodes);
        bytes.extend_from_slice(&attributes);
        bytes.extend_from_slice(&values);
        bytes
    }

    fn build_test_lsf_v2() -> Vec<u8> {
        let names = build_names_section(&[&["Progressions", "Progression", "UUID", "Name"]]);

        let mut values = Vec::new();
        values.extend_from_slice(
            Uuid::parse_str("aaaaaaaa-1111-2222-3333-bbbbbbbbbbbb")
                .unwrap()
                .to_bytes_le()
                .as_slice(),
        );
        values.extend_from_slice(b"Barbarian");

        let mut attributes = Vec::new();
        attributes.extend_from_slice(&make_attr_v2(0, 2, 31, 16, 1));
        attributes.extend_from_slice(&make_attr_v2(0, 3, 23, 9, 1));

        let mut nodes = Vec::new();
        nodes.extend_from_slice(&make_node_v2(0, 0, -1, -1));
        nodes.extend_from_slice(&make_node_v2(0, 1, 0, 0));

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"LSOF");
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&0i32.to_le_bytes());
        bytes.extend_from_slice(&(names.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&(nodes.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&(attributes.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&(values.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.push(0);
        bytes.push(0);
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&names);
        bytes.extend_from_slice(&nodes);
        bytes.extend_from_slice(&attributes);
        bytes.extend_from_slice(&values);
        bytes
    }

    fn build_names_section(buckets: &[&[&str]]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(buckets.len() as u32).to_le_bytes());
        for bucket in buckets {
            bytes.extend_from_slice(&(bucket.len() as u16).to_le_bytes());
            for value in *bucket {
                bytes.extend_from_slice(&(value.len() as u16).to_le_bytes());
                bytes.extend_from_slice(value.as_bytes());
            }
        }
        bytes
    }

    fn make_node_v3(
        name_index: u16,
        name_offset: u16,
        parent_index: i32,
        next_sibling_index: i32,
        first_attribute_index: i32,
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        let name_hash = ((name_index as u32) << 16) | name_offset as u32;
        bytes.extend_from_slice(&name_hash.to_le_bytes());
        bytes.extend_from_slice(&parent_index.to_le_bytes());
        bytes.extend_from_slice(&next_sibling_index.to_le_bytes());
        bytes.extend_from_slice(&first_attribute_index.to_le_bytes());
        bytes
    }

    fn make_node_v2(
        name_index: u16,
        name_offset: u16,
        first_attribute_index: i32,
        parent_index: i32,
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        let name_hash = ((name_index as u32) << 16) | name_offset as u32;
        bytes.extend_from_slice(&name_hash.to_le_bytes());
        bytes.extend_from_slice(&first_attribute_index.to_le_bytes());
        bytes.extend_from_slice(&parent_index.to_le_bytes());
        bytes
    }

    fn make_attr_v3(
        name_index: u16,
        name_offset: u16,
        type_id: u32,
        length: u32,
        next_attribute_index: i32,
        data_offset: u32,
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        let name_hash = ((name_index as u32) << 16) | name_offset as u32;
        let type_and_length = (length << 6) | type_id;
        bytes.extend_from_slice(&name_hash.to_le_bytes());
        bytes.extend_from_slice(&type_and_length.to_le_bytes());
        bytes.extend_from_slice(&next_attribute_index.to_le_bytes());
        bytes.extend_from_slice(&data_offset.to_le_bytes());
        bytes
    }

    fn make_attr_v2(
        name_index: u16,
        name_offset: u16,
        type_id: u32,
        length: u32,
        node_index: i32,
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        let name_hash = ((name_index as u32) << 16) | name_offset as u32;
        let type_and_length = (length << 6) | type_id;
        bytes.extend_from_slice(&name_hash.to_le_bytes());
        bytes.extend_from_slice(&type_and_length.to_le_bytes());
        bytes.extend_from_slice(&node_index.to_le_bytes());
        bytes
    }

    #[test]
    fn write_then_read_round_trip() {
        // Build a resource with typed attributes matching an effect file
        let resource = LsxResource {
            regions: vec![LsxRegion {
                id: "Effect".to_string(),
                nodes: vec![LsxNode {
                    id: "Effect".to_string(),
                    key_attribute: None,
                    attributes: vec![LsxNodeAttribute {
                        id: "Duration".to_string(),
                        attr_type: "float".to_string(),
                        value: "2.5".to_string(),
                        handle: None,
                        version: None,
                        arguments: Vec::new(),
                    }],
                    children: vec![LsxNode {
                        id: "EffectComponents".to_string(),
                        key_attribute: None,
                        attributes: Vec::new(),
                        children: vec![LsxNode {
                            id: "EffectComponent".to_string(),
                            key_attribute: None,
                            attributes: vec![
                                LsxNodeAttribute {
                                    id: "EndTime".to_string(),
                                    attr_type: "float".to_string(),
                                    value: "2.5".to_string(),
                                    handle: None,
                                    version: None,
                                    arguments: Vec::new(),
                                },
                                LsxNodeAttribute {
                                    id: "ID".to_string(),
                                    attr_type: "guid".to_string(),
                                    value: "a304fd53-7589-4818-a5d1-4c42d33cc68f".to_string(),
                                    handle: None,
                                    version: None,
                                    arguments: Vec::new(),
                                },
                                LsxNodeAttribute {
                                    id: "Name".to_string(),
                                    attr_type: "LSString".to_string(),
                                    value: "BoundingSphere".to_string(),
                                    handle: None,
                                    version: None,
                                    arguments: Vec::new(),
                                },
                                LsxNodeAttribute {
                                    id: "StartTime".to_string(),
                                    attr_type: "float".to_string(),
                                    value: "0".to_string(),
                                    handle: None,
                                    version: None,
                                    arguments: Vec::new(),
                                },
                                LsxNodeAttribute {
                                    id: "Track".to_string(),
                                    attr_type: "uint32".to_string(),
                                    value: "0".to_string(),
                                    handle: None,
                                    version: None,
                                    arguments: Vec::new(),
                                },
                                LsxNodeAttribute {
                                    id: "Type".to_string(),
                                    attr_type: "LSString".to_string(),
                                    value: "BoundingSphere".to_string(),
                                    handle: None,
                                    version: None,
                                    arguments: Vec::new(),
                                },
                            ],
                            children: vec![LsxNode {
                                id: "Properties".to_string(),
                                key_attribute: None,
                                attributes: Vec::new(),
                                children: vec![
                                    LsxNode {
                                        id: "Property".to_string(),
                                        key_attribute: None,
                                        attributes: vec![
                                            LsxNodeAttribute {
                                                id: "AttributeName".to_string(),
                                                attr_type: "FixedString".to_string(),
                                                value: "Radius".to_string(),
                                                handle: None,
                                                version: None,
                                                arguments: Vec::new(),
                                            },
                                            LsxNodeAttribute {
                                                id: "FullName".to_string(),
                                                attr_type: "FixedString".to_string(),
                                                value: "Radius".to_string(),
                                                handle: None,
                                                version: None,
                                                arguments: Vec::new(),
                                            },
                                            LsxNodeAttribute {
                                                id: "Type".to_string(),
                                                attr_type: "uint8".to_string(),
                                                value: "4".to_string(),
                                                handle: None,
                                                version: None,
                                                arguments: Vec::new(),
                                            },
                                            LsxNodeAttribute {
                                                id: "Value".to_string(),
                                                attr_type: "float".to_string(),
                                                value: "3".to_string(),
                                                handle: None,
                                                version: None,
                                                arguments: Vec::new(),
                                            },
                                        ],
                                        children: Vec::new(),
                                        commented: false,
                                    },
                                    LsxNode {
                                        id: "Property".to_string(),
                                        key_attribute: None,
                                        attributes: vec![
                                            LsxNodeAttribute {
                                                id: "AttributeName".to_string(),
                                                attr_type: "FixedString".to_string(),
                                                value: "Visible".to_string(),
                                                handle: None,
                                                version: None,
                                                arguments: Vec::new(),
                                            },
                                            LsxNodeAttribute {
                                                id: "FullName".to_string(),
                                                attr_type: "FixedString".to_string(),
                                                value: "Visible".to_string(),
                                                handle: None,
                                                version: None,
                                                arguments: Vec::new(),
                                            },
                                            LsxNodeAttribute {
                                                id: "Type".to_string(),
                                                attr_type: "uint8".to_string(),
                                                value: "0".to_string(),
                                                handle: None,
                                                version: None,
                                                arguments: Vec::new(),
                                            },
                                            LsxNodeAttribute {
                                                id: "Value".to_string(),
                                                attr_type: "bool".to_string(),
                                                value: "True".to_string(),
                                                handle: None,
                                                version: None,
                                                arguments: Vec::new(),
                                            },
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
        };

        // Write to bytes
        let mut output = Vec::new();
        write_lsf(&mut output, &resource).expect("write_lsf should succeed");

        // Verify it starts with the LSOF signature
        assert!(output.len() > 16, "output should be at least header size");
        assert_eq!(&output[0..4], b"LSOF");

        // Read back
        let parsed =
            parse_lsf(Cursor::new(output)).expect("parse_lsf should succeed on written data");

        // Verify structure
        assert_eq!(parsed.regions.len(), 1);
        assert_eq!(parsed.regions[0].id, "Effect");

        let effect_node = &parsed.regions[0].nodes[0];
        assert_eq!(effect_node.id, "Effect");

        let duration = effect_node
            .attributes
            .iter()
            .find(|a| a.id == "Duration")
            .unwrap();
        assert_eq!(duration.attr_type, "float");
        // Float round-trip: 2.5 should survive
        let dur_val: f32 = duration.value.parse().unwrap();
        assert!((dur_val - 2.5).abs() < 0.001);

        let ec = &effect_node.children[0];
        assert_eq!(ec.id, "EffectComponents");
        let comp = &ec.children[0];
        assert_eq!(comp.id, "EffectComponent");
        assert_eq!(comp.attributes.len(), 6);

        // GUID round-trip
        let id_attr = comp.attributes.iter().find(|a| a.id == "ID").unwrap();
        assert_eq!(id_attr.value, "a304fd53-7589-4818-a5d1-4c42d33cc68f");

        // String round-trip
        let name_attr = comp.attributes.iter().find(|a| a.id == "Name").unwrap();
        assert_eq!(name_attr.value, "BoundingSphere");

        // Bool round-trip
        let props = &comp.children[0]; // Properties
        let visible_prop = &props.children[1]; // second Property
        let val = visible_prop
            .attributes
            .iter()
            .find(|a| a.id == "Value")
            .unwrap();
        assert_eq!(val.value, "True");
        assert_eq!(val.attr_type, "bool");
    }

    #[test]
    fn write_lsf_v7_matches_divine_section_sizes() {
        // Parse the same LSX that divine.exe converted, write via our writer,
        // then compare header/section sizes with divine's output.
        let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures");
        let lsx_path = fixtures.join("divine_charvisuals.lsx");
        let divine_lsf_path = fixtures.join("divine_charvisuals.lsf");

        if !lsx_path.exists() || !divine_lsf_path.exists() {
            eprintln!("Skipping divine comparison — fixtures not found");
            return;
        }

        let lsx_content = fs::read_to_string(&lsx_path).unwrap();
        let resource = parse_lsx_resource(&lsx_content).unwrap();

        let mut our_output = Vec::new();
        write_lsf(&mut our_output, &resource).unwrap();

        let divine_bytes = fs::read(&divine_lsf_path).unwrap();

        // Both should start with LSOF signature
        assert_eq!(&our_output[0..4], b"LSOF");
        assert_eq!(&divine_bytes[0..4], b"LSOF");

        // Both should be version 7
        let our_ver = u32::from_le_bytes(our_output[4..8].try_into().unwrap());
        let div_ver = u32::from_le_bytes(divine_bytes[4..8].try_into().unwrap());
        assert_eq!(our_ver, 7, "our version should be 7");
        assert_eq!(div_ver, 7, "divine version should be 7");

        // Compare section sizes from headers (offset 16 onwards, u32 pairs)
        let bytes_read_u32_at = |buf: &[u8], off: usize| -> u32 {
            u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
        };

        // strings_uncompressed at offset 16, strings_on_disk at offset 20
        let our_strings_uncmp = bytes_read_u32_at(&our_output, 16);
        let div_strings_uncmp = bytes_read_u32_at(&divine_bytes, 16);
        let our_strings_disk = bytes_read_u32_at(&our_output, 20);
        let div_strings_disk = bytes_read_u32_at(&divine_bytes, 20);
        assert_eq!(
            our_strings_disk, 0,
            "our strings_on_disk should be 0 (uncompressed)"
        );
        assert_eq!(div_strings_disk, 0, "divine strings_on_disk should be 0");
        assert_eq!(
            our_strings_uncmp, div_strings_uncmp,
            "strings section size mismatch: ours={our_strings_uncmp} divine={div_strings_uncmp}"
        );

        // nodes at offset 32/36
        let our_nodes = bytes_read_u32_at(&our_output, 32);
        let div_nodes = bytes_read_u32_at(&divine_bytes, 32);
        assert_eq!(
            our_nodes, div_nodes,
            "nodes section size mismatch: ours={our_nodes} divine={div_nodes}"
        );

        // attrs at offset 40/44
        let our_attrs = bytes_read_u32_at(&our_output, 40);
        let div_attrs = bytes_read_u32_at(&divine_bytes, 40);
        assert_eq!(
            our_attrs, div_attrs,
            "attrs section size mismatch: ours={our_attrs} divine={div_attrs}"
        );

        // values at offset 48/52
        let our_values = bytes_read_u32_at(&our_output, 48);
        let div_values = bytes_read_u32_at(&divine_bytes, 48);
        assert_eq!(
            our_values, div_values,
            "values section size mismatch: ours={our_values} divine={div_values}"
        );

        // Total file size
        assert_eq!(
            our_output.len(),
            divine_bytes.len(),
            "total file size mismatch: ours={} divine={}",
            our_output.len(),
            divine_bytes.len()
        );

        // Verify our output is readable and round-trips correctly
        let parsed = parse_lsf(Cursor::new(&our_output)).unwrap();
        assert!(
            !parsed.regions.is_empty(),
            "parsed output should have regions"
        );

        println!(
            "DIVINE_COMPARE: our_size={} divine_size={} strings={} nodes={} attrs={} values={}",
            our_output.len(),
            divine_bytes.len(),
            our_strings_uncmp,
            our_nodes,
            our_attrs,
            our_values
        );
    }
}
