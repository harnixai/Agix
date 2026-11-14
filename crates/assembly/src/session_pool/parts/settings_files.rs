//! 改写 settings 目录里的文件：持目录锁、在阻塞线程上执行。

use std::path::Path;

/// Run `edit` holding the settings directory `settings_file` is in — see
/// `settings::file` — on a blocking thread, since the lock can wait on
/// another process.
pub(in crate::session_pool) async fn edit_settings_dir<R: Send + 'static>(
    settings_file: &Path,
    edit: impl FnOnce(&settings::file::SettingsDir) -> Result<R, String> + Send + 'static,
) -> Result<R, String> {
    let dir = settings_file
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    tokio::task::spawn_blocking(move || {
        let tier = settings::file::SettingsDir::lock(&dir).map_err(|e| e.to_string())?;
        edit(&tier)
    })
    .await
    .map_err(|e| format!("writing settings did not finish: {e}"))?
}
