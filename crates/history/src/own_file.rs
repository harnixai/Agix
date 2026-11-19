//! A session's own files opened as the path itself: on Unix a symlink there
//! fails the open (`ELOOP`) instead of being followed. A session kept in its
//! project has its files inside a repository, where a link can be anyone's.

use std::path::Path;
use tokio::io::AsyncReadExt;

pub fn options() -> std::fs::OpenOptions {
    #[allow(unused_mut)]
    let mut options = std::fs::OpenOptions::new();
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut options, libc::O_NOFOLLOW);
    options
}

pub fn read_blocking(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    options().read(true).open(path)?.read_to_end(&mut bytes)?;
    Ok(bytes)
}

pub fn read_to_string_blocking(path: &Path) -> std::io::Result<String> {
    text(read_blocking(path)?)
}

pub async fn open(path: &Path) -> std::io::Result<tokio::fs::File> {
    let mut options = options();
    options.read(true);
    tokio::fs::OpenOptions::from(options).open(path).await
}

pub async fn read(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    open(path).await?.read_to_end(&mut bytes).await?;
    Ok(bytes)
}

pub async fn read_to_string(path: &Path) -> std::io::Result<String> {
    text(read(path).await?)
}

fn text(bytes: Vec<u8>) -> std::io::Result<String> {
    String::from_utf8(bytes).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_symlink_is_not_read_through() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        std::fs::write(&target, "secret").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(read_blocking(&link).is_err());
        assert!(read(&link).await.is_err());
        assert_eq!(read_to_string(&target).await.unwrap(), "secret");
    }
}
