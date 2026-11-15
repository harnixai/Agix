//! The files an archive carries besides transcripts and images: the root's
//! checkpoint objects and the saved tool results its markers point at.

use super::{SessionPart, Sink};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

/// The directory the root session's lineage saves long tool results in.
pub struct SavedResultsPart {
    pub dir: PathBuf,
}

/// Every kept earlier version of a file the root's log names, under
/// `checkpoints/<sha256>`; one that is no longer kept is left out.
pub(super) fn pack_checkpoints(
    objects: &Option<std::sync::Arc<dyn history::blob::BlobStore>>,
    sessions: &[SessionPart],
    sink: &mut dyn Sink,
) -> std::io::Result<Value> {
    let (Some(objects), Some(root)) = (objects, sessions.first()) else {
        return Ok(Value::Null);
    };
    let entries = &root.entries;
    let ids: std::collections::BTreeSet<String> = entries
        .iter()
        .filter_map(history::checkpoint::plan::pre_of)
        .flat_map(|pre| pre.files)
        .filter_map(|file| match file.before {
            history::checkpoint::Before::File { sha256, .. } => Some(sha256),
            _ => None,
        })
        .collect();
    let (mut count, mut bytes) = (0usize, 0u64);
    for id in ids {
        if let Some(content) = objects.get(&id)? {
            sink.put(&format!("checkpoints/{id}"), &content, true)?;
            count += 1;
            bytes += content.len() as u64;
        }
    }
    Ok(json!({ "objects": count, "bytes": bytes, "pruned_before": null }))
}

/// Every saved file a marker in any of the sessions' logs points at, under
/// `tool-results/<name>`; `missing` counts markers whose file is gone.
pub(super) fn pack_saved_results(
    part: &Option<SavedResultsPart>,
    sessions: &[SessionPart],
    sink: &mut dyn Sink,
) -> std::io::Result<Value> {
    let Some(part) = part else {
        return Ok(Value::Null);
    };
    let names = runtime::saved_results::marked_in(
        sessions.iter().flat_map(|s| &s.entries).map(|e| &e.entry),
        &part.dir,
    );
    let (mut files, mut missing) = (Vec::new(), 0usize);
    for name in names {
        match std::fs::read(part.dir.join(&name)) {
            Ok(bytes) => {
                sink.put(&format!("tool-results/{name}"), &bytes, true)?;
                files.push(json!({
                    "name": name,
                    "sha256": hex::encode(Sha256::digest(&bytes)),
                    "bytes": bytes.len(),
                }));
            }
            Err(_) => missing += 1,
        }
    }
    Ok(json!({ "dir": part.dir.display().to_string(), "files": files, "missing": missing }))
}
