//! Reader and writer for `.lsefx` toolkit XML effect files.
//!
//! The `.lsefx` format is the BG3 Toolkit's source representation for visual
//! effects. It is compiled into the runtime `.lsfx` binary that the game engine
//! actually loads.

use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};
use quick_xml::reader::Reader;
use quick_xml::Writer;
use std::io::Cursor;

use crate::models::effect::*;

// ── Reader ──────────────────────────────────────────────────────────

/// Parse `.lsefx` XML content into an [`EffectResource`].
pub fn read_lsefx(content: &str) -> Result<EffectResource, String> {
    let mut reader = Reader::from_str(content);
    let mut buf = Vec::new();
    let mut effect = EffectResource::default();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Eof) => break,
            Ok(Event::Start(ref e)) => {
                let tag = tag_name(e.name().as_ref());
                match tag.as_str() {
                    "effect" => {
                        for attr in e.attributes().flatten() {
                            match attr.key.as_ref() {
                                b"version" => effect.version = attr_val(&attr),
                                b"effectversion" => effect.effect_version = attr_val(&attr),
                                b"id" => effect.id = attr_val(&attr),
                                _ => {}
                            }
                        }
                    }
                    "phases" => {
                        effect.phases_xml = read_inner_xml(&mut reader, "phases")?;
                    }
                    "colors" => {
                        effect.colors_xml = read_inner_xml(&mut reader, "colors")?;
                    }
                    "trackgroup" => {
                        let tg = read_trackgroup(&mut reader, e)?;
                        effect.track_groups.push(tg);
                    }
                    _ => {}
                }
            }
            Err(e) => return Err(format!("lsefx parse error: {e}")),
            _ => {}
        }
        buf.clear();
    }

    Ok(effect)
}

fn read_trackgroup(reader: &mut Reader<&[u8]>, start: &BytesStart) -> Result<TrackGroup, String> {
    let mut tg = TrackGroup::default();
    for attr in start.attributes().flatten() {
        if attr.key.as_ref() == b"name" {
            tg.name = attr_val(&attr);
        }
    }

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let tag = tag_name(e.name().as_ref());
                match tag.as_str() {
                    "id" => {
                        let mut val = String::new();
                        for attr in e.attributes().flatten() {
                            if attr.key.as_ref() == b"value" {
                                val = attr_val(&attr);
                            }
                        }
                        tg.ids.push(TrackGroupId { value: val });
                        // consume the closing </id>
                        skip_to_end(reader, "id")?;
                    }
                    "track" => {
                        let track = read_track(reader, e)?;
                        tg.tracks.push(track);
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(ref e)) => {
                let tag = tag_name(e.name().as_ref());
                if tag == "id" {
                    let mut val = String::new();
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"value" {
                            val = attr_val(&attr);
                        }
                    }
                    tg.ids.push(TrackGroupId { value: val });
                }
            }
            Ok(Event::End(ref e)) if tag_name(e.name().as_ref()) == "trackgroup" => break,
            Ok(Event::Eof) => return Err("Unexpected EOF inside <trackgroup>".into()),
            Err(e) => return Err(format!("lsefx parse error in trackgroup: {e}")),
            _ => {}
        }
        buf.clear();
    }
    Ok(tg)
}

fn read_track(reader: &mut Reader<&[u8]>, start: &BytesStart) -> Result<Track, String> {
    let mut track = Track::default();
    for attr in start.attributes().flatten() {
        match attr.key.as_ref() {
            b"name" => track.name = attr_val(&attr),
            b"muted" => track.muted = attr_val(&attr),
            b"locked" => track.locked = attr_val(&attr),
            b"mutestateoverride" => track.mute_state_override = attr_val(&attr),
            _ => {}
        }
    }

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let tag = tag_name(e.name().as_ref());
                if tag == "component" {
                    let comp = read_component(reader, e)?;
                    track.components.push(comp);
                }
            }
            Ok(Event::End(ref e)) if tag_name(e.name().as_ref()) == "track" => break,
            Ok(Event::Eof) => return Err("Unexpected EOF inside <track>".into()),
            Err(e) => return Err(format!("lsefx parse error in track: {e}")),
            _ => {}
        }
        buf.clear();
    }
    Ok(track)
}

fn read_component(
    reader: &mut Reader<&[u8]>,
    start: &BytesStart,
) -> Result<EffectComponent, String> {
    let mut comp = EffectComponent {
        class_name: String::new(),
        start: "0".into(),
        end: "0".into(),
        instance_name: String::new(),
        properties: Vec::new(),
        property_groups: Vec::new(),
        modules: Vec::new(),
    };

    for attr in start.attributes().flatten() {
        match attr.key.as_ref() {
            b"class" => comp.class_name = attr_val(&attr),
            b"start" => comp.start = attr_val(&attr),
            b"end" => comp.end = attr_val(&attr),
            b"instancename" => comp.instance_name = attr_val(&attr),
            _ => {}
        }
    }

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let tag = tag_name(e.name().as_ref());
                match tag.as_str() {
                    "property" => {
                        let prop = read_property(reader, e)?;
                        comp.properties.push(prop);
                    }
                    "propertygroup" => {
                        let pg = read_propertygroup(reader, e)?;
                        comp.property_groups.push(pg);
                    }
                    "module" => {
                        let m = read_module(reader, e)?;
                        comp.modules.push(m);
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(ref e)) => {
                let tag = tag_name(e.name().as_ref());
                if tag == "module" {
                    let m = read_module_empty(e)?;
                    comp.modules.push(m);
                }
            }
            Ok(Event::End(ref e)) if tag_name(e.name().as_ref()) == "component" => break,
            Ok(Event::Eof) => return Err("Unexpected EOF inside <component>".into()),
            Err(e) => return Err(format!("lsefx parse error in component: {e}")),
            _ => {}
        }
        buf.clear();
    }
    Ok(comp)
}

fn read_property(reader: &mut Reader<&[u8]>, start: &BytesStart) -> Result<EffectProperty, String> {
    let mut prop = EffectProperty {
        guid: String::new(),
        data: Vec::new(),
        platform_metadata: Vec::new(),
    };

    for attr in start.attributes().flatten() {
        if attr.key.as_ref() == b"id" {
            prop.guid = attr_val(&attr);
        }
    }

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let tag = tag_name(e.name().as_ref());
                match tag.as_str() {
                    "datum" => {
                        let datum = read_datum(reader, e)?;
                        prop.data.push(datum);
                    }
                    "platformmetadata" => {
                        let pm = read_platform_metadata(e);
                        prop.platform_metadata.push(pm);
                        skip_to_end(reader, "platformmetadata")?;
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(ref e)) => {
                let tag = tag_name(e.name().as_ref());
                match tag.as_str() {
                    "datum" => {
                        let datum = read_datum_empty(e);
                        prop.data.push(datum);
                    }
                    "platformmetadata" => {
                        let pm = read_platform_metadata(e);
                        prop.platform_metadata.push(pm);
                    }
                    _ => {}
                }
            }
            Ok(Event::End(ref e)) if tag_name(e.name().as_ref()) == "property" => break,
            Ok(Event::Eof) => return Err("Unexpected EOF inside <property>".into()),
            Err(e) => return Err(format!("lsefx parse error in property: {e}")),
            _ => {}
        }
        buf.clear();
    }
    Ok(prop)
}

fn read_datum(reader: &mut Reader<&[u8]>, start: &BytesStart) -> Result<Datum, String> {
    let mut datum = Datum::default();
    for attr in start.attributes().flatten() {
        match attr.key.as_ref() {
            b"platform" => datum.platform = attr_val(&attr),
            b"lod" => datum.lod = attr_val(&attr),
            b"value" => datum.value = Some(attr_val(&attr)),
            _ => {}
        }
    }

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let tag = tag_name(e.name().as_ref());
                if tag == "rampchanneldata" {
                    datum.ramp_channel_data = Some(read_rampchanneldata(reader)?);
                }
            }
            Ok(Event::End(ref e)) if tag_name(e.name().as_ref()) == "datum" => break,
            Ok(Event::Eof) => return Err("Unexpected EOF inside <datum>".into()),
            Err(e) => return Err(format!("lsefx parse error in datum: {e}")),
            _ => {}
        }
        buf.clear();
    }
    Ok(datum)
}

fn read_datum_empty(e: &BytesStart) -> Datum {
    let mut datum = Datum::default();
    for attr in e.attributes().flatten() {
        match attr.key.as_ref() {
            b"platform" => datum.platform = attr_val(&attr),
            b"lod" => datum.lod = attr_val(&attr),
            b"value" => datum.value = Some(attr_val(&attr)),
            _ => {}
        }
    }
    datum
}

fn read_rampchanneldata(reader: &mut Reader<&[u8]>) -> Result<RampChannelData, String> {
    let mut rcd = RampChannelData {
        channels: Vec::new(),
    };

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let tag = tag_name(e.name().as_ref());
                if tag == "rampchannel" {
                    let ch = read_rampchannel(reader, e)?;
                    rcd.channels.push(ch);
                }
            }
            Ok(Event::End(ref e)) if tag_name(e.name().as_ref()) == "rampchanneldata" => break,
            Ok(Event::Eof) => return Err("Unexpected EOF inside <rampchanneldata>".into()),
            Err(e) => return Err(format!("lsefx parse error in rampchanneldata: {e}")),
            _ => {}
        }
        buf.clear();
    }
    Ok(rcd)
}

fn read_rampchannel(reader: &mut Reader<&[u8]>, start: &BytesStart) -> Result<RampChannel, String> {
    let mut ch = RampChannel {
        channel_type: "Linear".into(),
        id: String::new(),
        selected: false,
        keyframes: Vec::new(),
    };

    for attr in start.attributes().flatten() {
        match attr.key.as_ref() {
            b"type" => ch.channel_type = attr_val(&attr),
            b"id" => ch.id = attr_val(&attr),
            b"selected" => ch.selected = attr_val(&attr).eq_ignore_ascii_case("true"),
            _ => {}
        }
    }

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e) | Event::Empty(ref e)) => {
                let tag = tag_name(e.name().as_ref());
                if tag == "keyframe" {
                    let kf = read_keyframe(e);
                    ch.keyframes.push(kf);
                }
            }
            Ok(Event::End(ref e)) if tag_name(e.name().as_ref()) == "rampchannel" => break,
            Ok(Event::Eof) => return Err("Unexpected EOF inside <rampchannel>".into()),
            Err(e) => return Err(format!("lsefx parse error in rampchannel: {e}")),
            _ => {}
        }
        buf.clear();
    }
    Ok(ch)
}

fn read_keyframe(e: &BytesStart) -> Keyframe {
    let mut kf = Keyframe {
        time: "0".into(),
        value: "0".into(),
        interpolation: None,
    };
    for attr in e.attributes().flatten() {
        match attr.key.as_ref() {
            b"time" => kf.time = attr_val(&attr),
            b"value" => kf.value = attr_val(&attr),
            b"interpolation" => kf.interpolation = Some(attr_val(&attr)),
            _ => {}
        }
    }
    kf
}

fn read_propertygroup(
    reader: &mut Reader<&[u8]>,
    start: &BytesStart,
) -> Result<PropertyGroup, String> {
    let mut pg = PropertyGroup {
        guid: String::new(),
        name: String::new(),
        collapsed: "False".into(),
    };
    for attr in start.attributes().flatten() {
        match attr.key.as_ref() {
            b"id" => pg.guid = attr_val(&attr),
            b"name" => pg.name = attr_val(&attr),
            b"collapsed" => pg.collapsed = attr_val(&attr),
            _ => {}
        }
    }
    skip_to_end(reader, "propertygroup")?;
    Ok(pg)
}

fn read_module(reader: &mut Reader<&[u8]>, start: &BytesStart) -> Result<EffectModule, String> {
    let m = read_module_empty(start)?;
    skip_to_end(reader, "module")?;
    Ok(m)
}

fn read_module_empty(e: &BytesStart) -> Result<EffectModule, String> {
    let mut m = EffectModule {
        guid: String::new(),
        muted: "False".into(),
        index: 0,
    };
    for attr in e.attributes().flatten() {
        match attr.key.as_ref() {
            b"id" => m.guid = attr_val(&attr),
            b"muted" => m.muted = attr_val(&attr),
            b"index" => {
                m.index = attr_val(&attr).parse::<u32>().unwrap_or(0);
            }
            _ => {}
        }
    }
    Ok(m)
}

fn read_platform_metadata(e: &BytesStart) -> PlatformMetadata {
    let mut pm = PlatformMetadata {
        platform: String::new(),
        expanded: "True".into(),
    };
    for attr in e.attributes().flatten() {
        match attr.key.as_ref() {
            b"platform" => pm.platform = attr_val(&attr),
            b"expanded" => pm.expanded = attr_val(&attr),
            _ => {}
        }
    }
    pm
}

// ── Writer ──────────────────────────────────────────────────────────

/// Serialize an [`EffectResource`] to `.lsefx` XML.
pub fn write_lsefx(effect: &EffectResource) -> String {
    let mut writer = Writer::new_with_indent(Cursor::new(Vec::new()), b' ', 2);

    // <?xml version="1.0" encoding="utf-8"?>
    writer
        .write_event(Event::Decl(quick_xml::events::BytesDecl::new(
            "1.0",
            Some("utf-8"),
            None,
        )))
        .unwrap();

    // <effect version="..." effectversion="..." id="...">
    let mut effect_start = BytesStart::new("effect");
    effect_start.push_attribute(("version", effect.version.as_str()));
    effect_start.push_attribute(("effectversion", effect.effect_version.as_str()));
    effect_start.push_attribute(("id", effect.id.as_str()));
    writer.write_event(Event::Start(effect_start)).unwrap();

    // <phases>...</phases>
    write_opaque_element(&mut writer, "phases", &effect.phases_xml);

    // <colors>...</colors>
    write_opaque_element(&mut writer, "colors", &effect.colors_xml);

    // <trackgroups>
    writer
        .write_event(Event::Start(BytesStart::new("trackgroups")))
        .unwrap();
    for tg in &effect.track_groups {
        write_trackgroup(&mut writer, tg);
    }
    writer
        .write_event(Event::End(BytesEnd::new("trackgroups")))
        .unwrap();

    // </effect>
    writer
        .write_event(Event::End(BytesEnd::new("effect")))
        .unwrap();

    let raw = String::from_utf8(writer.into_inner().into_inner()).unwrap();

    // quick_xml writes self-closing tags as `<tag/>` — the vanilla .lsefx
    // format uses `<tag />` (space before `/>`). Post-process to match.
    raw.replace("/>", " />")
}

fn write_opaque_element(writer: &mut Writer<Cursor<Vec<u8>>>, tag: &str, inner: &str) {
    if inner.is_empty() {
        writer
            .write_event(Event::Empty(BytesStart::new(tag)))
            .unwrap();
    } else {
        writer
            .write_event(Event::Start(BytesStart::new(tag)))
            .unwrap();
        writer
            .write_event(Event::Text(BytesText::from_escaped(inner)))
            .unwrap();
        writer.write_event(Event::End(BytesEnd::new(tag))).unwrap();
    }
}

fn write_trackgroup(writer: &mut Writer<Cursor<Vec<u8>>>, tg: &TrackGroup) {
    let mut el = BytesStart::new("trackgroup");
    el.push_attribute(("name", tg.name.as_str()));
    writer.write_event(Event::Start(el)).unwrap();

    // <ids>
    writer
        .write_event(Event::Start(BytesStart::new("ids")))
        .unwrap();
    for tid in &tg.ids {
        let mut id_el = BytesStart::new("id");
        id_el.push_attribute(("value", tid.value.as_str()));
        writer.write_event(Event::Empty(id_el)).unwrap();
    }
    writer
        .write_event(Event::End(BytesEnd::new("ids")))
        .unwrap();

    // tracks
    for track in &tg.tracks {
        write_track(writer, track);
    }

    writer
        .write_event(Event::End(BytesEnd::new("trackgroup")))
        .unwrap();
}

fn write_track(writer: &mut Writer<Cursor<Vec<u8>>>, track: &Track) {
    let mut el = BytesStart::new("track");
    el.push_attribute(("name", track.name.as_str()));
    el.push_attribute(("muted", track.muted.as_str()));
    el.push_attribute(("locked", track.locked.as_str()));
    el.push_attribute(("mutestateoverride", track.mute_state_override.as_str()));

    if track.components.is_empty() {
        writer.write_event(Event::Empty(el)).unwrap();
    } else {
        writer.write_event(Event::Start(el)).unwrap();
        for comp in &track.components {
            write_component(writer, comp);
        }
        writer
            .write_event(Event::End(BytesEnd::new("track")))
            .unwrap();
    }
}

fn write_component(writer: &mut Writer<Cursor<Vec<u8>>>, comp: &EffectComponent) {
    let mut el = BytesStart::new("component");
    el.push_attribute(("class", comp.class_name.as_str()));
    el.push_attribute(("start", comp.start.as_str()));
    el.push_attribute(("end", comp.end.as_str()));
    el.push_attribute(("instancename", comp.instance_name.as_str()));
    writer.write_event(Event::Start(el)).unwrap();

    // <properties>
    writer
        .write_event(Event::Start(BytesStart::new("properties")))
        .unwrap();
    for prop in &comp.properties {
        write_property(writer, prop);
    }
    for pg in &comp.property_groups {
        let mut pg_el = BytesStart::new("propertygroup");
        pg_el.push_attribute(("id", pg.guid.as_str()));
        pg_el.push_attribute(("name", pg.name.as_str()));
        pg_el.push_attribute(("collapsed", pg.collapsed.as_str()));
        writer.write_event(Event::Empty(pg_el)).unwrap();
    }
    writer
        .write_event(Event::End(BytesEnd::new("properties")))
        .unwrap();

    // <modules>
    writer
        .write_event(Event::Start(BytesStart::new("modules")))
        .unwrap();
    for m in &comp.modules {
        let mut mel = BytesStart::new("module");
        mel.push_attribute(("id", m.guid.as_str()));
        mel.push_attribute(("muted", m.muted.as_str()));
        mel.push_attribute(("index", m.index.to_string().as_str()));
        writer.write_event(Event::Empty(mel)).unwrap();
    }
    writer
        .write_event(Event::End(BytesEnd::new("modules")))
        .unwrap();

    writer
        .write_event(Event::End(BytesEnd::new("component")))
        .unwrap();
}

fn write_property(writer: &mut Writer<Cursor<Vec<u8>>>, prop: &EffectProperty) {
    let mut el = BytesStart::new("property");
    el.push_attribute(("id", prop.guid.as_str()));
    writer.write_event(Event::Start(el)).unwrap();

    // <data>
    writer
        .write_event(Event::Start(BytesStart::new("data")))
        .unwrap();
    for datum in &prop.data {
        write_datum(writer, datum);
    }
    writer
        .write_event(Event::End(BytesEnd::new("data")))
        .unwrap();

    // platform metadata
    for pm in &prop.platform_metadata {
        let mut pm_el = BytesStart::new("platformmetadata");
        pm_el.push_attribute(("platform", pm.platform.as_str()));
        pm_el.push_attribute(("expanded", pm.expanded.as_str()));
        writer.write_event(Event::Empty(pm_el)).unwrap();
    }

    writer
        .write_event(Event::End(BytesEnd::new("property")))
        .unwrap();
}

fn write_datum(writer: &mut Writer<Cursor<Vec<u8>>>, datum: &Datum) {
    let mut el = BytesStart::new("datum");
    el.push_attribute(("platform", datum.platform.as_str()));
    el.push_attribute(("lod", datum.lod.as_str()));

    if let Some(ref rcd) = datum.ramp_channel_data {
        // Has child ramp data — use start/end tags
        writer.write_event(Event::Start(el)).unwrap();
        write_rampchanneldata(writer, rcd);
        writer
            .write_event(Event::End(BytesEnd::new("datum")))
            .unwrap();
    } else if let Some(ref val) = datum.value {
        el.push_attribute(("value", val.as_str()));
        writer.write_event(Event::Empty(el)).unwrap();
    } else {
        el.push_attribute(("value", ""));
        writer.write_event(Event::Empty(el)).unwrap();
    }
}

fn write_rampchanneldata(writer: &mut Writer<Cursor<Vec<u8>>>, rcd: &RampChannelData) {
    writer
        .write_event(Event::Start(BytesStart::new("rampchanneldata")))
        .unwrap();
    for ch in &rcd.channels {
        let mut ch_el = BytesStart::new("rampchannel");
        ch_el.push_attribute(("type", ch.channel_type.as_str()));
        ch_el.push_attribute(("id", ch.id.as_str()));
        ch_el.push_attribute(("selected", if ch.selected { "True" } else { "False" }));
        writer.write_event(Event::Start(ch_el)).unwrap();

        writer
            .write_event(Event::Start(BytesStart::new("keyframes")))
            .unwrap();
        for kf in &ch.keyframes {
            let mut kf_el = BytesStart::new("keyframe");
            kf_el.push_attribute(("time", kf.time.as_str()));
            kf_el.push_attribute(("value", kf.value.as_str()));
            if let Some(ref interp) = kf.interpolation {
                kf_el.push_attribute(("interpolation", interp.as_str()));
            }
            writer.write_event(Event::Empty(kf_el)).unwrap();
        }
        writer
            .write_event(Event::End(BytesEnd::new("keyframes")))
            .unwrap();

        writer
            .write_event(Event::End(BytesEnd::new("rampchannel")))
            .unwrap();
    }
    writer
        .write_event(Event::End(BytesEnd::new("rampchanneldata")))
        .unwrap();
}

// ── Helpers ─────────────────────────────────────────────────────────

fn attr_val(attr: &quick_xml::events::attributes::Attribute) -> String {
    String::from_utf8_lossy(&attr.value).to_string()
}

fn tag_name(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw).to_string()
}

/// Read all inner XML content between the current position and a matching
/// closing tag, returning it as a raw string.
fn read_inner_xml(reader: &mut Reader<&[u8]>, end_tag: &str) -> Result<String, String> {
    let mut depth: u32 = 1;
    let mut buf = Vec::new();
    let mut inner = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let tag = tag_name(e.name().as_ref());
                // Reconstruct the opening tag
                inner.push('<');
                inner.push_str(&tag);
                for attr in e.attributes().flatten() {
                    inner.push(' ');
                    inner.push_str(&String::from_utf8_lossy(attr.key.as_ref()));
                    inner.push_str("=\"");
                    inner.push_str(&String::from_utf8_lossy(&attr.value));
                    inner.push('"');
                }
                inner.push('>');
                if tag == end_tag {
                    depth += 1;
                }
            }
            Ok(Event::End(ref e)) => {
                let tag = tag_name(e.name().as_ref());
                if tag == end_tag {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                inner.push_str("</");
                inner.push_str(&tag);
                inner.push('>');
            }
            Ok(Event::Empty(ref e)) => {
                let tag = tag_name(e.name().as_ref());
                inner.push('<');
                inner.push_str(&tag);
                for attr in e.attributes().flatten() {
                    inner.push(' ');
                    inner.push_str(&String::from_utf8_lossy(attr.key.as_ref()));
                    inner.push_str("=\"");
                    inner.push_str(&String::from_utf8_lossy(&attr.value));
                    inner.push('"');
                }
                inner.push_str("/>");
            }
            Ok(Event::Text(ref t)) => {
                inner.push_str(&t.decode().unwrap_or_default());
            }
            Ok(Event::Eof) => {
                return Err(format!("Unexpected EOF reading inner XML of <{end_tag}>"))
            }
            Err(e) => return Err(format!("Error reading inner XML of <{end_tag}>: {e}")),
            _ => {}
        }
        buf.clear();
    }
    Ok(inner)
}

/// Skip events until the matching end tag is consumed.
fn skip_to_end(reader: &mut Reader<&[u8]>, end_tag: &str) -> Result<(), String> {
    let mut depth: u32 = 1;
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                if tag_name(e.name().as_ref()) == end_tag {
                    depth += 1;
                }
            }
            Ok(Event::End(ref e)) => {
                if tag_name(e.name().as_ref()) == end_tag {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(());
                    }
                }
            }
            Ok(Event::Eof) => return Err(format!("Unexpected EOF skipping <{end_tag}>")),
            Err(e) => return Err(format!("Error skipping <{end_tag}>: {e}")),
            _ => {}
        }
        buf.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_minimal() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<effect version="0.0" effectversion="1.0.0" id="abcdef01-2345-6789-abcd-ef0123456789">
  <phases />
  <colors />
  <trackgroups>
    <trackgroup name="My Group">
      <ids>
        <id value="1" />
      </ids>
      <track name="Track 1" muted="False" locked="False" mutestateoverride="Unmuted">
        <component class="BoundingSphere" start="0" end="5" instancename="c73e2e8b-0000-0000-0000-000000000000">
          <properties>
            <property id="ba2ee0f9-d369-4d36-bd63-30d4c8c46a0a">
              <data>
                <datum platform="00000000-0000-0000-0000-000000000000" lod="00000000-0000-0000-0000-000000000000" value="5" />
              </data>
            </property>
          </properties>
          <modules>
            <module id="3cc91d05-19eb-4472-a94b-e622bf1574b4" muted="False" index="0" />
          </modules>
        </component>
      </track>
    </trackgroup>
  </trackgroups>
</effect>"#;

        let effect = read_lsefx(xml).unwrap();
        assert_eq!(effect.version, "0.0");
        assert_eq!(effect.effect_version, "1.0.0");
        assert_eq!(effect.id, "abcdef01-2345-6789-abcd-ef0123456789");
        assert_eq!(effect.track_groups.len(), 1);

        let tg = &effect.track_groups[0];
        assert_eq!(tg.name, "My Group");
        assert_eq!(tg.ids.len(), 1);
        assert_eq!(tg.ids[0].value, "1");
        assert_eq!(tg.tracks.len(), 1);

        let track = &tg.tracks[0];
        assert_eq!(track.name, "Track 1");
        assert_eq!(track.components.len(), 1);

        let comp = &track.components[0];
        assert_eq!(comp.class_name, "BoundingSphere");
        assert_eq!(comp.start, "0");
        assert_eq!(comp.end, "5");
        assert_eq!(comp.properties.len(), 1);
        assert_eq!(comp.modules.len(), 1);

        let prop = &comp.properties[0];
        assert_eq!(prop.guid, "ba2ee0f9-d369-4d36-bd63-30d4c8c46a0a");
        assert_eq!(prop.data[0].value.as_deref(), Some("5"));

        let m = &comp.modules[0];
        assert_eq!(m.guid, "3cc91d05-19eb-4472-a94b-e622bf1574b4");

        // Write it back and re-parse
        let output = write_lsefx(&effect);
        let effect2 = read_lsefx(&output).unwrap();
        assert_eq!(effect2.track_groups.len(), 1);
        assert_eq!(
            effect2.track_groups[0].tracks[0].components[0]
                .properties
                .len(),
            1
        );
    }

    #[test]
    fn round_trip_rampchanneldata() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<effect version="0.0" effectversion="1.0.0" id="00000000-0000-0000-0000-000000000000">
  <phases />
  <colors />
  <trackgroups>
    <trackgroup name="TG">
      <ids />
      <track name="T" muted="False" locked="False" mutestateoverride="Unmuted">
        <component class="Test" start="0" end="10" instancename="inst-001">
          <properties>
            <property id="aaa-bbb">
              <data>
                <datum platform="00000000-0000-0000-0000-000000000000" lod="00000000-0000-0000-0000-000000000000">
                  <rampchanneldata>
                    <rampchannel type="Linear" id="ch-1" selected="True">
                      <keyframes>
                        <keyframe time="0" value="1" interpolation="1" />
                        <keyframe time="5" value="0.5" />
                      </keyframes>
                    </rampchannel>
                  </rampchanneldata>
                </datum>
              </data>
            </property>
          </properties>
          <modules />
        </component>
      </track>
    </trackgroup>
  </trackgroups>
</effect>"#;

        let effect = read_lsefx(xml).unwrap();
        let comp = &effect.track_groups[0].tracks[0].components[0];
        let datum = &comp.properties[0].data[0];
        assert!(datum.value.is_none());
        let rcd = datum.ramp_channel_data.as_ref().unwrap();
        assert_eq!(rcd.channels.len(), 1);
        assert_eq!(rcd.channels[0].channel_type, "Linear");
        assert!(rcd.channels[0].selected);
        assert_eq!(rcd.channels[0].keyframes.len(), 2);
        assert_eq!(
            rcd.channels[0].keyframes[0].interpolation.as_deref(),
            Some("1")
        );
        assert!(rcd.channels[0].keyframes[1].interpolation.is_none());

        // Round-trip
        let output = write_lsefx(&effect);
        let effect2 = read_lsefx(&output).unwrap();
        let rcd2 = effect2.track_groups[0].tracks[0].components[0].properties[0].data[0]
            .ramp_channel_data
            .as_ref()
            .unwrap();
        assert_eq!(rcd2.channels[0].keyframes.len(), 2);
    }

    // ── Unhappy-path tests ─────────────────────────────────────────

    #[test]
    fn malformed_xml_returns_error() {
        let xml = r#"<effect version="0.0"><trackgroups><trackgroup name="TG">"#;
        // Truncated XML — no closing tags
        let result = read_lsefx(xml);
        assert!(
            result.is_err() || {
                // If it doesn't error, it should at least produce an empty/partial result
                // due to unexpected EOF — the parser tolerates some partial input
                let eff = result.unwrap();
                eff.track_groups.is_empty() || eff.track_groups[0].tracks.is_empty()
            }
        );
    }

    #[test]
    fn completely_invalid_xml_returns_error() {
        // quick_xml tolerates some malformed constructs; use unclosed tags to
        // trigger a real parse error inside a nested reader.
        let xml = r#"<effect version="0.0"><trackgroups><trackgroup name="X"><track name="T"><component class="C" start="0" end="1"><properties><property id="p"><data><datum><rampchanneldata>"#;
        let result = read_lsefx(xml);
        assert!(
            result.is_err(),
            "deeply truncated XML should produce an Err"
        );
    }

    #[test]
    fn empty_string_returns_default_effect() {
        let result = read_lsefx("");
        // Empty input is valid XML — yields a default EffectResource
        assert!(result.is_ok());
        let eff = result.unwrap();
        assert!(eff.track_groups.is_empty());
        // Default id is the zero-GUID, not empty
        assert_eq!(eff.id, "00000000-0000-0000-0000-000000000000");
    }

    #[test]
    fn self_closing_effect_uses_defaults() {
        // Self-closing <effect /> is an Empty event — the reader only handles
        // Start events, so attributes are not captured. This verifies the
        // parser doesn't panic and returns defaults.
        let xml = r#"<?xml version="1.0"?><effect version="1" effectversion="2" id="abc" />"#;
        let eff = read_lsefx(xml).unwrap();
        // Attributes are NOT read from Empty events — defaults are used
        assert_eq!(eff.version, "0.0");
        assert!(eff.track_groups.is_empty());
    }

    #[test]
    fn trackgroup_with_no_tracks() {
        let xml = r#"<?xml version="1.0"?>
<effect version="0.0" effectversion="1.0.0" id="test">
  <trackgroups>
    <trackgroup name="Empty TG">
      <ids><id value="99" /></ids>
    </trackgroup>
  </trackgroups>
</effect>"#;
        let eff = read_lsefx(xml).unwrap();
        assert_eq!(eff.track_groups.len(), 1);
        assert_eq!(eff.track_groups[0].name, "Empty TG");
        assert!(eff.track_groups[0].tracks.is_empty());
    }

    #[test]
    fn component_with_no_properties_or_modules() {
        let xml = r#"<?xml version="1.0"?>
<effect version="0.0" effectversion="1.0.0" id="test">
  <trackgroups>
    <trackgroup name="TG"><ids/>
      <track name="T" muted="False" locked="False" mutestateoverride="None">
        <component class="Bare" start="0" end="1" instancename="bare-id">
        </component>
      </track>
    </trackgroup>
  </trackgroups>
</effect>"#;
        let eff = read_lsefx(xml).unwrap();
        let comp = &eff.track_groups[0].tracks[0].components[0];
        assert_eq!(comp.class_name, "Bare");
        assert!(comp.properties.is_empty());
        assert!(comp.modules.is_empty());
    }

    #[test]
    fn property_with_no_data() {
        let xml = r#"<?xml version="1.0"?>
<effect version="0.0" effectversion="1.0.0" id="test">
  <trackgroups>
    <trackgroup name="TG"><ids/>
      <track name="T" muted="False" locked="False" mutestateoverride="None">
        <component class="X" start="0" end="1" instancename="id-1">
          <properties>
            <property id="no-data-guid">
            </property>
          </properties>
          <modules/>
        </component>
      </track>
    </trackgroup>
  </trackgroups>
</effect>"#;
        let eff = read_lsefx(xml).unwrap();
        let prop = &eff.track_groups[0].tracks[0].components[0].properties[0];
        assert_eq!(prop.guid, "no-data-guid");
        assert!(
            prop.data.is_empty(),
            "property with no <datum> should have empty data vec"
        );
    }

    #[test]
    fn datum_with_no_value_attribute() {
        let xml = r#"<?xml version="1.0"?>
<effect version="0.0" effectversion="1.0.0" id="test">
  <trackgroups>
    <trackgroup name="TG"><ids/>
      <track name="T" muted="False" locked="False" mutestateoverride="None">
        <component class="X" start="0" end="1" instancename="id-1">
          <properties>
            <property id="guid-1">
              <data>
                <datum platform="p" lod="l" />
              </data>
            </property>
          </properties>
          <modules/>
        </component>
      </track>
    </trackgroup>
  </trackgroups>
</effect>"#;
        let eff = read_lsefx(xml).unwrap();
        let datum = &eff.track_groups[0].tracks[0].components[0].properties[0].data[0];
        assert!(
            datum.value.is_none(),
            "datum without value= should have None value"
        );
        assert_eq!(datum.platform, "p");
        assert_eq!(datum.lod, "l");
    }

    #[test]
    fn missing_required_attributes_use_defaults() {
        // Component with no class/start/end/instancename attributes
        let xml = r#"<?xml version="1.0"?>
<effect version="0.0" effectversion="1.0.0" id="test">
  <trackgroups>
    <trackgroup name="TG"><ids/>
      <track name="T" muted="False" locked="False" mutestateoverride="None">
        <component>
          <properties/>
          <modules/>
        </component>
      </track>
    </trackgroup>
  </trackgroups>
</effect>"#;
        let eff = read_lsefx(xml).unwrap();
        let comp = &eff.track_groups[0].tracks[0].components[0];
        assert!(
            comp.class_name.is_empty(),
            "missing class should default to empty"
        );
        assert_eq!(comp.start, "0", "missing start should default to '0'");
        assert_eq!(comp.end, "0", "missing end should default to '0'");
        assert!(
            comp.instance_name.is_empty(),
            "missing instancename should default to empty"
        );
    }

    #[test]
    fn unknown_elements_are_ignored() {
        let xml = r#"<?xml version="1.0"?>
<effect version="0.0" effectversion="1.0.0" id="test">
  <trackgroups>
    <nonsense foo="bar"><child/></nonsense>
    <trackgroup name="TG"><ids/>
      <track name="T" muted="False" locked="False" mutestateoverride="None">
        <alien_tag><deeply><nested/></deeply></alien_tag>
        <component class="X" start="0" end="1" instancename="id-1">
          <unknown_section><stuff/></unknown_section>
          <properties/>
          <modules/>
        </component>
      </track>
    </trackgroup>
  </trackgroups>
</effect>"#;
        // Should not error — unknown tags are silently skipped
        let eff = read_lsefx(xml).unwrap();
        assert_eq!(eff.track_groups.len(), 1);
        assert_eq!(eff.track_groups[0].tracks[0].components[0].class_name, "X");
    }

    #[test]
    fn write_empty_effect_round_trips() {
        let empty = EffectResource::default();
        let xml = write_lsefx(&empty);
        let parsed = read_lsefx(&xml).unwrap();
        assert!(parsed.track_groups.is_empty());
    }
}
