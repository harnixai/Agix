//! 改写 settings 目录里的文件：持目录锁、在阻塞线程上执行。

use std::path::Path;

use crate::session_pool::PoolError;

pub(in crate::session_pool) enum SettingsEditError {
    Locked(String),
    Failed(String),
}

impl SettingsEditError {
    pub(in crate::session_pool) fn into_pool(self, failed: fn(String) -> PoolError) -> PoolError {
        match self {
            Self::Locked(message) => PoolError::SettingsLocked(message),
            Self::Failed(message) => failed(message),
        }
    }
}

impl From<SettingsEditError> for PoolError {
    fn from(e: SettingsEditError) -> Self {
        e.into_pool(PoolError::InvalidRequest)
    }
}

/// Run `edit` holding the settings directory `settings_file` is in — see
/// `settings::file` — on a blocking thread, since the lock can wait on
/// another process, for a bounded time.
pub(in crate::session_pool) async fn edit_settings_dir<R: Send + 'static>(
    settings_file: &Path,
    edit: impl FnOnce(&settings::file::SettingsDir) -> Result<R, String> + Send + 'static,
) -> Result<R, SettingsEditError> {
    let dir = settings_file
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    tokio::task::spawn_blocking(move || {
        let tier = settings::file::SettingsDir::lock(&dir).map_err(|e| match e {
            locked @ settings::file::SettingsFileError::Locked { .. } => {
                SettingsEditError::Locked(locked.to_string())
            }
            other => SettingsEditError::Failed(other.to_string()),
        })?;
        edit(&tier).map_err(SettingsEditError::Failed)
    })
    .await
    .map_err(|e| SettingsEditError::Failed(format!("writing settings did not finish: {e}")))?
}
