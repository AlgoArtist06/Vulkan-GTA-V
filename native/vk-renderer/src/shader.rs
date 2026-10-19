//! Native shader assets and the small WGSL reflection surface needed by the
//! command consumer.  The mirror's `index.json` and pack files are the source
//! of truth; malformed records are reported instead of treated as empty
//! shaders.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct ShaderAsset {
    pub source: String,
    pub constants: Option<Vec<u32>>,
    pub label: String,
}

#[derive(Clone, Debug)]
pub struct ShaderSourceStore {
    dir: PathBuf,
    index: Option<HashMap<String, ShaderRecord>>,
    packs: HashMap<usize, Vec<u8>>,
}

#[derive(Clone, Debug, Default)]
struct ShaderRecord {
    ok: bool,
    effect: String,
    program: String,
    pack: Option<[usize; 4]>,
    has_constants: bool,
}

impl ShaderSourceStore {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir, index: None, packs: HashMap::new() }
    }

    pub fn load(&mut self, hash: &str) -> Result<ShaderAsset, String> {
        self.load_index()?;
        let record = self.index.as_ref().and_then(|index| index.get(hash)).cloned();

        if record.as_ref().is_some_and(|record| !record.ok) {
            return Err(format!("shader {hash} is marked untranslated in index.json"));
        }

        let individual = self.dir.join(format!("{hash}.wgsl"));
        if individual.is_file() {
            let source = fs::read_to_string(&individual).map_err(|error| format!("read {}: {error}", individual.display()))?;
            if source.trim().is_empty() {
                return Err(format!("shader {hash} has an empty WGSL source"));
            }
            let constants = if record.as_ref().is_some_and(|record| record.has_constants) {
                let path = self.dir.join(format!("{hash}.consts.json"));
                Some(read_constants(&path)?)
            } else {
                None
            };
            return Ok(ShaderAsset { source, constants, label: label_for(hash, record.as_ref()) });
        }

        let record = record.ok_or_else(|| format!("shader {hash} is absent from index.json and has no individual WGSL file"))?;
        if !record.ok {
            return Err(format!("shader {hash} is marked untranslated in index.json"));
        }
        let packed = record.pack.ok_or_else(|| format!("shader {hash} has no pack range"))?;
        let bytes = self.pack_bytes(packed[0])?;
        let source_bytes = checked_range(&bytes, packed[1], packed[2], "WGSL")?;
        let source = String::from_utf8(source_bytes.to_vec()).map_err(|error| format!("shader {hash} WGSL is not UTF-8: {error}"))?;
        if source.trim().is_empty() {
            return Err(format!("shader {hash} has an empty packed WGSL source"));
        }
        let constants = if packed[3] == 0 {
            None
        } else {
            let bytes = checked_range(&bytes, packed[1].checked_add(packed[2]).ok_or_else(|| format!("shader {hash} pack range overflows"))?, packed[3], "constant table")?;
            Some(parse_constants(bytes).map_err(|error| format!("shader {hash} packed constants: {error}"))?)
        };
        Ok(ShaderAsset { source, constants, label: label_for(hash, Some(&record)) })
    }

    fn load_index(&mut self) -> Result<(), String> {
        if self.index.is_some() {
            return Ok(());
        }
        let path = self.dir.join("index.json");
        let value: serde_json::Value = match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).map_err(|error| format!("parse {}: {error}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.index = Some(HashMap::new());
                return Ok(());
            }
            Err(error) => return Err(format!("read {}: {error}", path.display())),
        };
        let object = value.as_object().ok_or_else(|| format!("{} is not a JSON object", path.display()))?;
        let mut index = HashMap::new();
        for (hash, record) in object {
            if hash.starts_with('_') {
                continue;
            }
            let Some(record) = record.as_object() else { continue };
            let ok = record.get("ok").and_then(serde_json::Value::as_bool).unwrap_or(false);
            let effect = record.get("effect").and_then(serde_json::Value::as_str).unwrap_or("unknown").to_string();
            let program = record.get("program").and_then(serde_json::Value::as_str).unwrap_or("unknown").to_string();
            let pack = parse_pack(record.get("p"))?;
            let has_constants = record.get("consts").and_then(serde_json::Value::as_bool).unwrap_or_else(|| pack.is_some_and(|pack| pack[3] != 0));
            index.insert(hash.clone(), ShaderRecord { ok, effect, program, pack, has_constants });
        }
        self.index = Some(index);
        Ok(())
    }

    fn pack_bytes(&mut self, pack: usize) -> Result<&[u8], String> {
        if !self.packs.contains_key(&pack) {
            let file = self
                .pack_file(pack)
                .ok_or_else(|| format!("pack {pack} is not listed in index.json"))?;
            let bytes = fs::read(&file).map_err(|error| format!("read {}: {error}", file.display()))?;
            self.packs.insert(pack, bytes);
        }
        Ok(self.packs.get(&pack).expect("pack inserted").as_slice())
    }

    fn pack_file(&self, pack: usize) -> Option<PathBuf> {
        let value = fs::read_to_string(self.dir.join("index.json")).ok().and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())?;
        let packs = value.get("_packs")?.as_array()?;
        let entry = packs.get(pack)?.as_object()?;
        let file = entry.get("file")?.as_str()?;
        Some(self.dir.join(file))
    }
}

fn label_for(hash: &str, record: Option<&ShaderRecord>) -> String {
    record.map(|record| format!("{}::{} ({hash})", record.effect, record.program)).unwrap_or_else(|| hash.to_string())
}

fn parse_pack(value: Option<&serde_json::Value>) -> Result<Option<[usize; 4]>, String> {
    let Some(value) = value else { return Ok(None) };
    let Some(values) = value.as_array() else { return Err("shader pack descriptor is not an array".to_string()) };
    if values.len() != 4 {
        return Err(format!("shader pack descriptor has {} fields, expected 4", values.len()));
    }
    let mut out = [0usize; 4];
    for (index, value) in values.iter().enumerate() {
        let number = value.as_u64().ok_or_else(|| format!("shader pack field {index} is not an unsigned integer"))?;
        out[index] = usize::try_from(number).map_err(|_| format!("shader pack field {index} does not fit host usize"))?;
    }
    Ok(Some(out))
}

fn checked_range<'a>(bytes: &'a [u8], offset: usize, size: usize, kind: &str) -> Result<&'a [u8], String> {
    let end = offset.checked_add(size).ok_or_else(|| format!("{kind} range overflows"))?;
    bytes.get(offset..end).ok_or_else(|| format!("{kind} range {offset}..{end} exceeds pack size {}", bytes.len()))
}

fn read_constants(path: &Path) -> Result<Vec<u32>, String> {
    let text = fs::read_to_string(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|error| format!("parse {}: {error}", path.display()))?;
    parse_constants_value(&value)
}

fn parse_constants(bytes: &[u8]) -> Result<Vec<u32>, String> {
    let text = std::str::from_utf8(bytes).map_err(|error| error.to_string())?;
    let value: serde_json::Value = serde_json::from_str(text).map_err(|error| error.to_string())?;
    parse_constants_value(&value)
}

fn parse_constants_value(value: &serde_json::Value) -> Result<Vec<u32>, String> {
    let values = value.as_array().ok_or_else(|| "constant table is not an array".to_string())?;
    values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let value = value.as_u64().ok_or_else(|| format!("constant table value {index} is not an unsigned integer"))?;
            u32::try_from(value).map_err(|_| format!("constant table value {index} exceeds u32"))
        })
        .collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum BindingKind {
    Uniform,
    Storage { read_only: bool },
    Sampler { comparison: bool },
    Texture { sample: TextureSample, dimension: TextureDimension, multisampled: bool },
    StorageTexture { format: StorageFormat, access: StorageAccess, dimension: TextureDimension },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum TextureSample {
    Float { filterable: bool },
    Sint,
    Uint,
    Depth,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum TextureDimension { D1, D2, D2Array, D3, Cube, CubeArray }

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum StorageAccess { WriteOnly, ReadOnly, ReadWrite }

pub use crate::shader_storage::StorageFormat;
use crate::shader_storage::parse_storage_format;

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BindingReflection {
    pub group: u32,
    pub binding: u32,
    pub name: String,
    pub kind: BindingKind,
    /// Bytes of a uniform binding's `array<vec4<..>, N>` struct (D3D constant registers); 0 otherwise.
    pub size: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ShaderReflection {
    pub bindings: Vec<BindingReflection>,
    pub fragment_outputs: Vec<u32>,
    pub fragment_inputs: Vec<(u32, String)>,
    pub uses_vertex_index: bool,
    pub uses_instance_index: bool,
    /// (sampler binding, texture binding) pairs of the shader's `textureSample*` calls.
    pub sampled_with: Vec<(u32, u32)>,
}

pub fn reflect_wgsl(source: &str) -> Result<ShaderReflection, String> {
    let mut reflection = ShaderReflection {
        uses_vertex_index: source.contains("vertex_index"),
        uses_instance_index: source.contains("instance_index"),
        ..ShaderReflection::default()
    };
    let mut cursor = 0;
    while let Some(relative) = source[cursor..].find("@group(") {
        let group_start = cursor + relative;
        let (group, after_group) = parse_attribute_number(source, group_start, "@group(")?;
        let binding_start = source[after_group..].find("@binding(").map(|offset| after_group + offset).ok_or_else(|| "WGSL group attribute has no binding attribute".to_string())?;
        let (binding, after_binding) = parse_attribute_number(source, binding_start, "@binding(")?;
        let var_start = source[after_binding..].find("var").map(|offset| after_binding + offset).ok_or_else(|| "WGSL binding attribute has no var declaration".to_string())?;
        let (kind_text, name, type_text, after_decl) = parse_var_decl(&source[var_start..])?;
        let kind = binding_kind(kind_text.as_deref(), &type_text)?;
        let size = if kind == BindingKind::Uniform { uniform_size(source, &type_text)? } else { 0 };
        reflection.bindings.push(BindingReflection { group, binding, name, kind, size });
        cursor = var_start + after_decl;
    }
    reflection.sampled_with = sampled_with(source, &reflection.bindings);
    reflection.fragment_outputs = fragment_outputs(source);
    reflection.fragment_inputs = fragment_inputs(source);
    Ok(reflection)
}

/// Which samplers each texture is sampled with: `textureSample*(texture, sampler, ...)`.
fn sampled_with(source: &str, bindings: &[BindingReflection]) -> Vec<(u32, u32)> {
    let binding = |name: &str| bindings.iter().find(|b| b.name == name).map(|b| b.binding);
    let mut pairs = Vec::new();
    for (at, _) in source.match_indices("textureSample") {
        let Some(open) = source[at..].find('(').map(|offset| at + offset + 1) else { continue };
        let mut args = source[open..].split(',').map(str::trim);
        let (Some(texture), Some(sampler)) = (args.next(), args.next()) else { continue };
        if let (Some(texture), Some(sampler)) = (binding(texture), binding(sampler)) {
            if !pairs.contains(&(sampler, texture)) { pairs.push((sampler, texture)); }
        }
    }
    pairs
}

/// The parameter list and the text between `)` and `{` of the stage's `main`.
fn main_signature<'a>(source: &'a str, stage: &str) -> Option<(&'a str, &'a str)> {
    let stage = source.find(stage)?;
    let open = stage + source[stage..].find("fn main(")? + "fn main(".len();
    let mut depth = 1;
    for (offset, byte) in source[open..].bytes().enumerate() {
        match byte {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    let close = open + offset;
                    let body = close + source[close..].find('{')?;
                    return Some((&source[open..close], &source[close + 1..body]));
                }
            }
            _ => {}
        }
    }
    None
}

/// Calls `visit(location, attribute_end, interpolate)` for each `@location(N)`,
/// where `interpolate` is the body and end of a directly following `@interpolate(...)`.
fn visit_locations(text: &str, mut visit: impl FnMut(u32, usize, Option<(&str, usize)>)) {
    let mut cursor = 0;
    while let Some(relative) = text[cursor..].find("@location(") {
        let start = cursor + relative + "@location(".len();
        let Some(close) = text[start..].find(')').map(|offset| start + offset) else { break };
        cursor = close + 1;
        let Ok(location) = text[start..close].trim().trim_end_matches('u').parse() else { continue };
        let next = cursor + (text.len() - cursor - text[cursor..].trim_start().len());
        let interpolate = text[next..].strip_prefix("@interpolate(").and_then(|rest| {
            let end = rest.find(')')?;
            Some((rest[..end].trim(), next + "@interpolate(".len() + end + 1))
        });
        visit(location, cursor, interpolate);
    }
}

/// Pixel-shader inputs with an explicit `@interpolate`, e.g. `(3, "flat")`.
fn fragment_inputs(source: &str) -> Vec<(u32, String)> {
    let Some((parameters, _)) = main_signature(source, "@fragment") else { return Vec::new() };
    let mut inputs = Vec::new();
    visit_locations(parameters, |location, _, interpolate| {
        if let Some((attribute, _)) = interpolate { inputs.push((location, attribute.to_string())); }
    });
    inputs
}

/// HLSL declares `nointerpolation` and `sample` only on the pixel shader input,
/// but wgpu requires both stages to agree on interpolation and sampling. Copies
/// each pixel input's `@interpolate` onto the vertex output with that location.
pub fn vertex_for_fragment_inputs(source: &str, inputs: &[(u32, String)]) -> Result<String, String> {
    let (_, result) = main_signature(source, "@vertex").ok_or("vertex shader has no main entry point")?;
    let name = result.trim().strip_prefix("->").map(str::trim).unwrap_or_default();
    if name.is_empty() || !name.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_') {
        return Err(format!("vertex main returns `{}`, not an output struct that pixel inputs {inputs:?} can match", result.trim()));
    }
    let header = format!("struct {name} {{");
    let start = source.find(&header).ok_or_else(|| format!("vertex output struct {name} is not declared"))? + header.len();
    let end = start + source[start..].find('}').ok_or_else(|| format!("vertex output struct {name} is not closed"))?;
    let body = &source[start..end];
    let mut out = String::with_capacity(source.len() + inputs.len() * 32);
    out.push_str(&source[..start]);
    let mut cursor = 0;
    visit_locations(body, |location, attribute_end, interpolate| {
        let Some((_, attribute)) = inputs.iter().find(|(input, _)| *input == location) else { return };
        out.push_str(&body[cursor..attribute_end]);
        out.push_str(&format!(" @interpolate({attribute})"));
        cursor = interpolate.map_or(attribute_end, |(_, end)| end);
    });
    out.push_str(&body[cursor..]);
    out.push_str(&source[end..]);
    Ok(out)
}

fn uniform_size(source: &str, type_name: &str) -> Result<u64, String> {
    let header = format!("struct {type_name} {{");
    let start = source.find(&header).ok_or_else(|| format!("uniform struct {type_name} is not declared"))? + header.len();
    let body = &source[start..start + source[start..].find('}').ok_or_else(|| format!("uniform struct {type_name} is not closed"))?];
    let Some(array) = body.find("array<vec4<") else {
        // A packed struct (`pack_uniforms`): its members are register arrays back to back.
        let mut size = 0;
        for member in body.split(',').map(str::trim).filter(|member| !member.is_empty()) {
            let (_, member_type) = member.split_once(':').ok_or_else(|| format!("uniform struct {type_name} member `{member}` has no type"))?;
            size += uniform_size(source, member_type.trim())?;
        }
        return if size == 0 { Err(format!("uniform struct {type_name} is not an array of vec4 registers")) } else { Ok(size) };
    };
    let count = body[array..].split(',').nth(1).map(|count| count.trim().trim_end_matches(|c: char| c == '>' || c.is_whitespace()).trim_end_matches('u'))
        .ok_or_else(|| format!("uniform struct {type_name} has no register count"))?;
    count.parse::<u64>().map(|count| count * 16).map_err(|error| format!("uniform struct {type_name} register count: {error}"))
}

pub const PACK_BINDING: u32 = 0;
/// The packed constants live in group (stage group + this): VS 2, PS 3, compute 2.
pub const PACK_GROUP_OFFSET: u32 = 0;

/// One D3D constant buffer inside a stage's packed uniform block.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PackMember {
    pub binding: u32,
    pub offset: u64,
    pub size: u64,
}

/// The browser worker's packUniforms: every `var<uniform>` of a stage except
/// the constant table (binding 15) becomes a member of one struct at binding 0,
/// in binding order, so a pipeline needs one dynamic offset per stage. The
/// renderer copies the bound D3D buffers back to back into that block.
pub fn pack_uniforms(source: &str) -> Result<Option<(String, Vec<PackMember>)>, String> {
    let mut declarations = Vec::new();
    let mut cursor = 0;
    while let Some(relative) = source[cursor..].find("var<uniform>") {
        let at = cursor + relative;
        let start = source[..at].rfind("@group(").ok_or("uniform declaration has no group attribute")?;
        let end = at + source[at..].find(';').ok_or("uniform declaration has no semicolon")? + 1;
        let (group, after_group) = parse_attribute_number(source, start, "@group(")?;
        let binding_start = after_group + source[after_group..].find("@binding(").ok_or("uniform declaration has no binding attribute")?;
        let (binding, _) = parse_attribute_number(source, binding_start, "@binding(")?;
        let (_, name, type_text, _) = parse_var_decl(&source[at..])?;
        if binding != 15 { declarations.push((start, end, group, binding, name, type_text)); }
        cursor = end;
    }
    let Some(&(first, _, group, ..)) = declarations.first() else { return Ok(None) };
    if declarations.iter().any(|declaration| declaration.2 != group) { return Err("uniform declarations span several groups".to_string()); }
    let mut members: Vec<(u32, String, String)> = declarations.iter().map(|(_, _, _, binding, name, type_text)| (*binding, name.clone(), type_text.clone())).collect();
    members.sort_by_key(|member| member.0);
    let mut packed = Vec::with_capacity(members.len());
    let mut offset = 0;
    for (binding, _, type_text) in &members {
        let size = uniform_size(source, type_text)?;
        packed.push(PackMember { binding: *binding, offset, size });
        offset += size;
    }
    let mut text = String::with_capacity(source.len() + 256);
    let mut copied = 0;
    for (start, end, ..) in &declarations {
        text.push_str(&source[copied..*start]);
        if *start == first {
            text.push_str("struct cbpack_t {\n");
            for (_, name, type_text) in &members { text.push_str(&format!("  {name} : {type_text},\n")); }
            // Two groups above the stage's resources: per-draw constants get their own bind group.
            text.push_str(&format!("}}\n@group({}u) @binding({PACK_BINDING}u) var<uniform> cbpack_v : cbpack_t;", group + PACK_GROUP_OFFSET));
        }
        copied = *end;
    }
    text.push_str(&source[copied..]);
    for (_, name, _) in &members { text = replace_identifier(&text, name, &format!("cbpack_v.{name}")); }
    // The struct's own member names were renamed with the rest: restore them.
    for (_, name, type_text) in &members { text = text.replace(&format!("  cbpack_v.{name} : {type_text},"), &format!("  {name} : {type_text},")); }
    Ok(Some((text, packed)))
}

/// Moves module-scope `var<private>` registers that only `main_inner` uses
/// into that function as locals. The translated shaders keep every D3D
/// register as a private global; a function-local variable is what GPU
/// compilers reliably keep in registers instead of per-pipeline scratch memory.
/// Both start at the type's zero value and `main_inner` runs once per
/// invocation, so the shader computes the same results.
pub fn localize_registers(source: &str) -> String {
    let Some(signature) = source.find("fn main_inner(") else { return source.to_string() };
    let Some(open) = source[signature..].find('{').map(|offset| signature + offset) else { return source.to_string() };
    let mut depth = 0usize;
    let mut close = None;
    for (offset, byte) in source.as_bytes()[open..].iter().enumerate() {
        match byte {
            b'{' => depth += 1,
            b'}' => { depth -= 1; if depth == 0 { close = Some(open + offset); break; } }
            _ => {}
        }
    }
    let Some(close) = close else { return source.to_string() };
    let mut moved = Vec::new();
    let mut lines = Vec::new();
    for line in source[..signature].split_inclusive('\n') {
        let declaration = line.trim().strip_prefix("var<private> ").and_then(|rest| rest.strip_suffix(';'));
        let local = declaration.and_then(|declaration| {
            let (name, _) = declaration.split_once(':')?;
            let name = name.trim();
            // Its own declaration is the one use before main_inner; none may follow it.
            (occurrences(&source[..signature], name) == 1 && occurrences(&source[close..], name) == 0).then_some(declaration)
        });
        match local {
            Some(declaration) => moved.push(format!("  var {declaration};\n")),
            None => lines.push(line),
        }
    }
    if moved.is_empty() { return source.to_string() }
    let mut text = String::with_capacity(source.len());
    for line in lines { text.push_str(line); }
    text.push_str(&source[signature..=open]);
    text.push('\n');
    for declaration in moved { text.push_str(&declaration); }
    text.push_str(&source[open + 1..]);
    text
}

/// Whole-identifier occurrences of `name` in `text` (not as a member after `.`).
fn occurrences(text: &str, name: &str) -> usize {
    let word = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_';
    let bytes = text.as_bytes();
    text.match_indices(name).filter(|(at, _)| {
        let end = at + name.len();
        (*at == 0 || !word(bytes[at - 1]) && bytes[at - 1] != b'.') && bytes.get(end).is_none_or(|byte| !word(*byte))
    }).count()
}

fn replace_identifier(text: &str, name: &str, replacement: &str) -> String {
    let word = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_';
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    while let Some(relative) = text[cursor..].find(name) {
        let at = cursor + relative;
        let end = at + name.len();
        let bounded = (at == 0 || !word(text.as_bytes()[at - 1]) && text.as_bytes()[at - 1] != b'.') && text.as_bytes().get(end).is_none_or(|byte| !word(*byte));
        out.push_str(&text[cursor..at]);
        out.push_str(if bounded { replacement } else { name });
        cursor = end;
    }
    out.push_str(&text[cursor..]);
    out
}

fn parse_attribute_number(source: &str, start: usize, prefix: &str) -> Result<(u32, usize), String> {
    let body_start = start.checked_add(prefix.len()).ok_or_else(|| "WGSL attribute offset overflow".to_string())?;
    let close = source[body_start..].find(')').map(|offset| body_start + offset).ok_or_else(|| format!("unterminated WGSL attribute {prefix}"))?;
    let value = source[body_start..close].trim().trim_end_matches('u').parse::<u32>().map_err(|error| format!("invalid WGSL attribute number: {error}"))?;
    Ok((value, close + 1))
}

fn parse_var_decl(source: &str) -> Result<(Option<String>, String, String, usize), String> {
    let mut cursor = 3;
    while source.as_bytes().get(cursor).is_some_and(u8::is_ascii_whitespace) { cursor += 1; }
    let space = if source.as_bytes().get(cursor) == Some(&b'<') {
        let close = source[cursor + 1..].find('>').map(|offset| cursor + 1 + offset).ok_or_else(|| "unterminated WGSL address space".to_string())?;
        let text = source[cursor + 1..close].replace(char::is_whitespace, "");
        cursor = close + 1;
        Some(text)
    } else { None };
    while source.as_bytes().get(cursor).is_some_and(u8::is_ascii_whitespace) { cursor += 1; }
    let name_start = cursor;
    while source.as_bytes().get(cursor).is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_') { cursor += 1; }
    if cursor == name_start { return Err("WGSL binding has no variable name".to_string()) }
    let name = source[name_start..cursor].to_string();
    while source.as_bytes().get(cursor).is_some_and(u8::is_ascii_whitespace) { cursor += 1; }
    if source.as_bytes().get(cursor) != Some(&b':') { return Err(format!("WGSL binding {name} has no type")) }
    cursor += 1;
    let type_start = cursor;
    while source.as_bytes().get(cursor).is_some_and(|byte| *byte != b';') { cursor += 1; }
    if source.as_bytes().get(cursor) != Some(&b';') { return Err(format!("WGSL binding {name} has no semicolon")) }
    Ok((space, name, source[type_start..cursor].trim().to_string(), cursor + 1))
}

fn binding_kind(space: Option<&str>, type_text: &str) -> Result<BindingKind, String> {
    if space == Some("uniform") { return Ok(BindingKind::Uniform) }
    if let Some(space) = space {
        if space.starts_with("storage") {
            return Ok(BindingKind::Storage { read_only: !space.contains("read_write") });
        }
    }
    if type_text == "sampler" { return Ok(BindingKind::Sampler { comparison: false }) }
    if type_text == "sampler_comparison" { return Ok(BindingKind::Sampler { comparison: true }) }
    if let Some(rest) = type_text.strip_prefix("texture_storage_") {
        let (dimension, args) = rest.split_once('<').ok_or_else(|| format!("storage texture type {type_text} has no arguments"))?;
        let args = args.strip_suffix('>').ok_or_else(|| format!("storage texture type {type_text} has no closing angle"))?;
        let mut args = args.split(',').map(str::trim);
        let format = parse_storage_format(args.next().ok_or_else(|| format!("storage texture type {type_text} has no format"))?)?;
        let access = match args.next().unwrap_or("write") { "write" | "write_only" => StorageAccess::WriteOnly, "read" | "read_only" => StorageAccess::ReadOnly, "read_write" => StorageAccess::ReadWrite, value => return Err(format!("unsupported storage texture access {value}")) };
        return Ok(BindingKind::StorageTexture { format, access, dimension: parse_dimension(dimension)? });
    }
    let Some(rest) = type_text.strip_prefix("texture_") else { return Err(format!("unsupported WGSL binding type {type_text}")) };
    let (dimension_name, arg) = rest.split_once('<').map_or((rest, "f32"), |(dim, args)| (dim, args.trim_end_matches('>')));
    let multisampled = dimension_name == "multisampled_2d";
    let dimension_name = dimension_name.strip_prefix("multisampled_").unwrap_or(dimension_name);
    let dimension = parse_dimension(dimension_name)?;
    let sample = if dimension_name.starts_with("depth_") { TextureSample::Depth } else { match arg.trim() { "f32" => TextureSample::Float { filterable: true }, "i32" => TextureSample::Sint, "u32" => TextureSample::Uint, value => return Err(format!("unsupported texture sample type {value}")) } };
    Ok(BindingKind::Texture { sample, dimension, multisampled })
}

fn parse_dimension(value: &str) -> Result<TextureDimension, String> {
    match value.strip_prefix("depth_").unwrap_or(value) {
        "1d" => Ok(TextureDimension::D1), "2d" => Ok(TextureDimension::D2), "2d_array" => Ok(TextureDimension::D2Array), "3d" => Ok(TextureDimension::D3), "cube" => Ok(TextureDimension::Cube), "cube_array" => Ok(TextureDimension::CubeArray), value => Err(format!("unsupported WGSL texture dimension {value}")),
    }
}



fn fragment_outputs(source: &str) -> Vec<u32> {
    let Some(fragment) = source.find("@fragment") else { return Vec::new() };
    let Some(main) = source[fragment..].find("fn main") else { return Vec::new() };
    let main = fragment + main;
    let Some(open) = source[main..].find('{') else { return Vec::new() };
    let header = &source[main..main + open];
    let mut outputs = locations(header);
    if outputs.is_empty() {
        if let Some(arrow) = header.find("->") {
            let name = header[arrow + 2..].split_whitespace().next().unwrap_or_default();
            if let Some(structure) = source.find(&format!("struct {name}")) {
                if let Some(end) = source[structure..].find('}') {
                    outputs = locations(&source[structure..structure + end]);
                }
            }
        }
    }
    outputs.sort_unstable();
    outputs.dedup();
    outputs
}

fn locations(source: &str) -> Vec<u32> {
    let mut out = Vec::new();
    let mut cursor = 0;
    while let Some(relative) = source[cursor..].find("@location(") {
        let start = cursor + relative + "@location(".len();
        let Some(close) = source[start..].find(')') else { break };
        if let Ok(value) = source[start..start + close].trim().trim_end_matches('u').parse() { out.push(value); }
        cursor = start + close + 1;
    }
    out
}


