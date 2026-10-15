//! Writing tags into XMP sidecars (`photo.jpg` → `photo.jpg.xmp`, as darktable and digiKam
//! name them).
//!
//! A sidecar may also belong to other programs (darktable keeps its edit history there), so
//! it is never rewritten wholesale: the properties the catalog owns are cut out of the
//! existing text and written again in an `rdf:Description` of their own, and every other
//! byte stays as it was. The catalog owns:
//!
//! - `dc:subject`: each tag's last segment (`Alice`).
//! - `lr:hierarchicalSubject`: whole tag paths (`People|Family|Alice`).
//! - `Iptc4xmpExt:PersonInImage`: the names of person tags.
//! - `mwg-rs:Regions`: person tags with a region, as MWG face regions.
//!
//! Values another program put in those properties are kept, as long as they aren't ones
//! the catalog wrote last time (those are the catalog's to remove).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use image::metadata::Orientation;
use quick_xml::escape::{escape, resolve_predefined_entity};
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::ResolveResult;
use quick_xml::NsReader;

use super::{Region, TagKind};

const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
const DC: &str = "http://purl.org/dc/elements/1.1/";
const LR: &str = "http://ns.adobe.com/lightroom/1.0/";
const IPTC_EXT: &str = "http://iptc.org/std/Iptc4xmpExt/2008-02-29/";
const MWG_RS: &str = "http://www.metadataworkinggroup.com/schemas/regions/";
const ST_AREA: &str = "http://ns.adobe.com/xmp/sType/Area#";

/// The properties the catalog owns, as (namespace, local name).
const OWNED: [(&str, &str); 4] = [(DC, "subject"), (LR, "hierarchicalSubject"), (IPTC_EXT, "PersonInImage"), (MWG_RS, "Regions")];

/// What a fresh sidecar starts as.
const EMPTY: &str = "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\" x:xmptk=\"Archive Studio\">
 <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">
 </rdf:RDF>
</x:xmpmeta>
";

/// `image`'s sidecar: `image` with `.xmp` appended.
pub fn sidecar_path(image: &Path) -> PathBuf {
    let mut path = image.as_os_str().to_owned();
    path.push(".xmp");
    path.into()
}

/// A tag to write.
pub(super) struct Tag {
    pub path: String,
    pub kind: TagKind,
    /// On the orientation-corrected image.
    pub region: Option<Region>,
}

/// The image's stored size and orientation: MWG regions are relative to the stored pixels.
#[derive(Clone, Copy)]
pub(super) struct Geometry {
    pub width: u32,
    pub height: u32,
    pub orientation: Orientation,
}

impl Geometry {
    /// `None` if the format can't be read (video, HEIC, …): its regions are left out.
    pub fn of(image: &Path) -> Option<Self> {
        use image::ImageDecoder;
        let mut decoder = image::ImageReader::open(image).ok()?.with_guessed_format().ok()?.into_decoder().ok()?;
        let (width, height) = decoder.dimensions();
        let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
        Some(Self { width, height, orientation })
    }
}

/// What to do with the sidecar.
#[derive(Debug, PartialEq)]
pub(super) enum Outcome {
    Write(String),
    /// It would hold nothing.
    Delete,
    /// There is none and none is needed.
    Nothing,
}

/// The new sidecar text for `existing` (`None`: no sidecar yet) holding `tags`. `ours` are
/// the tag paths written last time.
pub(super) fn merge(existing: Option<&str>, tags: &[Tag], geometry: Option<Geometry>, ours: &[String]) -> Result<Outcome, String> {
    if existing.is_none() && tags.is_empty() {
        return Ok(Outcome::Nothing);
    }
    let text = existing.unwrap_or(EMPTY);
    let parsed = parse(text)?;
    let rdf_end = parsed.rdf_end.ok_or("no rdf:RDF element")?;

    // Values others wrote: not ours from last time, and not about to be written again.
    let ours_leaves: Vec<&str> = ours.iter().map(|p| leaf(p)).collect();
    let was_ours = |value: &str, set: &[&str]| set.iter().any(|v| v.eq_ignore_ascii_case(value));
    let ours_paths: Vec<&str> = ours.iter().map(String::as_str).collect();
    let foreign = |key: (&str, &str), set: &[&str]| -> Vec<String> {
        parsed.values.get(&key).into_iter().flatten().filter(|v| !was_ours(v, set)).cloned().collect()
    };

    let mut subjects = foreign((DC, "subject"), &ours_leaves);
    let mut hierarchical = foreign((LR, "hierarchicalSubject"), &ours_paths);
    let mut people = foreign((IPTC_EXT, "PersonInImage"), &ours_leaves);
    let mut regions: Vec<String> =
        parsed.regions.iter().filter(|r| !was_ours(&r.name, &ours_leaves)).map(ForeignRegion::to_xml).collect();
    for tag in tags {
        push_unique(&mut subjects, leaf(&tag.path));
        push_unique(&mut hierarchical, &tag.path);
        if tag.kind == TagKind::Person {
            push_unique(&mut people, leaf(&tag.path));
            if let (Some(region), Some(geometry)) = (tag.region, geometry) {
                regions.push(face_region(leaf(&tag.path), stored_region(region, geometry.orientation)));
            }
        }
    }

    let mut description = String::new();
    bag(&mut description, "dc:subject", &subjects);
    bag(&mut description, "lr:hierarchicalSubject", &hierarchical);
    bag(&mut description, "Iptc4xmpExt:PersonInImage", &people);
    if !regions.is_empty() {
        description.push_str("   <mwg-rs:Regions rdf:parseType=\"Resource\">\n");
        if let Some(g) = geometry {
            description.push_str(&format!(
                "    <mwg-rs:AppliedToDimensions stDim:w=\"{}\" stDim:h=\"{}\" stDim:unit=\"pixel\"/>\n",
                g.width, g.height
            ));
        }
        description.push_str("    <mwg-rs:RegionList>\n     <rdf:Bag>\n");
        for region in &regions {
            description.push_str(region);
        }
        description.push_str("     </rdf:Bag>\n    </mwg-rs:RegionList>\n   </mwg-rs:Regions>\n");
    }

    if description.is_empty() && !parsed.has_other_content {
        return Ok(if existing.is_some() { Outcome::Delete } else { Outcome::Nothing });
    }

    // Splice: cut the removals (each with the whitespace before it), insert ours before `</rdf:RDF>`.
    let mut out = String::with_capacity(text.len() + description.len() + 512);
    let mut pos = 0;
    for &(start, end) in &parsed.cuts {
        out.push_str(&text[pos..trim_back(text, start).max(pos)]);
        pos = end;
    }
    let insert_at = trim_back(text, rdf_end).max(pos);
    out.push_str(&text[pos..insert_at]);
    if !description.is_empty() {
        out.push_str(&format!(
            "\n  <rdf:Description rdf:about=\"\"\n    xmlns:dc=\"{DC}\"\n    xmlns:lr=\"{LR}\"\n    \
             xmlns:Iptc4xmpExt=\"{IPTC_EXT}\"\n    xmlns:mwg-rs=\"{MWG_RS}\"\n    \
             xmlns:stDim=\"http://ns.adobe.com/xap/1.0/sType/Dimensions#\"\n    xmlns:stArea=\"{ST_AREA}\">\n"
        ));
        out.push_str(&description);
        out.push_str("  </rdf:Description>");
    }
    out.push_str(&text[insert_at..]);
    Ok(Outcome::Write(out))
}

fn push_unique(values: &mut Vec<String>, value: &str) {
    if !values.iter().any(|v| v.eq_ignore_ascii_case(value)) {
        values.push(value.to_owned());
    }
}

fn leaf(path: &str) -> &str {
    path.rsplit('|').next().unwrap_or(path)
}

fn bag(out: &mut String, property: &str, values: &[String]) {
    if values.is_empty() {
        return;
    }
    out.push_str(&format!("   <{property}>\n    <rdf:Bag>\n"));
    for value in values {
        out.push_str(&format!("     <rdf:li>{}</rdf:li>\n", escape(value.as_str())));
    }
    out.push_str(&format!("    </rdf:Bag>\n   </{property}>\n"));
}

/// MWG area: center and size, as fractions of the stored image.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Area {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

fn face_region(name: &str, area: Area) -> String {
    region_xml(name, "Face", area)
}

fn region_xml(name: &str, kind: &str, a: Area) -> String {
    format!(
        "      <rdf:li rdf:parseType=\"Resource\">\n       <mwg-rs:Name>{}</mwg-rs:Name>\n       \
         <mwg-rs:Type>{}</mwg-rs:Type>\n       <mwg-rs:Area stArea:x=\"{:.6}\" stArea:y=\"{:.6}\" \
         stArea:w=\"{:.6}\" stArea:h=\"{:.6}\" stArea:unit=\"normalized\"/>\n      </rdf:li>\n",
        escape(name),
        escape(kind),
        a.x,
        a.y,
        a.w,
        a.h
    )
}

/// `region` (top-left and size on the displayed image) as an MWG area on the stored image,
/// which `orientation` turns into the displayed one.
fn stored_region(region: Region, orientation: Orientation) -> Area {
    let (u, v) = (f64::from(region.x) + f64::from(region.w) / 2.0, f64::from(region.y) + f64::from(region.h) / 2.0);
    let (w, h) = (f64::from(region.w), f64::from(region.h));
    let ((x, y), swapped) = match orientation {
        Orientation::NoTransforms => ((u, v), false),
        Orientation::FlipHorizontal => ((1.0 - u, v), false),
        Orientation::Rotate180 => ((1.0 - u, 1.0 - v), false),
        Orientation::FlipVertical => ((u, 1.0 - v), false),
        Orientation::Rotate90 => ((v, 1.0 - u), true),
        Orientation::Rotate270 => ((1.0 - v, u), true),
        Orientation::Rotate90FlipH => ((v, u), true),
        Orientation::Rotate270FlipH => ((1.0 - v, 1.0 - u), true),
    };
    let (w, h) = if swapped { (h, w) } else { (w, h) };
    Area { x, y, w, h }
}

/// A region another program wrote, kept as its name, type and area (other fields, such as
/// program-specific extensions, are dropped).
struct ForeignRegion {
    name: String,
    kind: String,
    area: Option<Area>,
}

impl ForeignRegion {
    fn to_xml(&self) -> String {
        match self.area {
            Some(area) => region_xml(&self.name, &self.kind, area),
            None => format!(
                "      <rdf:li rdf:parseType=\"Resource\">\n       <mwg-rs:Name>{}</mwg-rs:Name>\n       \
                 <mwg-rs:Type>{}</mwg-rs:Type>\n      </rdf:li>\n",
                escape(self.name.as_str()),
                escape(self.kind.as_str())
            ),
        }
    }
}

struct Parsed {
    /// Byte ranges to cut, in order.
    cuts: Vec<(usize, usize)>,
    /// Where `</rdf:RDF>` starts.
    rdf_end: Option<usize>,
    /// The `rdf:li` values of each owned bag property.
    values: HashMap<(&'static str, &'static str), Vec<String>>,
    regions: Vec<ForeignRegion>,
    /// Anything left once the owned properties are cut.
    has_other_content: bool,
}

type Name = (String, String);

/// One top-level `rdf:li` inside an owned property: the values found in it, by the
/// (namespace, local name) of the attribute or innermost element holding them.
type Record = Vec<(Name, String)>;

fn resolved(result: ResolveResult) -> String {
    match result {
        ResolveResult::Bound(ns) => String::from_utf8_lossy(ns.as_ref()).into_owned(),
        _ => String::new(),
    }
}

fn is(name: &Name, ns: &str, local: &str) -> bool {
    name.0 == ns && name.1 == local
}

/// The `rdf:Description` being read.
struct Description {
    start: usize,
    cuts: Vec<(usize, usize)>,
    /// Holds something besides owned properties, so it stays.
    keep: bool,
}

/// The owned property being read.
struct Property {
    key: (&'static str, &'static str),
    start: usize,
    records: Vec<Record>,
    /// Depth in the element stack of the record's `rdf:li`, while in one.
    record_depth: Option<usize>,
}

struct Parser {
    parsed: Parsed,
    /// Open elements, outermost first.
    stack: Vec<Name>,
    description: Option<Description>,
    property: Option<Property>,
}

impl Parser {
    /// Whether the innermost open element is an `rdf:Description` directly under `rdf:RDF`.
    fn in_description(&self) -> bool {
        let n = self.stack.len();
        n >= 2 && is(&self.stack[n - 1], RDF, "Description") && is(&self.stack[n - 2], RDF, "RDF")
    }

    fn text(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        if let Some(p) = &mut self.property {
            if let (Some(_), Some(name), Some(record)) = (p.record_depth, self.stack.last(), p.records.last_mut()) {
                record.push((name.clone(), text.to_owned()));
            }
        } else if let Some(d) = &mut self.description {
            d.keep = true;
        }
    }

    fn open(&mut self, start: usize, name: Name, attrs: Vec<(Name, String)>) {
        let in_rdf = self.stack.last().is_some_and(|parent| is(parent, RDF, "RDF"));
        if in_rdf && is(&name, RDF, "Description") {
            let keep = attrs.iter().any(|(n, _)| !is(n, RDF, "about"));
            self.description = Some(Description { start, cuts: Vec::new(), keep });
        } else if in_rdf {
            // Something other than a description directly under rdf:RDF.
            self.parsed.has_other_content = true;
        } else if self.property.is_none() && self.in_description() {
            match OWNED.iter().find(|(ns, local)| is(&name, ns, local)) {
                Some(&key) => self.property = Some(Property { key, start, records: Vec::new(), record_depth: None }),
                None => {
                    if let Some(d) = &mut self.description {
                        d.keep = true;
                    }
                }
            }
        }
        if let Some(p) = &mut self.property {
            if p.record_depth.is_none() && is(&name, RDF, "li") {
                p.record_depth = Some(self.stack.len());
                p.records.push(Vec::new());
            }
            if p.record_depth.is_some() {
                p.records.last_mut().unwrap().extend(attrs);
            }
        }
        self.stack.push(name);
    }

    fn close(&mut self, start: usize, end: usize) {
        let Some(name) = self.stack.pop() else { return };
        let in_description = self.in_description();
        if let Some(p) = &mut self.property {
            if p.record_depth == Some(self.stack.len()) {
                p.record_depth = None;
            }
            if !(in_description && is(&name, p.key.0, p.key.1)) {
                return;
            }
            let p = self.property.take().unwrap();
            if let Some(d) = &mut self.description {
                d.cuts.push((p.start, end));
            }
            if p.key == (MWG_RS, "Regions") {
                self.parsed.regions.extend(p.records.iter().filter_map(region_of));
            } else {
                let values = self.parsed.values.entry(p.key).or_default();
                values.extend(p.records.into_iter().filter_map(|r| r.into_iter().find(|(n, _)| is(n, RDF, "li")).map(|(_, v)| v)));
            }
        } else if is(&name, RDF, "RDF") && self.parsed.rdf_end.is_none() {
            self.parsed.rdf_end = Some(start);
        } else if is(&name, RDF, "Description") && self.stack.last().is_some_and(|p| is(p, RDF, "RDF")) {
            if let Some(d) = self.description.take() {
                if d.keep {
                    self.parsed.has_other_content = true;
                    self.parsed.cuts.extend(d.cuts);
                } else {
                    self.parsed.cuts.push((d.start, end));
                }
            }
        }
    }
}

fn parse(text: &str) -> Result<Parsed, String> {
    let mut reader = NsReader::from_str(text);
    let mut parser = Parser {
        parsed: Parsed { cuts: Vec::new(), rdf_end: None, values: HashMap::new(), regions: Vec::new(), has_other_content: false },
        stack: Vec::new(),
        description: None,
        property: None,
    };
    let mut pending_text = String::new();
    loop {
        let start = reader.buffer_position() as usize;
        let error = |e: &dyn std::fmt::Display| format!("XML error at byte {start}: {e}");
        let (ns, event) = reader.read_resolved_event().map_err(|e| error(&e))?;
        let ns = resolved(ns);
        match event {
            Event::Text(t) => pending_text.push_str(&t.decode().map_err(|e| error(&e))?),
            Event::CData(t) => pending_text.push_str(&t.decode().map_err(|e| error(&e))?),
            Event::GeneralRef(r) => {
                if let Some(c) = r.resolve_char_ref().map_err(|e| error(&e))? {
                    pending_text.push(c);
                } else {
                    let name = r.decode().map_err(|e| error(&e))?;
                    pending_text.push_str(resolve_predefined_entity(&name).ok_or_else(|| error(&format!("unknown entity &{name};")))?);
                }
            }
            Event::Start(e) => {
                parser.text(&std::mem::take(&mut pending_text));
                let name = (ns, String::from_utf8_lossy(e.local_name().as_ref()).into_owned());
                parser.open(start, name, attributes(&reader, &e)?);
            }
            Event::Empty(e) => {
                parser.text(&std::mem::take(&mut pending_text));
                let name = (ns, String::from_utf8_lossy(e.local_name().as_ref()).into_owned());
                parser.open(start, name, attributes(&reader, &e)?);
                parser.close(start, reader.buffer_position() as usize);
            }
            Event::End(_) => {
                parser.text(&std::mem::take(&mut pending_text));
                parser.close(start, reader.buffer_position() as usize);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(parser.parsed)
}

fn attributes(reader: &NsReader<&[u8]>, e: &BytesStart) -> Result<Vec<(Name, String)>, String> {
    let mut out = Vec::new();
    for attr in e.attributes() {
        let attr = attr.map_err(|e| format!("XML attribute error: {e}"))?;
        // Namespace declarations aren't content.
        if attr.key.as_ref() == b"xmlns" || attr.key.prefix().is_some_and(|p| p.as_ref() == b"xmlns") {
            continue;
        }
        let (ns, local) = reader.resolver().resolve_attribute(attr.key);
        let name = (resolved(ns), String::from_utf8_lossy(local.as_ref()).into_owned());
        let value = attr.decoded_and_normalized_value(quick_xml::XmlVersion::Implicit1_0, reader.decoder()).map_err(|e| format!("XML attribute error: {e}"))?;
        out.push((name, value.into_owned()));
    }
    Ok(out)
}

/// A region record from `mwg-rs:RegionList`, if it has a name.
fn region_of(record: &Record) -> Option<ForeignRegion> {
    let get = |ns: &str, local: &str| record.iter().find(|(n, _)| is(n, ns, local)).map(|(_, v)| v.as_str());
    let name = get(MWG_RS, "Name")?.to_owned();
    let kind = get(MWG_RS, "Type").unwrap_or("Face").to_owned();
    let num = |local| get(ST_AREA, local).and_then(|v| v.trim().parse::<f64>().ok());
    let area = match (num("x"), num("y"), num("w"), num("h")) {
        (Some(x), Some(y), Some(w), Some(h)) => Some(Area { x, y, w, h }),
        _ => None,
    };
    Some(ForeignRegion { name, kind, area })
}

/// `pos` moved back over the whitespace before it.
fn trim_back(text: &str, pos: usize) -> usize {
    text[..pos].trim_end().len()
}

/// Extracts all tag paths and kinds from an XMP sidecar document. A flat keyword or name
/// (`dc:subject`, `PersonInImage`, a region's name) that is the last level of a
/// hierarchical one (`Alice` beside `People|Alice`, as these sidecars are written) is the
/// same tag and isn't listed again; `People|…` paths are people.
pub fn extract_tags(text: &str) -> Result<Vec<(String, TagKind)>, String> {
    let parsed = parse(text)?;
    let mut tags: Vec<(String, TagKind)> = Vec::new();
    let known = |tags: &[(String, TagKind)], name: &str| {
        tags.iter().any(|(path, _)| path.eq_ignore_ascii_case(name) || path.rsplit('|').next().is_some_and(|leaf| leaf.trim().eq_ignore_ascii_case(name.trim())))
    };
    if let Some(hierarchical) = parsed.values.get(&(LR, "hierarchicalSubject")) {
        for path in hierarchical {
            let first = path.split('|').next().unwrap_or_default().trim();
            let kind = if first.eq_ignore_ascii_case(super::people::PEOPLE) && path.contains('|') { TagKind::Person } else { TagKind::Keyword };
            tags.push((path.clone(), kind));
        }
    }
    if let Some(subjects) = parsed.values.get(&(DC, "subject")) {
        for s in subjects {
            if !known(&tags, s) {
                tags.push((s.clone(), TagKind::Keyword));
            }
        }
    }
    if let Some(people) = parsed.values.get(&(IPTC_EXT, "PersonInImage")) {
        for p in people {
            if !known(&tags, p) {
                tags.push((p.clone(), TagKind::Person));
            }
        }
    }
    for r in &parsed.regions {
        if !r.name.is_empty() && !known(&tags, &r.name) {
            tags.push((r.name.clone(), TagKind::Person));
        }
    }
    Ok(tags)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// As darktable writes it, with keywords of its own, plus digiKam-style face regions.
    const DARKTABLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="XMP Core 4.4.0-Exiv2">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    xmlns:darktable="http://darktable.sf.net/"
    xmlns:dc="http://purl.org/dc/elements/1.1/"
    xmlns:lr="http://ns.adobe.com/lightroom/1.0/"
    xmlns:mwg-rs="http://www.metadataworkinggroup.com/schemas/regions/"
    xmlns:stArea="http://ns.adobe.com/xmp/sType/Area#"
   xmp:Rating="3"
   darktable:xmp_version="5">
   <darktable:history>
    <rdf:Seq>
     <rdf:li darktable:operation="exposure" darktable:params="0000c03f"/>
    </rdf:Seq>
   </darktable:history>
   <dc:subject>
    <rdf:Bag>
     <rdf:li>format</rdf:li>
     <rdf:li>Tom &amp; Jerry</rdf:li>
    </rdf:Bag>
   </dc:subject>
   <lr:hierarchicalSubject>
    <rdf:Bag>
     <rdf:li>darktable|format|jpg</rdf:li>
    </rdf:Bag>
   </lr:hierarchicalSubject>
   <mwg-rs:Regions rdf:parseType="Resource">
    <mwg-rs:RegionList>
     <rdf:Bag>
      <rdf:li>
       <rdf:Description mwg-rs:Name="Carol" mwg-rs:Type="Face">
        <mwg-rs:Area stArea:x="0.25" stArea:y="0.5" stArea:w="0.1" stArea:h="0.2" stArea:unit="normalized"/>
       </rdf:Description>
      </rdf:li>
     </rdf:Bag>
    </mwg-rs:RegionList>
   </mwg-rs:Regions>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>
"#;

    fn tag(path: &str, kind: TagKind) -> Tag {
        Tag { path: path.into(), kind, region: None }
    }

    fn written(outcome: Outcome) -> String {
        match outcome {
            Outcome::Write(text) => text,
            other => panic!("{other:?}"),
        }
    }

    fn values(text: &str, key: (&'static str, &'static str)) -> Vec<String> {
        parse(text).unwrap().values.remove(&key).unwrap_or_default()
    }

    fn region_names(text: &str) -> Vec<String> {
        parse(text).unwrap().regions.into_iter().map(|r| r.name).collect()
    }

    const PLAIN: Geometry = Geometry { width: 400, height: 300, orientation: Orientation::NoTransforms };

    #[test]
    fn fresh_sidecar() {
        assert_eq!(merge(None, &[], None, &[]).unwrap(), Outcome::Nothing);
        let alice = Tag { region: Some(Region { x: 0.1, y: 0.2, w: 0.2, h: 0.4 }), ..tag("People|Alice", TagKind::Person) };
        let text = written(merge(None, &[alice, tag("Scenes|Beach", TagKind::Scene)], Some(PLAIN), &[]).unwrap());
        assert_eq!(values(&text, (DC, "subject")), ["Alice", "Beach"]);
        assert_eq!(values(&text, (LR, "hierarchicalSubject")), ["People|Alice", "Scenes|Beach"]);
        assert_eq!(values(&text, (IPTC_EXT, "PersonInImage")), ["Alice"]);
        let regions = parse(&text).unwrap().regions;
        assert_eq!(regions.len(), 1);
        assert_eq!((regions[0].name.as_str(), regions[0].kind.as_str()), ("Alice", "Face"));
        let a = regions[0].area.unwrap();
        // Center, not corner.
        assert!((a.x - 0.2).abs() < 1e-6 && (a.y - 0.4).abs() < 1e-6 && (a.w - 0.2).abs() < 1e-6 && (a.h - 0.4).abs() < 1e-6);
        assert!(text.contains(r#"stDim:w="400" stDim:h="300""#));
        // Regions need the image size; without it they are left out.
        let alice = Tag { region: Some(Region { x: 0.1, y: 0.2, w: 0.2, h: 0.4 }), ..tag("People|Alice", TagKind::Person) };
        assert!(region_names(&written(merge(None, &[alice], None, &[]).unwrap())).is_empty());
    }

    #[test]
    fn keeps_what_others_wrote() {
        let tags = [tag("People|Family|Alice", TagKind::Person), tag("Tom & Jerry", TagKind::Keyword)];
        let text = written(merge(Some(DARKTABLE), &tags, None, &[]).unwrap());
        // darktable's own content is untouched.
        for kept in [
            r#"<?xml version="1.0" encoding="UTF-8"?>"#,
            r#"xmp:Rating="3""#,
            r#"<rdf:li darktable:operation="exposure" darktable:params="0000c03f"/>"#,
        ] {
            assert!(text.contains(kept), "{kept}");
        }
        // Foreign values survive (once), ours are added.
        assert_eq!(values(&text, (DC, "subject")), ["format", "Tom & Jerry", "Alice"]);
        assert_eq!(values(&text, (LR, "hierarchicalSubject")), ["darktable|format|jpg", "People|Family|Alice", "Tom & Jerry"]);
        assert_eq!(region_names(&text), ["Carol"]);
        let carol = parse(&text).unwrap().regions.remove(0).area.unwrap();
        assert_eq!(carol, Area { x: 0.25, y: 0.5, w: 0.1, h: 0.2 });
        // The owned properties now appear once each.
        assert_eq!(text.matches("<dc:subject>").count(), 1);
        assert_eq!(text.matches("<mwg-rs:Regions").count(), 1);
        assert!(text.contains("Tom &amp; Jerry") && !text.contains("Tom & Jerry"));
    }

    #[test]
    fn rewriting_is_stable_and_removes_our_old_values() {
        let tags = [tag("People|Alice", TagKind::Person), tag("Dog", TagKind::Object)];
        let ours: Vec<String> = tags.iter().map(|t| t.path.clone()).collect();
        let once = written(merge(Some(DARKTABLE), &tags, None, &[]).unwrap());
        let twice = written(merge(Some(&once), &tags, None, &ours).unwrap());
        assert_eq!(once, twice);

        // Alice and Dog are taken off; only the foreign values stay.
        let removed = written(merge(Some(&twice), &[], None, &ours).unwrap());
        assert_eq!(values(&removed, (DC, "subject")), ["format", "Tom & Jerry"]);
        assert_eq!(values(&removed, (IPTC_EXT, "PersonInImage")), Vec::<String>::new());
        assert!(removed.contains("darktable:history"));

        // A sidecar holding only our tags goes away with them.
        let mine = written(merge(None, &tags, None, &[]).unwrap());
        assert_eq!(merge(Some(&mine), &[], None, &ours).unwrap(), Outcome::Delete);
        // Renaming replaces the old path instead of keeping it as foreign.
        let renamed = written(merge(Some(&mine), &[tag("Animals|Dog", TagKind::Object)], None, &ours).unwrap());
        assert_eq!(values(&renamed, (LR, "hierarchicalSubject")), ["Animals|Dog"]);
    }

    #[test]
    fn rejects_what_it_cannot_parse() {
        assert!(merge(Some("<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF"), &[tag("A", TagKind::Keyword)], None, &[]).is_err());
        assert!(merge(Some("<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"/>"), &[tag("A", TagKind::Keyword)], None, &[]).is_err());
    }

    #[test]
    fn regions_map_to_stored_pixels() {
        // A 6×4 stored image with one marked pixel; for each orientation, find where the
        // pixel is displayed and check the region mapped back lands on the original pixel.
        let (sw, sh) = (6u32, 4u32);
        let (px, py) = (1u32, 0u32);
        for orientation in [
            Orientation::NoTransforms,
            Orientation::FlipHorizontal,
            Orientation::Rotate180,
            Orientation::FlipVertical,
            Orientation::Rotate90,
            Orientation::Rotate270,
            Orientation::Rotate90FlipH,
            Orientation::Rotate270FlipH,
        ] {
            let mut img = image::RgbImage::new(sw, sh);
            img.put_pixel(px, py, image::Rgb([255, 0, 0]));
            let mut shown = image::DynamicImage::ImageRgb8(img);
            shown.apply_orientation(orientation);
            let shown = shown.to_rgb8();
            let (dx, dy, _) = shown.enumerate_pixels().find(|(_, _, p)| p.0[0] == 255).unwrap();
            let (dw, dh) = shown.dimensions();
            let region = Region {
                x: dx as f32 / dw as f32,
                y: dy as f32 / dh as f32,
                w: 1.0 / dw as f32,
                h: 1.0 / dh as f32,
            };
            let a = stored_region(region, orientation);
            let expect = Area {
                x: (px as f64 + 0.5) / sw as f64,
                y: (py as f64 + 0.5) / sh as f64,
                w: 1.0 / sw as f64,
                h: 1.0 / sh as f64,
            };
            for (got, want) in [(a.x, expect.x), (a.y, expect.y), (a.w, expect.w), (a.h, expect.h)] {
                assert!((got - want).abs() < 1e-6, "{orientation:?}: {a:?} != {expect:?}");
            }
        }
    }

    #[test]
    fn extracts_tags_from_xmp() {
        let tags = extract_tags(DARKTABLE).unwrap();
        let paths: Vec<&str> = tags.iter().map(|(p, _)| p.as_str()).collect();
        assert!(paths.contains(&"darktable|format|jpg"));
        assert!(paths.contains(&"format"));
        assert!(paths.contains(&"Tom & Jerry"));
        assert!(paths.contains(&"Carol"));
    }

    #[test]
    fn person_tags_are_not_listed_twice() {
        let ann = tag("People|Ann Lee", TagKind::Person);
        let Ok(Outcome::Write(text)) = merge(Some(DARKTABLE), &[ann], None, &[]) else { panic!("written") };
        assert!(text.contains("<rdf:li>Ann Lee</rdf:li>"), "the name is written flat too");
        let tags: Vec<_> = extract_tags(&text).unwrap().into_iter().filter(|(p, _)| p.contains("Ann")).collect();
        assert_eq!(tags, [("People|Ann Lee".to_owned(), TagKind::Person)]);
    }
}
