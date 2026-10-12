use super::*;
use std::collections::HashMap;

pub(crate) fn json_bytes(value: &Value) -> Vec<u8> {
    serde_json::to_vec(&canonical(value)).expect("JSON serialization")
}
fn escape(s: &str) -> String {
    s.replace('~', "~0").replace('/', "~1")
}
fn envelope(schema: &str, data: Value, references: Vec<Value>) -> Value {
    json!({"schema":schema,"data":data,"references":references})
}
struct Pool {
    entities: Vec<Entity>,
    sources: HashMap<String, String>,
    codes: HashMap<Vec<u8>, String>,
}
impl Pool {
    fn source(&mut self, content: &str) -> String {
        if let Some(hash) = self.sources.get(content) {
            return hash.clone();
        }
        let hash = hx(sha(content));
        let e = fit(
            "sourcefile",
            json_bytes(&json!({"schema":"sourcify.sourcefile.v1","content":content})),
            "application/json",
            attrs(json!({"hash":attr("bytes32",hash.as_str()),"size":attr("i32",content.len())})),
            &mut self.entities,
        );
        self.entities.push(e);
        self.sources.insert(content.into(), hash.clone());
        hash
    }
    fn code(&mut self, data: Vec<u8>) -> String {
        if let Some(hash) = self.codes.get(&data) {
            return hash.clone();
        }
        let hash = hx(Keccak256::digest(&data));
        let e = fit(
            "code",
            data.clone(),
            "application/octet-stream",
            attrs(json!({"hash":attr("bytes32",hash.as_str()),"size":attr("i32",data.len())})),
            &mut self.entities,
        );
        self.entities.push(e);
        self.codes.insert(data, hash.clone());
        hash
    }
    fn strip(&self, value: &mut Value, path: &str, refs: &mut Vec<Value>) {
        if let Some(s) = value.as_str() {
            if let Some(hash) = self
                .sources
                .get(s)
                .filter(|_| !s.is_empty() || path.ends_with("/content"))
            {
                refs.push(json!({"path":path,"kind":"sourcefile","hash":hash}));
                return;
            }
            let (prefix, hexstr) = s.strip_prefix("0x").map_or((false, s), |s| (true, s));
            if !hexstr.is_empty() {
                if let Ok(raw) = hex::decode(hexstr) {
                    if let Some(hash) = self.codes.get(&raw) {
                        let uppercase: Vec<usize> = hexstr
                            .bytes()
                            .enumerate()
                            .filter_map(|(i, c)| c.is_ascii_uppercase().then_some(i))
                            .collect();
                        refs.push(json!({"path":path,"kind":"code","hash":hash,"encoding":{"prefix":prefix,"uppercase":uppercase}}));
                        return;
                    }
                }
            }
        }
        match value {
            Value::Object(m) => {
                let keys: Vec<_> = m.keys().cloned().collect();
                for k in keys {
                    let before = refs.len();
                    let child = format!("{path}/{}", escape(&k));
                    self.strip(m.get_mut(&k).unwrap(), &child, refs);
                    if refs.get(before).is_some_and(|r| r["path"] == child) {
                        m.remove(&k);
                    }
                }
            }
            Value::Array(a) => {
                for (i, v) in a.iter_mut().enumerate() {
                    let child = format!("{path}/{i}");
                    let before = refs.len();
                    self.strip(v, &child, refs);
                    if refs.get(before).is_some_and(|r| r["path"] == child) {
                        *v = Value::Null;
                    }
                }
            }
            _ => {}
        }
    }
}
// Standard JSON repeats selected-contract artifacts. Reuse exact matching subtrees
// from the hydrated compilation, without interpreting source-owned object keys.
fn strip_artifacts(shared: &mut Value, refs: &mut Vec<Value>) {
    fn collect(v: &Value, path: &str, index: &mut HashMap<Vec<u8>, String>) {
        if json_bytes(v).len() >= 32 {
            index.entry(json_bytes(v)).or_insert_with(|| path.into());
        }
        match v {
            Value::Object(m) => {
                for (k, v) in m {
                    collect(v, &format!("{path}/{}", escape(k)), index)
                }
            }
            Value::Array(a) => {
                for (i, v) in a.iter().enumerate() {
                    collect(v, &format!("{path}/{i}"), index)
                }
            }
            _ => {}
        }
    }
    fn strip(
        v: &mut Value,
        path: &str,
        index: &HashMap<Vec<u8>, String>,
        refs: &mut Vec<Value>,
    ) -> bool {
        if let Some(pointer) = index.get(&json_bytes(v)) {
            let reference = json!({"path":path,"kind":"local","pointer":pointer});
            if json_bytes(v).len() > json_bytes(&reference).len() {
                refs.push(reference);
                return true;
            }
        }
        match v {
            Value::Object(m) => {
                let keys: Vec<_> = m.keys().cloned().collect();
                for k in keys {
                    if strip(
                        m.get_mut(&k).unwrap(),
                        &format!("{path}/{}", escape(&k)),
                        index,
                        refs,
                    ) {
                        m.remove(&k);
                    }
                }
            }
            Value::Array(a) => {
                for (i, v) in a.iter_mut().enumerate() {
                    if strip(v, &format!("{path}/{i}"), index, refs) {
                        *v = Value::Null;
                    }
                }
            }
            _ => {}
        }
        false
    }
    let mut index = HashMap::new();
    for (k, v) in shared.as_object().unwrap() {
        if k != "stdJsonInput" && k != "stdJsonOutput" {
            collect(v, &format!("/{}", escape(k)), &mut index);
        }
    }
    for name in ["stdJsonInput", "stdJsonOutput"] {
        if let Some(v) = shared.get_mut(name) {
            if strip(v, &format!("/{name}"), &index, refs) {
                shared.as_object_mut().unwrap().remove(name);
            }
        }
    }
}
type SplitOutput = (Vec<Entity>, Vec<u8>, String, Map<String, Value>);

pub(crate) fn split(r: &Value) -> Result<SplitOutput> {
    let mut pool = Pool {
        entities: vec![],
        sources: HashMap::new(),
        codes: HashMap::new(),
    };
    for sources in [&r["sources"], &r["stdJsonInput"]["sources"]] {
        for (_, source) in sources.as_object().into_iter().flatten() {
            if let Some(content) = source["content"].as_str() {
                pool.source(content);
            }
        }
    }
    for (_, source) in r["sources"].as_object().into_iter().flatten() {
        ensure!(source["content"].is_string(), "missing source body");
    }
    let mut code_hashes = Map::new();
    for (kind, role) in [
        ("creationBytecode", "creation"),
        ("runtimeBytecode", "runtime"),
    ] {
        for (field, suffix) in [
            ("onchainBytecode", "Onchain"),
            ("recompiledBytecode", "Recompiled"),
        ] {
            let raw = r[kind].get(field).context("missing bytecode field")?;
            let hash = if raw.is_null() {
                Value::Null
            } else {
                let s = raw.as_str().context("bytecode must be hex or null")?;
                ensure!(s.starts_with("0x"), "bytecode needs 0x");
                let data = hex::decode(&s[2..])?;
                if data.is_empty() {
                    Value::Null
                } else if field == "recompiledBytecode" {
                    json!(pool.code(data))
                } else {
                    json!(hx(Keccak256::digest(data)))
                }
            };
            code_hashes.insert(format!("{role}{suffix}"), hash);
        }
    }
    let mut data = r.clone();
    let mut shared = Map::new();
    let mut paths = Vec::new();
    for name in [
        "compilation",
        "additionalInput",
        "sources",
        "abi",
        "signatures",
        "metadata",
        "storageLayout",
        "transientStorageLayout",
        "userdoc",
        "devdoc",
        "sourceIds",
        "stdJsonInput",
        "stdJsonOutput",
    ] {
        if let Some(v) = data.as_object_mut().unwrap().remove(name) {
            shared.insert(name.into(), v);
            paths.push(format!("/{name}"));
        }
    }
    for kind in ["creationBytecode", "runtimeBytecode"] {
        let fields = data[kind].as_object_mut().unwrap();
        let mut artifacts = Map::new();
        for name in [
            "recompiledBytecode",
            "sourceMap",
            "linkReferences",
            "immutableReferences",
            "cborAuxdata",
        ] {
            if let Some(v) = fields.remove(name) {
                artifacts.insert(name.into(), v);
                paths.push(format!("/{kind}/{name}"));
            }
        }
        shared.insert(kind.into(), Value::Object(artifacts));
    }
    let mut shared = Value::Object(shared);
    let mut shared_refs = vec![];
    strip_artifacts(&mut shared, &mut shared_refs);
    pool.strip(&mut shared, "", &mut shared_refs);
    let cp = envelope("sourcify.compilation.v3", shared, shared_refs);
    let cp_bytes = json_bytes(&cp);
    let fingerprint = hx(sha(&cp_bytes));
    let mut ca = compiler_attrs(r, true);
    ca.insert("hash".into(), attr("bytes32", fingerprint.as_str()));
    let e = fit(
        "compilation",
        cp_bytes,
        "application/json",
        ca,
        &mut pool.entities,
    );
    let compilation_id = hex::encode(e.id);
    pool.entities.push(e);
    for kind in ["creationBytecode", "runtimeBytecode"] {
        if let Some(s) = r[kind]["onchainBytecode"].as_str() {
            let data = hex::decode(&s[2..])?;
            if !data.is_empty() {
                pool.code(data);
            }
        }
    }
    let mut references = vec![];
    pool.strip(&mut data, "", &mut references);
    for path in paths {
        references
            .push(json!({"path":path,"kind":"compilation","hash":fingerprint,"pointer":path}));
    }
    Ok((
        pool.entities,
        json_bytes(&envelope("sourcify.verified-contract.v3", data, references)),
        compilation_id,
        code_hashes,
    ))
}
