//! 按层改设置文件的纯逻辑：一层是 `settings.json` 加旁边的 `settings.local.json`，
//! 对它们按键路径套用改动（凭据字段只落 local）、算这一层内容的 revision。值放不放得
//! 进、加锁、写盘、重载与信任判定由调用方做——`config.set`、`config.setProvider`、
//! `mcp.*` 都经这里改文件。

use serde_json::{Map, Value};

use crate::merge::merge_json_values;

/// The settings that hold credentials, with `*` for one map key: kept out of
/// the committed `settings.json` by every writer.
pub const SECRET_PATHS: &[&str] = &["mcp.servers.*.headers", "mcp.servers.*.env"];

/// The lists each file adds to the other files' rather than replacing them,
/// so a tier's two files are edited apart: a change reaches `settings.json`
/// only, and a change of the object above one leaves what
/// `settings.local.json` holds there.
pub const CONCATENATED_KEYS: &[&str] = &[
    "permissions.allow",
    "permissions.allow_exact",
    "permissions.ask",
    "permissions.deny",
    "app.trusted_projects",
    "providers",
];

/// A dotted settings key, one segment per object level.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct KeyPath(Vec<String>);

impl KeyPath {
    /// Segments are not empty; `*` and array indexes are not keys.
    pub fn parse(dotted: &str) -> Result<Self, String> {
        let segments: Vec<String> = dotted.split('.').map(String::from).collect();
        if segments.iter().any(String::is_empty) {
            return Err(format!("`{dotted}` is not a dotted settings key"));
        }
        if segments.iter().any(|s| s == "*" || s.contains('[')) {
            return Err(format!(
                "`{dotted}`: wildcards and list indexes are not keys; write the whole list or map"
            ));
        }
        Ok(Self(segments))
    }

    /// A key whose segments are given, such as a map entry with a `.` in its
    /// name.
    pub fn new<S: Into<String>>(segments: impl IntoIterator<Item = S>) -> Self {
        Self(segments.into_iter().map(Into::into).collect())
    }

    pub fn segments(&self) -> Vec<&str> {
        self.0.iter().map(String::as_str).collect()
    }

    pub fn dotted(&self) -> String {
        self.0.join(".")
    }

    /// Whether `other` is this key or beneath it.
    pub fn contains(&self, other: &KeyPath) -> bool {
        other.0.starts_with(&self.0)
    }

    pub fn overlaps(&self, other: &KeyPath) -> bool {
        self.contains(other) || other.contains(self)
    }

    /// Whether this is one of [`CONCATENATED_KEYS`].
    pub fn is_concatenated(&self) -> bool {
        CONCATENATED_KEYS.contains(&self.dotted().as_str())
    }

    /// The parts of a value at this key that are credentials, relative to
    /// it: `[]` when the key itself is one.
    fn secrets_beneath(&self) -> Vec<Vec<String>> {
        SECRET_PATHS
            .iter()
            .filter_map(|pattern| {
                let pattern: Vec<&str> = pattern.split('.').collect();
                let shared = pattern.len().min(self.0.len());
                let agrees = pattern[..shared]
                    .iter()
                    .zip(&self.0)
                    .all(|(p, k)| *p == "*" || p == k);
                agrees.then(|| pattern[shared..].iter().map(|s| s.to_string()).collect())
            })
            .collect()
    }
}

/// One change to a tier.
#[derive(Debug, Clone, PartialEq)]
pub enum EditOp {
    /// The tier holds `value` at the key, replacing what either file held
    /// there.
    Set(KeyPath, Value),
    /// [`Set`](Self::Set), except that the named keys directly beneath the
    /// key keep what each file held there: a server's configuration replaced
    /// whole, its `enabled` kept.
    SetKeeping(KeyPath, Value, Vec<String>),
    /// Neither file holds the key.
    Remove(KeyPath),
    /// `value` merged key by key into what `settings.json` holds at the key;
    /// what `settings.local.json` holds beside it stays unless `value` sets
    /// a credential.
    Merge(KeyPath, Value),
    /// `value` merged into the entry whose `id` is the given one of the list
    /// at the key, in the file that holds it — `settings.json` when both do —
    /// or added to `settings.json`'s list when neither does.
    MergeEntry(KeyPath, String, Value),
    /// Neither file's list at the key holds an entry with that `id`.
    RemoveEntry(KeyPath, String),
}

impl EditOp {
    pub fn path(&self) -> &KeyPath {
        match self {
            Self::Set(path, _)
            | Self::SetKeeping(path, _, _)
            | Self::Remove(path)
            | Self::Merge(path, _)
            | Self::MergeEntry(path, _, _)
            | Self::RemoveEntry(path, _) => path,
        }
    }
}

/// A tier's two files as objects.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TierFiles {
    pub shared: Map<String, Value>,
    pub local: Map<String, Value>,
}

impl TierFiles {
    /// Whether either file holds `path`.
    pub fn declares(&self, path: &KeyPath) -> bool {
        let segments = path.segments();
        [&self.shared, &self.local]
            .iter()
            .any(|file| get(file, &segments).is_some())
    }

    /// Whether either file holds at `path` anything but `besides`: a value
    /// that is not an object, or an object with some other key.
    pub fn declares_besides(&self, path: &KeyPath, besides: &str) -> bool {
        let segments = path.segments();
        [&self.shared, &self.local]
            .iter()
            .filter_map(|file| get(file, &segments))
            .any(|value| {
                value
                    .as_object()
                    .is_none_or(|fields| fields.keys().any(|key| key != besides))
            })
    }
}

/// Apply `ops` in order, credentials into `local`; credentials `shared`
/// already held beneath a key an op names move to `local` too, unless
/// `local` has its own. Whether `local` changed.
pub fn apply(files: &mut TierFiles, ops: &[EditOp]) -> bool {
    let local_before = files.local.clone();
    let ops: Vec<&EditOp> = ops.iter().filter(|op| !op.path().0.is_empty()).collect();
    for op in &ops {
        match op {
            EditOp::Set(path, value) => set(files, path, value.clone()),
            EditOp::SetKeeping(path, value, keep) => {
                let held: Vec<(Vec<&str>, Option<Value>, Option<Value>)> = keep
                    .iter()
                    .map(|key| {
                        let mut at = path.segments();
                        at.push(key);
                        let shared = get(&files.shared, &at).cloned();
                        let local = get(&files.local, &at).cloned();
                        (at, shared, local)
                    })
                    .collect();
                set(files, path, value.clone());
                for (at, shared, local) in held {
                    if let Some(value) = shared {
                        put(&mut files.shared, &at, value);
                    }
                    if let Some(value) = local {
                        put(&mut files.local, &at, value);
                    }
                }
            }
            EditOp::Remove(path) => {
                take(&mut files.shared, &path.segments());
                take_local(files, path);
            }
            EditOp::Merge(path, value) => merge(files, path, value.clone()),
            EditOp::MergeEntry(path, id, value) => merge_entry(files, path, id, value.clone()),
            EditOp::RemoveEntry(path, id) => {
                remove_entry(&mut files.shared, path, id);
                remove_entry(&mut files.local, path, id);
            }
        }
    }
    for op in &ops {
        move_secrets_out(files, op.path());
    }
    files.local != local_before
}

fn set(files: &mut TierFiles, path: &KeyPath, mut value: Value) {
    let segments = path.segments();
    if path.is_concatenated() {
        put(&mut files.shared, &segments, value);
        return;
    }
    take_local(files, path);
    let secrets = path.secrets_beneath();
    if secrets.iter().any(Vec::is_empty) {
        take(&mut files.shared, &segments);
        put(&mut files.local, &segments, value);
        return;
    }
    let found = extract_all(&mut value, &secrets);
    put(&mut files.shared, &segments, value);
    if let Some(found) = found {
        put(&mut files.local, &segments, found);
    }
}

/// Remove what `settings.local.json` holds at `path`, except the concatenated
/// lists at or beneath it.
fn take_local(files: &mut TierFiles, path: &KeyPath) {
    if path.is_concatenated() {
        return;
    }
    let kept: Vec<(Vec<&str>, Value)> = CONCATENATED_KEYS
        .iter()
        .map(|key| key.split('.').collect::<Vec<_>>())
        .filter(|key| path.contains(&KeyPath::new(key.iter().copied())))
        .filter_map(|key| get(&files.local, &key).cloned().map(|value| (key, value)))
        .collect();
    take(&mut files.local, &path.segments());
    for (key, value) in kept {
        put(&mut files.local, &key, value);
    }
}

fn merge(files: &mut TierFiles, path: &KeyPath, mut value: Value) {
    let segments = path.segments();
    let secrets = path.secrets_beneath();
    let found = if secrets.iter().any(Vec::is_empty) {
        Some(std::mem::take(&mut value))
    } else {
        let found = extract_all(&mut value, &secrets);
        merge_json_values(slot(&mut files.shared, &segments), value);
        found
    };
    if let Some(found) = found {
        merge_json_values(slot(&mut files.local, &segments), found);
    }
}

fn has_id(entry: &Value, id: &str) -> bool {
    entry.get("id").and_then(Value::as_str) == Some(id)
}

fn merge_entry(files: &mut TierFiles, path: &KeyPath, id: &str, value: Value) {
    let segments = path.segments();
    for file in [&mut files.shared, &mut files.local] {
        let entry = get_mut(file, &segments)
            .and_then(Value::as_array_mut)
            .and_then(|list| list.iter_mut().find(|entry| has_id(entry, id)));
        if let Some(entry) = entry {
            merge_json_values(entry, value);
            return;
        }
    }
    let mut entry = serde_json::json!({ "id": id });
    merge_json_values(&mut entry, value);
    match get_mut(&mut files.shared, &segments).and_then(Value::as_array_mut) {
        Some(list) => list.push(entry),
        None => put(&mut files.shared, &segments, Value::Array(vec![entry])),
    }
}

/// The list at `path` without its entry `id`; gone with it when emptied.
fn remove_entry(file: &mut Map<String, Value>, path: &KeyPath, id: &str) {
    let segments = path.segments();
    let Some(list) = get_mut(file, &segments).and_then(Value::as_array_mut) else {
        return;
    };
    list.retain(|entry| !has_id(entry, id));
    if list.is_empty() {
        take(file, &segments);
    }
}

fn move_secrets_out(files: &mut TierFiles, path: &KeyPath) {
    let segments = path.segments();
    let secrets = path.secrets_beneath();
    let found = if secrets.iter().any(Vec::is_empty) {
        take(&mut files.shared, &segments)
    } else {
        let Some(held) = get_mut(&mut files.shared, &segments) else {
            return;
        };
        extract_all(held, &secrets)
    };
    if let Some(found) = found {
        merge_absent(slot(&mut files.local, &segments), found);
    }
}

/// Take every part of `value` a relative credential pattern names out of
/// it, as one tree of the same shape.
fn extract_all(value: &mut Value, patterns: &[Vec<String>]) -> Option<Value> {
    let mut found: Option<Value> = None;
    for pattern in patterns {
        if let Some(part) = extract(value, pattern) {
            match &mut found {
                Some(tree) => merge_json_values(tree, part),
                None => found = Some(part),
            }
        }
    }
    found
}

fn extract(value: &mut Value, pattern: &[String]) -> Option<Value> {
    let (first, rest) = pattern.split_first()?;
    let fields = value.as_object_mut()?;
    let keys: Vec<String> = if first == "*" {
        fields.keys().cloned().collect()
    } else {
        fields
            .contains_key(first)
            .then(|| first.clone())
            .into_iter()
            .collect()
    };
    let mut out = Map::new();
    for key in keys {
        let part = if rest.is_empty() {
            fields.remove(&key)
        } else {
            fields.get_mut(&key).and_then(|child| extract(child, rest))
        };
        if let Some(part) = part {
            out.insert(key, part);
        }
    }
    (!out.is_empty()).then_some(Value::Object(out))
}

/// `over` merged into `base`, keeping every value `base` already has.
fn merge_absent(base: &mut Value, over: Value) {
    match (base, over) {
        (Value::Object(base), Value::Object(over)) => {
            for (key, value) in over {
                match base.get_mut(&key) {
                    Some(existing) => merge_absent(existing, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base @ Value::Null, over) => *base = over,
        _ => {}
    }
}

fn get<'a>(file: &'a Map<String, Value>, segments: &[&str]) -> Option<&'a Value> {
    let (first, rest) = segments.split_first()?;
    let value = file.get(*first)?;
    if rest.is_empty() {
        return Some(value);
    }
    get(value.as_object()?, rest)
}

fn get_mut<'a>(file: &'a mut Map<String, Value>, segments: &[&str]) -> Option<&'a mut Value> {
    let (first, rest) = segments.split_first()?;
    let value = file.get_mut(*first)?;
    if rest.is_empty() {
        return Some(value);
    }
    get_mut(value.as_object_mut()?, rest)
}

/// The value at `segments`, made: each level above it an object, replacing
/// what was not one; `null` where there was nothing.
fn slot<'a>(file: &'a mut Map<String, Value>, segments: &[&str]) -> &'a mut Value {
    let Some((first, rest)) = segments.split_first() else {
        unreachable!("`apply` passes no op with an empty key")
    };
    let value = file.entry(first.to_string()).or_insert(Value::Null);
    if rest.is_empty() {
        return value;
    }
    if !value.is_object() {
        *value = Value::Object(Map::new());
    }
    match value {
        Value::Object(inner) => slot(inner, rest),
        _ => unreachable!("made an object above"),
    }
}

fn put(file: &mut Map<String, Value>, segments: &[&str], value: Value) {
    *slot(file, segments) = value;
}

/// Remove the value at `segments`, and each object above it left empty.
fn take(file: &mut Map<String, Value>, segments: &[&str]) -> Option<Value> {
    let (first, rest) = segments.split_first()?;
    if rest.is_empty() {
        return file.remove(*first);
    }
    let inner = file.get_mut(*first)?.as_object_mut()?;
    let taken = take(inner, rest)?;
    if inner.is_empty() {
        file.remove(*first);
    }
    Some(taken)
}

/// A tier's content as of its two files' bytes (a missing file is empty):
/// what `if_revision` is compared with.
pub fn revision(shared: &[u8], local: &[u8]) -> String {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for (tag, bytes) in [(b's', shared), (b'l', local)] {
        let length = (bytes.len() as u64).to_le_bytes();
        for byte in std::iter::once(&tag).chain(&length).chain(bytes) {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(PRIME);
        }
    }
    format!("r1:{hash:016x}")
}

#[cfg(test)]
mod tests;
