//! Lossless reconstruction of v3 storage envelopes. User JSON is always literal data.
use super::*;
use std::collections::{HashMap, HashSet};

fn check_entity(e: &Entity) -> Result<()> {
    let mut checked = e.clone();
    checked.identify();
    ensure!(checked.id == e.id, "entity staging identity mismatch");
    ensure!(
        e.attributes["kind"]["value"] == e.kind,
        "entity kind mismatch"
    );
    Ok(())
}
/// Read a logical payload, verifying any chunk manifest and SHA-256 integrity.
pub fn payload_with<F>(entity: &Entity, resolve: &mut F) -> Result<Vec<u8>>
where
    F: FnMut(&str, &str, Option<u64>) -> Result<Entity>,
{
    check_entity(entity)?;
    if entity.content_type != "application/json" {
        return Ok(entity.payload.clone());
    }
    let v: Value = serde_json::from_slice(&entity.payload)?;
    let Some(spill) = v
        .as_object()
        .filter(|m| m.len() == 1)
        .and_then(|m| m.get("$spill"))
    else {
        return Ok(entity.payload.clone());
    };
    let hash = spill["hash"].as_str().context("spill hash missing")?;
    let parts = spill["parts"].as_u64().context("spill parts missing")?;
    let size = spill["bytes"].as_u64().context("spill size missing")?;
    ensure!(
        parts > 0 && size > BUDGET as u64 && parts == size.div_ceil(CHUNK as u64),
        "invalid spill dimensions"
    );
    let mut result = Vec::new();
    for part in 0..parts {
        let blob = resolve("blob", hash, Some(part))?;
        check_entity(&blob)?;
        ensure!(
            blob.kind == "blob"
                && blob.attributes["hash"]["value"] == hash
                && blob.attributes["part"]["value"] == part
                && blob.attributes["parts"]["value"] == parts,
            "blob reference mismatch"
        );
        let expected = if part + 1 == parts {
            size - (parts - 1) * CHUNK as u64
        } else {
            CHUNK as u64
        };
        ensure!(
            blob.payload.len() as u64 == expected && blob.attributes["size"]["value"] == expected,
            "blob size mismatch"
        );
        result.extend_from_slice(&blob.payload);
    }
    ensure!(
        result.len() as u64 == size && hx(sha(&result)) == hash,
        "spill integrity mismatch"
    );
    Ok(result)
}
/// Hydrate one contract using shared entities resolved by kind, content hash and optional blob part.
pub fn hydrate_with<F>(contract: &Entity, resolve: F) -> Result<Value>
where
    F: FnMut(&str, &str, Option<u64>) -> Result<Entity>,
{
    let mut state = State::new(resolve, || Ok(()));
    state.full = true;
    let envelope = state.contract(contract)?;
    state
        .expand_at(&envelope, "", 0)?
        .context("missing envelope data")
}
/// Hydrate only the requested top-level fields, omitting fields absent from storage.
///
/// Field names are literal keys, not JSON pointers or nested selectors. Dependencies
/// of unselected fields are never resolved. A selected value still requires full
/// integrity verification of its logical payload, including every spill blob.
pub fn hydrate_fields_with<F>(contract: &Entity, fields: &[&str], resolve: F) -> Result<Value>
where
    F: FnMut(&str, &str, Option<u64>) -> Result<Entity>,
{
    hydrate_fields_with_check(contract, fields, resolve, || Ok(()))
}
/// Selective hydration with cooperative cancellation or work-budget checkpoints.
///
/// `check` runs before dependency reads and during reference traversal. Callers
/// should additionally bound entity sizes: parsing, hashing and cloning one JSON
/// value are indivisible operations between checkpoints.
pub fn hydrate_fields_with_check<F, C>(
    contract: &Entity,
    fields: &[&str],
    resolve: F,
    check: C,
) -> Result<Value>
where
    F: FnMut(&str, &str, Option<u64>) -> Result<Entity>,
    C: FnMut() -> Result<()>,
{
    let mut state = State::new(resolve, check);
    let envelope = state.contract(contract)?;
    let mut result = Map::new();
    for field in fields {
        (state.check)()?;
        if result.contains_key(*field) {
            continue;
        }
        let path = format!("/{}", field.replace('~', "~0").replace('/', "~1"));
        if let Some(value) = state.expand_at(&envelope, &path, 0)? {
            result.insert((*field).into(), value);
        }
    }
    (state.check)()?;
    Ok(Value::Object(result))
}
/// Hydrate from an in-memory logical entity collection.
pub fn hydrate(contract: &Entity, entities: &[Entity]) -> Result<Value> {
    hydrate_with(contract, |kind, hash, part| {
        let mut candidates = entities.iter().filter(|e| {
            e.kind == kind
                && e.attributes["hash"]["value"] == hash
                && part.is_none_or(|p| e.attributes["part"]["value"] == p)
        });
        let found = candidates.next().context("missing referenced entity")?;
        for other in candidates {
            ensure!(other.id == found.id, "ambiguous referenced entity");
        }
        Ok(found.clone())
    })
}

struct Envelope {
    data: Value,
    references: Vec<Value>,
}
#[derive(Clone)]
enum Shared {
    Scalar(Value),
    Compilation(std::rc::Rc<Envelope>),
}
type Locator = (String, String, Option<u64>);
struct State<F, C> {
    resolve: F,
    check: C,
    entities: HashMap<Locator, Entity>,
    shared: HashMap<String, Shared>,
    expanded: HashMap<(String, String), Value>,
    active: HashSet<String>,
    full: bool,
}
impl<F, C> State<F, C>
where
    F: FnMut(&str, &str, Option<u64>) -> Result<Entity>,
    C: FnMut() -> Result<()>,
{
    fn new(resolve: F, check: C) -> Self {
        Self {
            resolve,
            check,
            entities: HashMap::new(),
            shared: HashMap::new(),
            expanded: HashMap::new(),
            active: HashSet::new(),
            full: false,
        }
    }
    fn resolve(&mut self, kind: &str, hash: &str, part: Option<u64>) -> Result<Entity> {
        (self.check)()?;
        let key = (kind.into(), hash.into(), part);
        if let Some(entity) = self.entities.get(&key) {
            return Ok(entity.clone());
        }
        let entity = (self.resolve)(kind, hash, part)?;
        self.entities.insert(key, entity.clone());
        Ok(entity)
    }
    fn payload(&mut self, entity: &Entity) -> Result<Vec<u8>> {
        (self.check)()?;
        let payload = payload_with(entity, &mut |kind, hash, part| {
            self.resolve(kind, hash, part)
        })?;
        (self.check)()?;
        Ok(payload)
    }
    fn contract(&mut self, contract: &Entity) -> Result<Envelope> {
        ensure!(
            contract.kind == "verified_contract",
            "expected verified_contract"
        );
        let payload = self.payload(contract)?;
        let envelope: Value = serde_json::from_slice(&payload)?;
        ensure!(
            envelope["schema"] == "sourcify.verified-contract.v3",
            "unsupported contract schema"
        );
        self.envelope(envelope)
    }
    fn envelope(&mut self, mut envelope: Value) -> Result<Envelope> {
        let data = envelope
            .get_mut("data")
            .context("missing envelope data")?
            .take();
        ensure!(data.is_object(), "envelope data must be object");
        let references = envelope["references"]
            .as_array_mut()
            .context("missing references")?;
        let references = std::mem::take(references);
        let mut paths = HashSet::new();
        for r in &references {
            (self.check)()?;
            let path = r["path"].as_str().context("missing reference path")?;
            validate_pointer(path, false)?;
            ensure!(paths.insert(path), "invalid or duplicate reference path");
            let kind = r["kind"].as_str().context("missing reference kind")?;
            if kind == "local" {
                ensure!(
                    envelope["schema"] == "sourcify.compilation.v3",
                    "local references require compilation"
                );
                let pointer = r["pointer"].as_str().context("missing local pointer")?;
                validate_pointer(pointer, false)?;
                let is_std = |s: &str| {
                    s == "/stdJsonInput"
                        || s.starts_with("/stdJsonInput/")
                        || s == "/stdJsonOutput"
                        || s.starts_with("/stdJsonOutput/")
                };
                ensure!(
                    is_std(path) && !is_std(pointer),
                    "invalid or cyclic local reference"
                );
            }
        }
        for path in &paths {
            (self.check)()?;
            let mut parent = *path;
            while let Some((prefix, _)) = parent.rsplit_once('/') {
                ensure!(!paths.contains(prefix), "overlapping references");
                parent = prefix;
            }
        }
        Ok(Envelope { data, references })
    }
    fn shared(&mut self, kind: &str, hash: &str) -> Result<Shared> {
        (self.check)()?;
        let key = format!("{kind}:{hash}");
        if let Some(v) = self.shared.get(&key) {
            return Ok(v.clone());
        }
        let e = self.resolve(kind, hash, None)?;
        ensure!(
            e.kind == kind && e.attributes["hash"]["value"] == hash,
            "reference kind/hash mismatch"
        );
        let raw = self.payload(&e)?;
        let value = match kind {
            "sourcefile" => {
                let v: Value = serde_json::from_slice(&raw)?;
                ensure!(
                    v["schema"] == "sourcify.sourcefile.v1",
                    "unsupported source schema"
                );
                let s = v["content"].as_str().context("missing source content")?;
                ensure!(hx(sha(s)) == hash, "source hash mismatch");
                Shared::Scalar(json!(s))
            }
            "code" => {
                ensure!(hx(Keccak256::digest(&raw)) == hash, "code hash mismatch");
                Shared::Scalar(json!(hex::encode(raw)))
            }
            "compilation" => {
                let v: Value = serde_json::from_slice(&raw)?;
                ensure!(
                    v["schema"] == "sourcify.compilation.v3",
                    "unsupported compilation schema"
                );
                ensure!(
                    hx(sha(split::json_bytes(&v))) == hash,
                    "compilation hash mismatch"
                );
                Shared::Compilation(std::rc::Rc::new(self.envelope(v)?))
            }
            _ => bail!("unsupported reference kind {kind}"),
        };
        (self.check)()?;
        self.shared.insert(key, value.clone());
        Ok(value)
    }
    /// Hydrate a subtree before following any references outside that subtree.
    /// Local references recursively select their source subtree in the same envelope.
    fn expand_at(
        &mut self,
        envelope: &Envelope,
        pointer: &str,
        depth: usize,
    ) -> Result<Option<Value>> {
        (self.check)()?;
        ensure!(depth < 64, "reference depth exceeded");
        validate_pointer(pointer, true)?;
        for r in &envelope.references {
            (self.check)()?;
            let path = r["path"].as_str().unwrap();
            if within(pointer, path) {
                let value = self.reference(envelope, r, depth)?;
                return Ok(value.pointer(&pointer[path.len()..]).cloned());
            }
        }
        let Some(mut data) = envelope.data.pointer(pointer).cloned() else {
            ensure!(
                !envelope
                    .references
                    .iter()
                    .any(|r| { within(r["path"].as_str().unwrap(), pointer) }),
                "reference parent missing"
            );
            return Ok(None);
        };
        // Preserve full hydration's insertion order for the CLI.
        for r in envelope
            .references
            .iter()
            .filter(|r| r["kind"] != "local")
            .chain(envelope.references.iter().filter(|r| r["kind"] == "local"))
        {
            (self.check)()?;
            let path = r["path"].as_str().unwrap();
            if within(path, pointer) {
                let value = self.reference(envelope, r, depth)?;
                insert(&mut data, &path[pointer.len()..], value)?;
            }
        }
        Ok(Some(data))
    }
    fn reference(&mut self, envelope: &Envelope, r: &Value, depth: usize) -> Result<Value> {
        (self.check)()?;
        validate_destination(&envelope.data, r["path"].as_str().unwrap())?;
        let kind = r["kind"].as_str().context("missing reference kind")?;
        if kind == "local" {
            return self
                .expand_at(envelope, r["pointer"].as_str().unwrap(), depth + 1)?
                .context("missing local field");
        }
        ensure!(depth < 64, "reference depth exceeded");
        let hash = r["hash"].as_str().context("missing reference hash")?;
        let mut value = match self.shared(kind, hash)? {
            Shared::Scalar(value) => value,
            Shared::Compilation(envelope) => {
                let pointer = r["pointer"]
                    .as_str()
                    .context("missing compilation pointer")?;
                let key = format!("{kind}:{hash}");
                let requested = if self.full { "" } else { pointer };
                let selection = (key.clone(), requested.to_owned());
                if let Some(value) = self.expanded.get(&selection) {
                    return if self.full {
                        value
                            .pointer(pointer)
                            .cloned()
                            .context("missing compilation field")
                    } else {
                        Ok(value.clone())
                    };
                }
                ensure!(self.active.insert(key.clone()), "reference cycle");
                let value = self
                    .expand_at(&envelope, requested, depth + 1)?
                    .context("missing compilation field")?;
                self.active.remove(&key);
                self.expanded.insert(selection, value.clone());
                if self.full {
                    value
                        .pointer(pointer)
                        .cloned()
                        .context("missing compilation field")?
                } else {
                    value
                }
            }
        };
        if kind == "code" {
            let prefix = r["encoding"]["prefix"]
                .as_bool()
                .context("missing hex prefix encoding")?;
            let mut hex = value.as_str().unwrap().as_bytes().to_vec();
            let mut used = HashSet::new();
            for n in r["encoding"]["uppercase"]
                .as_array()
                .context("missing hex case encoding")?
            {
                (self.check)()?;
                let i = usize::try_from(n.as_u64().context("invalid hex case offset")?)?;
                ensure!(used.insert(i), "duplicate hex case offset");
                let c = hex.get_mut(i).context("hex case offset out of bounds")?;
                ensure!((*c >= b'a') && (*c <= b'f'), "invalid hex uppercase offset");
                *c = c.to_ascii_uppercase();
            }
            value = json!(format!(
                "{}{}",
                if prefix { "0x" } else { "" },
                String::from_utf8(hex)?
            ));
        }
        Ok(value)
    }
}
pub(crate) fn within(path: &str, parent: &str) -> bool {
    path == parent
        || path
            .strip_prefix(parent)
            .is_some_and(|suffix| suffix.starts_with('/'))
}
pub(crate) fn validate_pointer(pointer: &str, allow_root: bool) -> Result<()> {
    ensure!(
        (allow_root && pointer.is_empty()) || pointer.starts_with('/'),
        "invalid JSON pointer"
    );
    let mut chars = pointer.chars();
    while let Some(c) = chars.next() {
        if c == '~' {
            ensure!(
                matches!(chars.next(), Some('0' | '1')),
                "invalid JSON pointer escape"
            );
        }
    }
    Ok(())
}
pub(crate) fn validate_destination(data: &Value, path: &str) -> Result<()> {
    let (parent, key) = path.rsplit_once('/').context("invalid JSON pointer")?;
    let decoded = key.replace("~1", "/").replace("~0", "~");
    let parent = data.pointer(parent).context("reference parent missing")?;
    match parent {
        Value::Object(m) => ensure!(!m.contains_key(&decoded), "reference overwrites literal"),
        Value::Array(a) => {
            let i: usize = decoded.parse()?;
            ensure!(i.to_string() == decoded, "invalid array index");
            let slot = a.get(i).context("array reference out of bounds")?;
            ensure!(slot.is_null(), "reference overwrites literal");
        }
        _ => bail!("reference parent must be object or array"),
    }
    Ok(())
}
pub(crate) fn insert(data: &mut Value, path: &str, value: Value) -> Result<()> {
    let (parent, key) = path.rsplit_once('/').context("invalid JSON pointer")?;
    let mut chars = key.chars();
    let mut decoded = String::new();
    while let Some(c) = chars.next() {
        if c == '~' {
            decoded.push(match chars.next() {
                Some('0') => '~',
                Some('1') => '/',
                _ => bail!("invalid JSON pointer escape"),
            });
        } else {
            decoded.push(c);
        }
    }
    let parent = data
        .pointer_mut(parent)
        .context("reference parent missing")?;
    match parent {
        Value::Object(m) => {
            ensure!(!m.contains_key(&decoded), "reference overwrites literal");
            m.insert(decoded, value);
        }
        Value::Array(a) => {
            let i: usize = decoded.parse()?;
            ensure!(i.to_string() == decoded, "invalid array index");
            let slot = a.get_mut(i).context("array reference out of bounds")?;
            ensure!(slot.is_null(), "reference overwrites literal");
            *slot = value;
        }
        _ => bail!("reference parent must be object or array"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn fixture() -> Value {
        let abi = json!([{"type":"function","name":"a_function_name_long_enough_to_make_its_abi_a_useful_local_reference","inputs":[],"outputs":[]}]);
        json!({
            "chainId":"1", "address":"0x1111111111111111111111111111111111111111",
            "matchId":"1", "verifiedAt":"2020-01-01T00:00:00Z",
            "abi": abi,
            "compilation":{"language":"Solidity","compiler":"solc","compilerVersion":"1","compilerSettings":{},"fullyQualifiedName":"A.sol:A"},
            "sources":{"A.sol":{"content":"contract A {}"},"~/odd.sol":{"content":"contract A {}"}},
            "creationBytecode":{"onchainBytecode":null,"recompiledBytecode":null},
            "runtimeBytecode":{"onchainBytecode":"0xABcd","recompiledBytecode":"0xaBcD"},
            "stdJsonInput":{"sources":{"~/odd.sol":{"content":"contract A {}"}}},
            "stdJsonOutput":{"contracts":{"~/odd.sol":{"A":{"abi":abi,"evm":{"bytecode":{"object":"aBcD"}}}}}},
            "metadata":[null,"contract A {}","0xABcd",{"~key/":"contract A {}"}],
            "unknown~/field":{"literal":{"$ref":"not a reference"},"nested":[null,"contract A {}"]}
        })
    }
    fn resolve(entities: &[Entity], kind: &str, hash: &str, part: Option<u64>) -> Result<Entity> {
        entities
            .iter()
            .find(|e| {
                e.kind == kind
                    && e.attributes["hash"]["value"] == hash
                    && part.is_none_or(|p| e.attributes["part"]["value"] == p)
            })
            .cloned()
            .context("missing dependency")
    }
    fn project(value: &Value, fields: &[&str]) -> Value {
        Value::Object(
            fields
                .iter()
                .filter_map(|field| value.get(field).cloned().map(|v| ((*field).into(), v)))
                .collect(),
        )
    }

    #[test]
    fn selections_equal_full_projection_for_every_stored_field() {
        let input = fixture();
        let entities = transform(&input).unwrap();
        let contract = entities.last().unwrap();
        let full = hydrate(contract, &entities).unwrap();
        assert_eq!(full, canonical(&input));
        let compilation: Value = serde_json::from_slice(
            &entities
                .iter()
                .find(|e| e.kind == "compilation")
                .unwrap()
                .payload,
        )
        .unwrap();
        assert!(compilation["references"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["kind"] == "local"));
        let mut selections: Vec<Vec<&str>> = full
            .as_object()
            .unwrap()
            .keys()
            .map(|field| vec![field.as_str()])
            .collect();
        selections.extend([
            vec![],
            vec!["missing"],
            vec!["abi", "compilation", "abi", "missing"],
            vec!["stdJsonInput", "stdJsonOutput", "sources", "metadata"],
            full.as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect(),
        ]);
        for fields in selections {
            let selected = hydrate_fields_with(contract, &fields, |kind, hash, part| {
                resolve(&entities, kind, hash, part)
            })
            .unwrap();
            assert_eq!(selected, project(&full, &fields), "fields={fields:?}");
        }
    }

    #[test]
    fn abi_and_compilation_skip_unrelated_source_and_code() {
        let entities = transform(&fixture()).unwrap();
        let mut reads = Vec::new();
        let selected = hydrate_fields_with(
            entities.last().unwrap(),
            &["abi", "compilation"],
            |kind, hash, part| {
                reads.push(kind.to_owned());
                ensure!(kind == "compilation", "unexpected dependency {kind}");
                resolve(&entities, kind, hash, part)
            },
        )
        .unwrap();
        assert_eq!(selected, project(&fixture(), &["abi", "compilation"]));
        assert_eq!(reads, ["compilation"]);
        let literal = hydrate_fields_with(entities.last().unwrap(), &["address"], |_, _, _| {
            bail!("literal field must not resolve dependencies")
        })
        .unwrap();
        assert_eq!(literal, project(&fixture(), &["address"]));
    }

    #[test]
    fn local_reference_only_reads_its_selected_dependency() {
        let mut input = fixture();
        input["stdJsonOutput"] = json!({"abi":input["abi"]});
        let entities = transform(&input).unwrap();
        let selected = hydrate_fields_with(
            entities.last().unwrap(),
            &["stdJsonOutput"],
            |kind, hash, part| {
                ensure!(kind == "compilation", "unrelated dependency {kind}");
                resolve(&entities, kind, hash, part)
            },
        )
        .unwrap();
        assert_eq!(selected, project(&input, &["stdJsonOutput"]));
    }

    #[test]
    fn local_array_aliases_expand_nested_and_escaped_source_pointers() {
        let mut input = fixture();
        input["metadata"] = json!({"~/artifact": {
            "long_name": "an artifact large enough to use a local reference instead of an inline copy",
            "values": ["contract A {}", "0xaBcD", null]
        }});
        input["stdJsonOutput"] = json!([
            null,
            input["abi"][0],
            input["metadata"]["~/artifact"],
            input["metadata"]["~/artifact"]
        ]);
        let entities = transform(&input).unwrap();
        let compilation: Value = serde_json::from_slice(
            &entities
                .iter()
                .find(|e| e.kind == "compilation")
                .unwrap()
                .payload,
        )
        .unwrap();
        assert!(compilation["references"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| { r["kind"] == "local" && r["pointer"] == "/metadata/~0~1artifact" }));
        let mut reads = HashMap::new();
        let selected = hydrate_fields_with(
            entities.last().unwrap(),
            &["stdJsonOutput"],
            |kind, hash, part| {
                *reads
                    .entry((kind.to_owned(), hash.to_owned(), part))
                    .or_insert(0) += 1;
                resolve(&entities, kind, hash, part)
            },
        )
        .unwrap();
        assert_eq!(
            selected,
            project(
                &hydrate(entities.last().unwrap(), &entities).unwrap(),
                &["stdJsonOutput"]
            )
        );
        assert!(reads.values().all(|count| *count == 1));
    }

    #[test]
    fn selected_spilled_compilation_checks_all_blobs_and_skips_source_and_code() {
        let mut input = fixture();
        input["metadata"] = json!("large metadata ".repeat(BUDGET));
        let entities = transform(&input).unwrap();
        let mut reads = HashMap::new();
        let selected = hydrate_fields_with(
            entities.last().unwrap(),
            &["abi", "compilation"],
            |kind, hash, part| {
                ensure!(kind == "compilation" || kind == "blob", "unrelated read");
                *reads
                    .entry((kind.to_owned(), hash.to_owned(), part))
                    .or_insert(0) += 1;
                resolve(&entities, kind, hash, part)
            },
        )
        .unwrap();
        assert_eq!(selected, project(&input, &["abi", "compilation"]));
        assert!(reads.keys().any(|(kind, _, _)| kind == "blob"));
        assert!(reads.values().all(|count| *count == 1));
        let corrupt =
            hydrate_fields_with(entities.last().unwrap(), &["abi"], |kind, hash, part| {
                let mut entity = resolve(&entities, kind, hash, part)?;
                if kind == "blob" {
                    entity.payload[0] ^= 1;
                    entity.identify();
                }
                Ok(entity)
            });
        assert!(corrupt.unwrap_err().to_string().contains("spill integrity"));
        let missing =
            hydrate_fields_with(entities.last().unwrap(), &["abi"], |kind, hash, part| {
                ensure!(kind != "blob" || part != Some(1), "missing spill blob");
                resolve(&entities, kind, hash, part)
            });
        assert!(missing.is_err());
    }

    #[test]
    fn source_and_code_sharing_a_spill_read_each_blob_once() {
        let mut input = fixture();
        let content = "x".repeat(BUDGET + 1);
        let bytes =
            split::json_bytes(&json!({"schema":"sourcify.sourcefile.v1","content":content}));
        input["sources"] =
            json!({"A.sol":{"content":content.clone()},"~/odd.sol":{"content":content}});
        input["runtimeBytecode"] = json!({
            "onchainBytecode":format!("0x{}",hex::encode(&bytes)),
            "recompiledBytecode":format!("0x{}",hex::encode(&bytes))
        });
        let entities = transform(&input).unwrap();
        for full in [false, true] {
            let mut reads = HashMap::new();
            let resolver = |kind: &str, hash: &str, part| {
                *reads
                    .entry((kind.to_owned(), hash.to_owned(), part))
                    .or_insert(0) += 1;
                resolve(&entities, kind, hash, part)
            };
            let selected = if full {
                hydrate_with(entities.last().unwrap(), resolver)
            } else {
                hydrate_fields_with(
                    entities.last().unwrap(),
                    &["sources", "runtimeBytecode"],
                    resolver,
                )
            }
            .unwrap();
            assert_eq!(
                selected,
                if full {
                    canonical(&input)
                } else {
                    project(&input, &["sources", "runtimeBytecode"])
                }
            );
            assert!(reads
                .keys()
                .any(|(kind, hash, _)| kind == "blob" && hash == &hx(sha(&bytes))));
            assert!(reads.values().all(|count| *count == 1), "reads={reads:?}");
        }
    }

    #[test]
    fn selection_rejects_references_overwriting_literals() {
        let entities = transform(&fixture()).unwrap();
        let mut contract = entities.last().unwrap().clone();
        let mut envelope: Value = serde_json::from_slice(&contract.payload).unwrap();
        let reference = envelope["references"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["path"] == "/abi")
            .unwrap()
            .clone();
        envelope["data"]["abi"] = json!([]);
        contract.payload = serde_json::to_vec(&envelope).unwrap();
        contract.identify();
        let error = hydrate_fields_with(&contract, &["abi"], |kind, hash, part| {
            assert_eq!(reference["kind"], kind);
            resolve(&entities, kind, hash, part)
        })
        .unwrap_err();
        assert!(error.to_string().contains("overwrites literal"));
    }

    #[test]
    fn cancellation_interrupts_traversal_without_dependency_reads() {
        let entities = transform(&fixture()).unwrap();
        let checks = Cell::new(0);
        let fields = vec!["absent"; 1_000];
        let error = hydrate_fields_with_check(
            entities.last().unwrap(),
            &fields,
            |_, _, _| bail!("unexpected dependency read"),
            || {
                checks.set(checks.get() + 1);
                ensure!(checks.get() < 100, "request cancelled");
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "request cancelled");
        assert_eq!(checks.get(), 100);
    }

    #[test]
    fn reference_depth_is_bounded() {
        let mut entities = Vec::new();
        let mut hash = String::new();
        for level in 0..66 {
            let envelope = if level == 0 {
                json!({"schema":"sourcify.compilation.v3","data":{"abi":[]},"references":[]})
            } else {
                json!({"schema":"sourcify.compilation.v3","data":{},"references":[{"path":"/abi","kind":"compilation","hash":hash,"pointer":"/abi"}]})
            };
            let payload = split::json_bytes(&envelope);
            hash = hx(sha(&payload));
            let entity = fit(
                "compilation",
                payload,
                "application/json",
                attrs(json!({"hash":attr("bytes32", hash.as_str())})),
                &mut entities,
            );
            entities.push(entity);
        }
        let envelope = json!({"schema":"sourcify.verified-contract.v3","data":{},"references":[{"path":"/abi","kind":"compilation","hash":hash,"pointer":"/abi"}]});
        let contract = fit(
            "verified_contract",
            split::json_bytes(&envelope),
            "application/json",
            Map::new(),
            &mut entities,
        );
        let error = hydrate_fields_with(&contract, &["abi"], |kind, hash, part| {
            resolve(&entities, kind, hash, part)
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "reference depth exceeded");
    }
}
