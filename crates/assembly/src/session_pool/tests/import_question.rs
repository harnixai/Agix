//! The `import_detected` question the daemon asks at startup about its own
//! project stays until that project's import answers it.

use super::mcp_for_project::test_pool;
use super::*;

#[tokio::test]
async fn importing_another_project_leaves_the_question_about_this_one() {
    let (pool, dir) = test_pool().await;
    let other = dir.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join(".cursorrules"), "be brief\n").unwrap();
    pool.ask_import(api::DaemonEvent::ImportDetected {
        sources: vec![api::ImportSource {
            source: "cursor".into(),
            description: "rules".into(),
        }],
    });

    pool.run_import(Some(&other), "cursor").await.unwrap();
    assert!(
        pool.pending_import_question().is_some(),
        "an import of another project answered this project's question"
    );
}
