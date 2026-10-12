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
pub fn hydrate_with<F>(contract: &Entity, mut resolve: F) -> Result<Value>
where
    F: FnMut(&str, &str, Option<u64>) -> Result<Entity>,
{
    ensure!(
        contract.kind == "verified_contract",
        "expected verified_contract"
    );
    let payload = payload_with(contract, &mut resolve)?;
    let envelope: Value = serde_json::from_slice(&payload)?;
    ensure!(
        envelope["schema"] == "sourcify.verified-contract.v3",
        "unsupported contract schema"
    );
    let mut state = State {
        resolve: &mut resolve,
        cache: HashMap::new(),
        active: HashSet::new(),
    };
    state.expand(&envelope, 0)
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
struct State<'a, F> {
    resolve: &'a mut F,
    cache: HashMap<String, Value>,
    active: HashSet<String>,
}
impl<F> State<'_, F>
where
    F: FnMut(&str, &str, Option<u64>) -> Result<Entity>,
{
    fn shared(&mut self, kind: &str, hash: &str, depth: usize) -> Result<Value> {
        ensure!(depth < 64, "reference depth exceeded");
        let key = format!("{kind}:{hash}");
        if let Some(v) = self.cache.get(&key) {
            return Ok(v.clone());
        }
        ensure!(self.active.insert(key.clone()), "reference cycle");
        let e = (self.resolve)(kind, hash, None)?;
        ensure!(
            e.kind == kind && e.attributes["hash"]["value"] == hash,
            "reference kind/hash mismatch"
        );
        let raw = payload_with(&e, self.resolve)?;
        let value = match kind {
            "sourcefile" => {
                let v: Value = serde_json::from_slice(&raw)?;
                ensure!(
                    v["schema"] == "sourcify.sourcefile.v1",
                    "unsupported source schema"
                );
                let s = v["content"].as_str().context("missing source content")?;
                ensure!(hx(sha(s)) == hash, "source hash mismatch");
                json!(s)
            }
            "code" => {
                ensure!(hx(Keccak256::digest(&raw)) == hash, "code hash mismatch");
                json!(hex::encode(raw))
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
                self.expand(&v, depth + 1)?
            }
            _ => bail!("unsupported reference kind {kind}"),
        };
        self.active.remove(&key);
        self.cache.insert(key, value.clone());
        Ok(value)
    }
    fn expand(&mut self, envelope: &Value, depth: usize) -> Result<Value> {
        let mut data = envelope
            .get("data")
            .context("missing envelope data")?
            .clone();
        ensure!(data.is_object(), "envelope data must be object");
        let refs = envelope["references"]
            .as_array()
            .context("missing references")?;
        let mut paths = HashSet::new();
        for r in refs {
            let path = r["path"].as_str().context("missing reference path")?;
            ensure!(
                path.starts_with('/') && paths.insert(path),
                "invalid or duplicate reference path"
            );
        }
        for path in &paths {
            let mut parent = *path;
            while let Some((prefix, _)) = parent.rsplit_once('/') {
                ensure!(!paths.contains(prefix), "overlapping references");
                parent = prefix;
            }
        }
        for r in refs.iter().filter(|r| r["kind"] != "local") {
            let kind = r["kind"].as_str().context("missing reference kind")?;
            let hash = r["hash"].as_str().context("missing reference hash")?;
            let mut value = self.shared(kind, hash, depth)?;
            if kind == "compilation" {
                value = value
                    .pointer(
                        r["pointer"]
                            .as_str()
                            .context("missing compilation pointer")?,
                    )
                    .context("missing compilation field")?
                    .clone();
            }
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
            insert(&mut data, r["path"].as_str().unwrap(), value)?;
        }
        for r in refs.iter().filter(|r| r["kind"] == "local") {
            ensure!(
                envelope["schema"] == "sourcify.compilation.v3",
                "local references require compilation"
            );
            let path = r["path"].as_str().unwrap();
            let pointer = r["pointer"].as_str().context("missing local pointer")?;
            let is_std = |s: &str| {
                s == "/stdJsonInput"
                    || s.starts_with("/stdJsonInput/")
                    || s == "/stdJsonOutput"
                    || s.starts_with("/stdJsonOutput/")
            };
            ensure!(
                is_std(path) && !is_std(pointer) && pointer.starts_with('/'),
                "invalid or cyclic local reference"
            );
            let value = data
                .pointer(pointer)
                .context("missing local field")?
                .clone();
            insert(&mut data, path, value)?;
        }
        Ok(data)
    }
}
fn insert(data: &mut Value, path: &str, value: Value) -> Result<()> {
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
