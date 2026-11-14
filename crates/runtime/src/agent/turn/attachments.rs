//! 用户输入附件：图片/文本的识别、内联与降级。

use base::interface::model::ModelContentBlock;

/// Media type for an image path, by extension. `None` for anything the
/// Anthropic API does not accept as an image, which is the signal to fall back
/// to reading the file as text.
pub(super) fn image_media_type(path: &std::path::Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => Some("image/png"),
        Some("jpg") | Some("jpeg") => Some("image/jpeg"),
        Some("gif") => Some("image/gif"),
        Some("webp") => Some("image/webp"),
        _ => None,
    }
}

pub(super) const MAX_ATTACHMENT_IMAGE_BYTES: u64 = base::tool::MAX_IMAGE_BYTES;

/// Anthropic caps a request at 32 MB, and base64 adds a third.
pub(super) const MAX_ATTACHMENT_DOCUMENT_BYTES: u64 = 20 * 1024 * 1024;

const PDF_MEDIA_TYPE: &str = "application/pdf";

fn is_pdf_path(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
}

/// What a prompt middleware is shown of the attachments.
pub(super) async fn describe_attachments(
    attachments: &[crate::agent::Attachment],
) -> Vec<base::interface::prompt_middleware::PromptAttachment<'_>> {
    use crate::agent::Attachment;
    use base::interface::prompt_middleware::PromptAttachment;

    let mut described = Vec::with_capacity(attachments.len());
    for att in attachments {
        described.push(match att {
            Attachment::File { path } => {
                let size = tokio::fs::metadata(path).await.ok().map(|m| m.len());
                let media_type = image_media_type(std::path::Path::new(path))
                    .or_else(|| is_pdf_path(std::path::Path::new(path)).then_some(PDF_MEDIA_TYPE));
                PromptAttachment::file(path, media_type, size)
            }
            Attachment::Text { path, content } => PromptAttachment::text(path, content),
            Attachment::Document {
                media_type,
                data,
                name,
            } => PromptAttachment::document(name.as_deref(), media_type, base64_decoded_len(data)),
            Attachment::Image { media_type, data } => {
                PromptAttachment::image(media_type, base64_decoded_len(data))
            }
        });
    }
    described
}

pub(super) fn base64_decoded_len(data: &str) -> u64 {
    let padding = data.bytes().rev().take_while(|b| *b == b'=').count().min(2);
    ((data.len() / 4) * 3).saturating_sub(padding) as u64
}

/// Turn attachments into content blocks for the user message.
///
/// Failures degrade to an explanatory text block rather than propagating: an
/// unreadable attachment should not fail the user's whole turn, and telling
/// the model "this attachment could not be read" is more useful than silently
/// dropping it (the user will refer to it, and the model needs to be able to
/// say why it cannot see it).
pub(super) async fn resolve_attachments(
    attachments: &[crate::agent::Attachment],
    files: &base::paths::PromptFiles,
) -> Vec<ModelContentBlock> {
    use crate::agent::Attachment;
    use base64::Engine as _;

    let mut blocks = Vec::new();
    for att in attachments {
        match att {
            Attachment::Image { media_type, data } => blocks.push(ModelContentBlock::Image {
                media_type: media_type.clone(),
                data: data.clone(),
            }),
            Attachment::Text { path, content } => blocks.push(ModelContentBlock::Text {
                text: format!("<attachment path=\"{path}\">\n{content}\n</attachment>"),
            }),
            Attachment::Document {
                media_type,
                data,
                name,
            } => {
                let label = name.as_deref().unwrap_or("document");
                let size = base64_decoded_len(data);
                blocks.push(if media_type != PDF_MEDIA_TYPE {
                    ModelContentBlock::Text {
                        text: format!(
                            "<attachment path=\"{label}\" error=\"unsupported document type \
                             {media_type}; only {PDF_MEDIA_TYPE}\" />"
                        ),
                    }
                } else if size > MAX_ATTACHMENT_DOCUMENT_BYTES {
                    ModelContentBlock::Text {
                        text: document_too_large(label, size),
                    }
                } else {
                    ModelContentBlock::Document {
                        media_type: media_type.clone(),
                        data: data.clone(),
                        title: name.clone(),
                    }
                });
            }
            Attachment::File { path } => {
                let p = match files.resolve(std::path::Path::new(path)) {
                    Ok(resolved) => resolved,
                    Err(e) => {
                        blocks.push(ModelContentBlock::Text {
                            text: format!("<attachment path=\"{path}\" error=\"{e}\" />"),
                        });
                        continue;
                    }
                };
                let p = p.as_path();
                if is_pdf_path(std::path::Path::new(path)) {
                    blocks.push(read_pdf(p, path).await);
                    continue;
                }
                match image_media_type(std::path::Path::new(path)) {
                    Some(media_type) => {
                        let size = tokio::fs::metadata(p).await.map(|m| m.len()).unwrap_or(0);
                        if size > MAX_ATTACHMENT_IMAGE_BYTES {
                            blocks.push(ModelContentBlock::Text {
                                text: format!(
                                    "<attachment path=\"{path}\" error=\"image too large: \
                                     {size} bytes, cap is {MAX_ATTACHMENT_IMAGE_BYTES}\" />"
                                ),
                            });
                            continue;
                        }
                        match tokio::fs::read(p).await {
                            Ok(bytes) => blocks.push(ModelContentBlock::Image {
                                media_type: media_type.to_string(),
                                data: base64::engine::general_purpose::STANDARD.encode(&bytes),
                            }),
                            Err(e) => blocks.push(ModelContentBlock::Text {
                                text: format!("<attachment path=\"{path}\" error=\"{e}\" />"),
                            }),
                        }
                    }
                    None => match tokio::fs::read_to_string(p).await {
                        Ok(text) => blocks.push(ModelContentBlock::Text {
                            text: format!("<attachment path=\"{path}\">\n{text}\n</attachment>"),
                        }),
                        Err(e) => blocks.push(ModelContentBlock::Text {
                            text: format!("<attachment path=\"{path}\" error=\"{e}\" />"),
                        }),
                    },
                }
            }
        }
    }
    blocks
}

fn document_too_large(label: &str, size: u64) -> String {
    format!(
        "<attachment path=\"{label}\" error=\"document too large: {size} bytes, cap is \
         {MAX_ATTACHMENT_DOCUMENT_BYTES}\" />"
    )
}

async fn read_pdf(resolved: &std::path::Path, path: &str) -> ModelContentBlock {
    use base64::Engine as _;

    let error = |reason: String| ModelContentBlock::Text {
        text: format!("<attachment path=\"{path}\" error=\"{reason}\" />"),
    };
    let size = match tokio::fs::metadata(resolved).await {
        Ok(meta) => meta.len(),
        Err(e) => return error(e.to_string()),
    };
    if size > MAX_ATTACHMENT_DOCUMENT_BYTES {
        return ModelContentBlock::Text {
            text: document_too_large(path, size),
        };
    }
    match tokio::fs::read(resolved).await {
        Ok(bytes) => ModelContentBlock::Document {
            media_type: PDF_MEDIA_TYPE.to_string(),
            data: base64::engine::general_purpose::STANDARD.encode(&bytes),
            title: std::path::Path::new(path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned()),
        },
        Err(e) => error(e.to_string()),
    }
}

/// Truncate `s` to at most `max_bytes` bytes without splitting a multi-byte
/// UTF-8 character. A module-local alias for `base::text`'s, which all
/// truncation sites share.
pub(super) fn truncate_at_char_boundary(s: &str, max_bytes: usize) -> &str {
    base::text::truncate_at_char_boundary(s, max_bytes)
}
