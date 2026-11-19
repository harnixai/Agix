//! 导出导入的根检查不靠存储自报：只实现必选方法的存储下，全局历史目录照样挡住。

use super::mcp_for_project::test_pool_with_store;
use super::*;

fn refused_as_inside(result: Result<impl std::fmt::Debug, PoolError>) {
    match result {
        Err(PoolError::InvalidRequest(m)) if m.contains("inside the session history store") => {}
        other => panic!("not refused as inside the history: {other:?}"),
    }
}

#[tokio::test]
async fn export_and_import_keep_off_the_global_history_whatever_the_store_answers() {
    let store: Arc<dyn history::store::HistoryStore> =
        Arc::new(history::store::InMemoryHistoryStore::new());
    let (pool, dir) = test_pool_with_store(Some(store)).await;
    let sessions = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();

    refused_as_inside(
        pool.export_session(crate::export::ExportRequest {
            session_id: base::session::SessionId::new().to_string(),
            path: sessions.join("out.zip"),
            format: Default::default(),
            include: Default::default(),
        })
        .await,
    );

    let archive = sessions.join("in.zip");
    std::fs::write(&archive, b"").unwrap();
    refused_as_inside(
        pool.import_session(crate::import::ImportRequest {
            path: archive,
            project_root: Default::default(),
            keep_ids: false,
        })
        .await,
    );
}
