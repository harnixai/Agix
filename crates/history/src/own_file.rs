//! A session's own files opened as the path itself: a symlink there is an
//! error, not followed. A session kept in its project has its files inside a
//! repository, where a link can be anyone's.

use std::path::Path;
use tokio::io::AsyncReadExt;

pub(crate) async fn open(path: &Path) -> std::io::Result<tokio::fs::File> {
    let mut options = base::fs::no_follow();
    options.read(true);
    tokio::fs::OpenOptions::from(options).open(path).await
}

pub(crate) async fn read(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    open(path).await?.read_to_end(&mut bytes).await?;
    Ok(bytes)
}

pub(crate) async fn read_to_string(path: &Path) -> std::io::Result<String> {
    String::from_utf8(read(path).await?)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}
