//! 宿主 skill 目录的校验对着会话沙箱实际放开的写根：引擎给会话的每一个可写根，里面的目录
//! 都不收。

use super::mcp_for_project::test_pool;
use super::*;
use crate::session_pool::api::SkillDirOption;
use crate::session_pool::parts::bindings::HostDirTable;
use base::context::Session;

#[tokio::test]
async fn every_root_a_sessions_sandboxed_commands_write_is_refused_as_a_skill_dir() {
    let (pool, _dir) = test_pool().await;
    let sid = pool
        .create_session(None, ProjectSelector::Default, None)
        .await
        .expect("a session")
        .session_id;
    let state = pool
        .live
        .with_live(&sid, |live| live.core.state().clone())
        .await
        .expect("the session is live");
    let env = state.env();
    let core = pool.live.core_of(&sid).await.expect("in memory");
    let binding = pool.binding_for(core.project(), &core.scene().id).await;
    let sensitive = base::paths::SensitivePaths::for_paths(
        &base::paths::ConfigPaths::from_settings(&binding.settings.paths),
    );
    let session_tmp = sensitive.session_tmp_dir(&sid).expect("a state root");
    let roots: Vec<PathBuf> = tools::bash::writable_dirs(&env, session_tmp)
        .into_iter()
        .chain(platform::sandbox::implicit_writable_roots(false))
        .chain([env.cwd.clone()])
        .collect();
    assert!(!env.cache_directories.is_empty());
    for root in roots {
        let listed = SkillDirOption {
            path: root.join("host-skills").display().to_string(),
        };
        assert!(
            HostDirTable::check(&[listed], &binding.settings).is_err(),
            "a host skill dir under {} is accepted",
            root.display()
        );
    }
}
