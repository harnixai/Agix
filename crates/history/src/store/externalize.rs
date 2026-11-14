//! Moving bulky entry content (large text, tool results, any image) out of
//! the log into the blob store and back.

use crate::blob::{BlobRef, BlobStore};
use crate::entry::{EnvelopedEntry, LogEntry};
use crate::error::HistoryError;
use base::message::{ContentBlock, ToolResultContent};

/// Serialized content above this is worth a round trip to the blob store.
pub(super) const BULKY_CONTENT_BYTES: usize = 1024;

/// Replace `entry` with a reference if it is one of the bulky kinds.
///
/// The whole entry is what gets stored, tag and auxiliary fields included, so
/// the blob is self-describing and hydration is a plain deserialize rather
/// than a reconstruction that has to agree with this function.
pub(super) fn externalize_if_bulky(
    entry: LogEntry,
    blobs: &dyn BlobStore,
) -> Result<LogEntry, HistoryError> {
    let Some(describes) = externalizable_kind(&entry) else {
        return Ok(entry);
    };
    let json = serde_json::to_string(&entry)?;
    let id = blobs.put(json.as_bytes()).map_err(HistoryError::Io)?;
    Ok(LogEntry::Blob {
        blob: BlobRef {
            store: blobs.name().to_string(),
            id,
            describes: describes.to_string(),
            bytes: json.len() as u64,
        },
    })
}

/// The `kind` tag of an entry worth moving out, or `None` to leave it inline.
///
/// Images leave at any size. Their bytes are never read as text, never
/// matched by a search and never skimmed by a person, so the threshold that
/// exists to keep short messages readable does not apply to them.
pub(super) fn externalizable_kind(entry: &LogEntry) -> Option<&'static str> {
    let (kind, blocks): (_, &[ContentBlock]) = match entry {
        LogEntry::User { content } => ("user", content),
        LogEntry::Assistant { content, .. } => ("assistant", content),
        LogEntry::ToolResult { content, .. } => {
            let carries_image = match content {
                ToolResultContent::Text(_) => false,
                ToolResultContent::Blocks(blocks) => blocks_carry_image(blocks),
            };
            return (carries_image || serialized_len(content) > BULKY_CONTENT_BYTES)
                .then_some("tool_result");
        }
        _ => return None,
    };
    (blocks_carry_image(blocks) || serialized_len(blocks) > BULKY_CONTENT_BYTES).then_some(kind)
}

pub(super) fn blocks_carry_image(blocks: &[ContentBlock]) -> bool {
    blocks.iter().any(|block| match block {
        ContentBlock::Image { .. } => true,
        ContentBlock::ToolResult {
            content: ToolResultContent::Blocks(nested),
            ..
        } => blocks_carry_image(nested),
        _ => false,
    })
}

/// The content's JSON size — the dominant term in the serialized line.
pub(super) fn serialized_len<T: serde::Serialize + ?Sized>(value: &T) -> usize {
    serde_json::to_string(value).map(|s| s.len()).unwrap_or(0)
}

/// Put the original entry back, if this entry is a reference and `blobs` has
/// what it points at.
///
/// Every other case leaves the entry as it is, deliberately and without an
/// error: a reference into a store nobody mounted, content that was cleaned
/// up, or a blob that no longer deserializes are all reasons for a session to
/// load with a gap and none of them are reasons for it not to load.
pub(super) fn hydrate(env: &mut EnvelopedEntry, blobs: &dyn BlobStore) {
    let id = match &env.entry {
        LogEntry::Blob { blob } if blob.store == blobs.name() => &blob.id,
        _ => return,
    };
    let fetched = match blobs.get(id) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(blob = %id, error = %e, "blob store unreadable; leaving the reference in place");
            return;
        }
    };
    match serde_json::from_slice::<LogEntry>(&fetched) {
        Ok(hydrated) => env.entry = hydrated,
        Err(e) => {
            tracing::warn!(blob = %id, error = %e, "blob does not deserialize; leaving the reference in place");
        }
    }
}
