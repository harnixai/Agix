use super::blobs::text_block;
use super::*;
use crate::entry::{self, SessionKind};
use crate::placement::{ProjectHistory, ProjectHistoryDir, SessionIndex};
use std::sync::atomic::{AtomicBool, Ordering};
use time::OffsetDateTime;

struct Projects {
    keep: bool,
    trusted: AtomicBool,
}

impl Projects {
    fn new(keep: bool) -> Arc<Self> {
        Arc::new(Self {
            keep,
            trusted: AtomicBool::new(true),
        })
    }
}

fn main_of(root: &Path) -> PathBuf {
    match root.parent() {
        Some(wkts) if wkts.file_name().is_some_and(|n| n == ".wkts") => {
            wkts.parent().unwrap().to_path_buf()
        }
        _ => root.to_path_buf(),
    }
}

fn history_dir(root: &Path) -> PathBuf {
    root.join(".agix/state/history")
}

impl ProjectHistory for Projects {
    fn history_of(&self, root: &Path) -> Option<ProjectHistoryDir> {
        let root = main_of(root);
        Some(ProjectHistoryDir {
            dir: history_dir(&root),
            root,
        })
    }

    fn keeps_new(&self, root: &Path) -> bool {
        self.keep && self.trusted(root)
    }

    fn trusted(&self, _root: &Path) -> bool {
        self.trusted.load(Ordering::SeqCst)
    }
}

struct World {
    state: TempDir,
    projects_parent: TempDir,
}

impl World {
    fn new() -> Self {
        Self {
            state: TempDir::new().unwrap(),
            projects_parent: TempDir::new().unwrap(),
        }
    }

    fn project(&self, name: &str) -> PathBuf {
        let root = self
            .projects_parent
            .path()
            .canonicalize()
            .unwrap()
            .join(name);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn roots(&self) -> HistoryRoots {
        HistoryRoots::under(self.state.path())
    }

    async fn store(&self, cwd: &Path, projects: Option<Arc<Projects>>) -> JsonlHistoryStore {
        let store = JsonlHistoryStore::with_roots(cwd, self.roots())
            .await
            .unwrap();
        match projects {
            Some(p) => store.with_project_history(p),
            None => store,
        }
    }
}

fn meta(project: Option<&Path>, parent: Option<SessionId>) -> LogEntry {
    LogEntry::Meta {
        cwd: project.map_or("/".into(), |p| p.display().to_string()),
        started_at: OffsetDateTime::now_utc(),
        model: "m".into(),
        permission_mode: "default".into(),
        engine_version: "0".into(),
        agix_version: "0".into(),
        parent_session_id: parent.map(|p| p.to_string()),
        scene: Some("code".into()),
        project_root: project.map(|p| p.display().to_string()),
        session_kind: if parent.is_some() {
            SessionKind::Sidechain
        } else {
            SessionKind::Primary
        },
        schema_version: entry::CURRENT_META_SCHEMA_VERSION,
    }
}

fn said(text: &str) -> LogEntry {
    LogEntry::User {
        content: vec![text_block(text)],
    }
}

async fn start(store: &JsonlHistoryStore, project: &Path) -> SessionId {
    let s = SessionId::new();
    store.append(s, meta(Some(project), None)).await.unwrap();
    store.append(s, said("hello")).await.unwrap();
    s
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

async fn listed(store: &JsonlHistoryStore, project: &Path) -> Vec<SessionId> {
    store
        .find_sessions(
            &SessionQuery::recent(50).within(SessionScope::Project(Some(project.to_path_buf()))),
        )
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.session_id)
        .collect()
}

#[tokio::test]
async fn a_trusted_projects_session_is_kept_in_it_and_the_global_tier_holds_only_its_index() {
    let world = World::new();
    let project = world.project("p");
    let store = world.store(&project, Some(Projects::new(true))).await;
    let s = start(&store, &project).await;

    let dir = history_dir(&project).join(s.to_string());
    assert!(dir.join(format!("{s}.jsonl")).is_file());
    assert!(dir.join("metadata.json").is_file());
    assert_eq!(
        names_in(&world.roots().sessions.join(s.to_string())),
        ["index.json", "writer.lock"]
    );
    assert!(names_in(&world.roots().projects).is_empty());
    let index = SessionIndex::read(&world.roots().sessions, s)
        .unwrap()
        .unwrap();
    assert_eq!(index.project_root, project);
    assert_eq!(index.dir, dir);

    drop(store);
    let again = world.store(&project, Some(Projects::new(true))).await;
    assert_eq!(again.load(s).await.unwrap().len(), 2);
    assert_eq!(listed(&again, &project).await, [s]);
    assert_eq!(again.list_sessions().await.unwrap(), [s]);
    assert_eq!(
        again.sidecar_locator(s).unwrap().dir().unwrap(),
        dir,
        "memory and input history sit beside the transcript"
    );
    let everywhere = again
        .find_sessions(&SessionQuery::matching("hello", 10).within(SessionScope::AllProjects))
        .await
        .unwrap();
    assert_eq!(everywhere.len(), 1);
}

#[tokio::test]
async fn a_session_in_the_projects_directory_that_no_index_names_is_not_read() {
    let world = World::new();
    let project = world.project("p");
    let store = world.store(&project, Some(Projects::new(true))).await;
    let real = start(&store, &project).await;

    let planted = SessionId::new();
    let real_dir = history_dir(&project).join(real.to_string());
    let planted_dir = history_dir(&project).join(planted.to_string());
    std::fs::create_dir_all(&planted_dir).unwrap();
    std::fs::copy(
        real_dir.join(format!("{real}.jsonl")),
        planted_dir.join(format!("{planted}.jsonl")),
    )
    .unwrap();
    std::fs::write(planted_dir.join("session_memory.md"), "planted").unwrap();

    let fresh = world.store(&project, Some(Projects::new(true))).await;
    assert_eq!(listed(&fresh, &project).await, [real]);
    assert!(matches!(
        fresh.load(planted).await,
        Err(HistoryError::SessionNotFound(_))
    ));
    assert_eq!(
        fresh.snapshot_lineage(real).await.unwrap().sessions.len(),
        1
    );
    assert!(!fresh
        .sidecar_locator(planted)
        .unwrap()
        .dir()
        .unwrap()
        .starts_with(&project));
    let everywhere = fresh
        .find_sessions(&SessionQuery::matching("", 10).within(SessionScope::AllProjects))
        .await
        .unwrap();
    assert_eq!(everywhere.len(), 1);
}

#[tokio::test]
async fn history_kept_in_a_project_no_longer_trusted_is_not_read_and_says_why() {
    let world = World::new();
    let project = world.project("p");
    let projects = Projects::new(true);
    let store = world.store(&project, Some(projects.clone())).await;
    let s = start(&store, &project).await;

    projects.trusted.store(false, Ordering::SeqCst);
    let err = store.load(s).await.unwrap_err();
    assert!(
        matches!(&err, HistoryError::ProjectUntrusted { project: p, .. } if *p == project),
        "{err}"
    );
    assert!(err.to_string().contains("not trusted"));
    assert!(listed(&store, &project).await.is_empty());
    assert!(store.sidecar_locator(s).unwrap().dir().is_err());
    assert!(store.checkpoint_objects(s).unwrap().list().is_err());

    projects.trusted.store(true, Ordering::SeqCst);
    assert_eq!(store.load(s).await.unwrap().len(), 2);
}

#[tokio::test]
async fn an_untrusted_project_or_the_global_setting_keeps_sessions_globally() {
    let world = World::new();
    let project = world.project("p");
    let untrusted = Projects::new(true);
    untrusted.trusted.store(false, Ordering::SeqCst);
    for projects in [Some(untrusted), Some(Projects::new(false)), None] {
        let store = world.store(&project, projects).await;
        let s = start(&store, &project).await;
        assert!(store.session_file_path(&s).is_file());
        assert!(!history_dir(&project).join(s.to_string()).exists());
        assert!(world
            .roots()
            .sessions
            .join(s.to_string())
            .join("metadata.json")
            .is_file());
    }
}

#[tokio::test]
async fn a_worktrees_session_is_kept_in_the_main_checkout() {
    let world = World::new();
    let main = world.project("repo");
    let worktree = main.join(".wkts/w1");
    std::fs::create_dir_all(&worktree).unwrap();
    let store = world.store(&main, Some(Projects::new(true))).await;
    let s = start(&store, &worktree).await;

    assert!(history_dir(&main)
        .join(s.to_string())
        .join(format!("{s}.jsonl"))
        .is_file());
    assert!(!worktree.join(".agix").exists());
    let index = SessionIndex::read(&world.roots().sessions, s)
        .unwrap()
        .unwrap();
    assert_eq!(index.project_root, main);
    assert_eq!(listed(&store, &worktree).await, [s]);
    assert!(listed(&store, &main).await.is_empty());
}

#[tokio::test]
async fn a_sidechain_is_kept_where_its_parent_is_and_goes_with_it() {
    let world = World::new();
    let project = world.project("p");
    let store = world.store(&project, Some(Projects::new(true))).await;
    let parent = start(&store, &project).await;
    let child = SessionId::new();
    store
        .append(child, meta(Some(&project), Some(parent)))
        .await
        .unwrap();
    store.append(child, said("sub")).await.unwrap();

    assert!(history_dir(&project).join(child.to_string()).is_dir());
    let lineage = store.snapshot_lineage(parent).await.unwrap();
    let ids: Vec<SessionId> = lineage.sessions.iter().map(|(s, _)| *s).collect();
    assert_eq!(ids, [parent, child]);
    assert_eq!(
        store.child_sessions(&parent.to_string()).await.unwrap(),
        [child]
    );

    store.delete(child).await.unwrap();
    store.delete(parent).await.unwrap();
    assert!(names_in(&history_dir(&project)).is_empty());
    assert!(names_in(&world.roots().sessions).is_empty());
}

#[tokio::test]
async fn a_moved_project_is_found_where_it_is_now_and_the_index_follows() {
    let world = World::new();
    let old = world.project("old");
    let store = world.store(&old, Some(Projects::new(true))).await;
    let s = start(&store, &old).await;
    drop(store);

    let new = world
        .projects_parent
        .path()
        .canonicalize()
        .unwrap()
        .join("new");
    std::fs::rename(&old, &new).unwrap();
    let moved = world.store(&new, Some(Projects::new(true))).await;
    let entries = moved.load(s).await.unwrap();
    assert_eq!(entries.len(), 2);
    let facts = moved.session_facts(s).await.unwrap();
    assert_eq!(facts.project_root, Some(new.display().to_string()));
    let index = SessionIndex::read(&world.roots().sessions, s)
        .unwrap()
        .unwrap();
    assert_eq!(index.project_root, new);
    assert_eq!(index.recorded_root, Some(old.clone()));
    assert_eq!(listed(&moved, &new).await, [s]);
    let summary = &moved
        .find_sessions(&SessionQuery::recent(5).within(SessionScope::Project(Some(new.clone()))))
        .await
        .unwrap()[0];
    assert_eq!(summary.canonical_cwd, Some(new.display().to_string()));
}

#[tokio::test]
async fn a_project_that_moved_does_not_take_over_an_id_its_index_lacks() {
    let world = World::new();
    let old = world.project("old");
    let store = world.store(&old, Some(Projects::new(true))).await;
    let s = start(&store, &old).await;
    drop(store);
    std::fs::remove_file(
        world
            .roots()
            .sessions
            .join(s.to_string())
            .join("index.json"),
    )
    .unwrap();

    let new = world
        .projects_parent
        .path()
        .canonicalize()
        .unwrap()
        .join("new");
    std::fs::rename(&old, &new).unwrap();
    let moved = world.store(&new, Some(Projects::new(true))).await;
    assert!(moved.load(s).await.is_err());
    assert!(listed(&moved, &new).await.is_empty());
    assert!(SessionIndex::read(&world.roots().sessions, s)
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn a_global_session_is_read_as_before_once_sessions_go_to_projects() {
    let world = World::new();
    let project = world.project("p");
    let before = world.store(&project, None).await;
    let old = start(&before, &project).await;
    drop(before);

    let store = world.store(&project, Some(Projects::new(true))).await;
    let new = start(&store, &project).await;
    assert_eq!(store.load(old).await.unwrap().len(), 2);
    let mut both = listed(&store, &project).await;
    both.sort_by_key(SessionId::to_string);
    let mut expected = vec![old, new];
    expected.sort_by_key(SessionId::to_string);
    assert_eq!(both, expected);
    assert!(store.session_file_path(&old).is_file());
}

#[tokio::test]
async fn the_store_names_the_projects_history_directory_around_a_path() {
    let world = World::new();
    let project = world.project("p");
    let store = world.store(&project, Some(Projects::new(true))).await;
    let inside = history_dir(&project).join("x.zip");
    let dirs = store.history_dirs_for(&inside);
    assert!(dirs.contains(&history_dir(&project)));
    assert!(dirs.contains(&world.roots().sessions));
}

#[cfg(unix)]
#[tokio::test]
async fn the_history_directory_is_named_around_a_path_through_a_linked_parent() {
    let world = World::new();
    let project = world.project("p");
    let store = world.store(&project, Some(Projects::new(true))).await;
    start(&store, &project).await;
    let link = world.project("elsewhere").join("lnk");
    std::os::unix::fs::symlink(history_dir(&project), &link).unwrap();
    let dirs = store.history_dirs_for(&link.join("out.zip"));
    assert!(dirs.contains(&history_dir(&project)), "{dirs:?}");
}

fn mentions(entries: &[EnvelopedEntry], text: &str) -> bool {
    serde_json::to_string(entries).unwrap().contains(text)
}

#[tokio::test]
async fn a_new_session_does_not_take_over_a_directory_already_under_its_id() {
    let world = World::new();
    let project = world.project("p");
    let other_state = TempDir::new().unwrap();
    let other = JsonlHistoryStore::with_roots(&project, HistoryRoots::under(other_state.path()))
        .await
        .unwrap()
        .with_project_history(Projects::new(true));
    let s = SessionId::new();
    other.append(s, meta(Some(&project), None)).await.unwrap();
    other.append(s, said("PLANTED")).await.unwrap();
    other
        .set_session_title(s, Some("planted".into()))
        .await
        .unwrap();

    let store = world.store(&project, Some(Projects::new(true))).await;
    store.append(s, meta(Some(&project), None)).await.unwrap();
    store.append(s, said("real")).await.unwrap();

    let mine = store.load(s).await.unwrap();
    assert!(mentions(&mine, "real") && !mentions(&mine, "PLANTED"));
    assert_ne!(
        store.session_stats(s).await.unwrap().title.as_deref(),
        Some("planted")
    );
    let theirs = other.load(s).await.unwrap();
    assert!(mentions(&theirs, "PLANTED") && !mentions(&theirs, "real"));
}

#[cfg(unix)]
#[tokio::test]
async fn a_planted_transcript_link_is_not_written_through() {
    let world = World::new();
    let project = world.project("p");
    let outside = world.project("outside").join("outside.txt");
    std::fs::write(&outside, "untouched").unwrap();
    let s = SessionId::new();
    let planted = history_dir(&project).join(s.to_string());
    std::fs::create_dir_all(&planted).unwrap();
    std::os::unix::fs::symlink(&outside, planted.join(format!("{s}.jsonl"))).unwrap();

    let store = world.store(&project, Some(Projects::new(true))).await;
    store.append(s, meta(Some(&project), None)).await.unwrap();
    store.append(s, said("real")).await.unwrap();
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "untouched");
    assert!(mentions(&store.load(s).await.unwrap(), "real"));
}

#[cfg(unix)]
#[tokio::test]
async fn a_kept_sessions_files_swapped_for_links_are_not_followed() {
    let world = World::new();
    let project = world.project("p");
    let elsewhere = world.project("elsewhere");
    let store = world.store(&project, Some(Projects::new(true))).await;
    let s = start(&store, &project).await;
    store
        .set_session_title(s, Some("mine".into()))
        .await
        .unwrap();
    drop(store);
    let dir = history_dir(&project).join(s.to_string());

    let outside_log = elsewhere.join("log.jsonl");
    std::fs::rename(dir.join(format!("{s}.jsonl")), &outside_log).unwrap();
    let before = std::fs::read(&outside_log).unwrap();
    std::os::unix::fs::symlink(&outside_log, dir.join(format!("{s}.jsonl"))).unwrap();

    let mut metadata: SessionMetadata =
        serde_json::from_str(&std::fs::read_to_string(dir.join("metadata.json")).unwrap()).unwrap();
    metadata.title = Some("planted".into());
    let outside_metadata = elsewhere.join("metadata.json");
    std::fs::write(&outside_metadata, serde_json::to_vec(&metadata).unwrap()).unwrap();
    std::fs::remove_file(dir.join("metadata.json")).unwrap();
    std::os::unix::fs::symlink(&outside_metadata, dir.join("metadata.json")).unwrap();

    let fresh = world.store(&project, Some(Projects::new(true))).await;
    assert!(fresh.load(s).await.is_err());
    assert_ne!(
        fresh
            .session_stats(s)
            .await
            .ok()
            .and_then(|stats| stats.title)
            .as_deref(),
        Some("planted")
    );
    assert!(fresh.append(s, said("more")).await.is_err());
    assert_eq!(std::fs::read(&outside_log).unwrap(), before);
}

#[tokio::test]
async fn a_lineage_in_a_project_no_longer_trusted_is_still_found_for_deleting() {
    let world = World::new();
    let project = world.project("p");
    let projects = Projects::new(true);
    let store = world.store(&project, Some(projects.clone())).await;
    let parent = start(&store, &project).await;
    let child = SessionId::new();
    store
        .append(child, meta(Some(&project), Some(parent)))
        .await
        .unwrap();
    drop(store);

    projects.trusted.store(false, Ordering::SeqCst);
    let fresh = world.store(&project, Some(projects)).await;
    assert_eq!(
        fresh.child_sessions(&parent.to_string()).await.unwrap(),
        [child]
    );
    let facts = fresh.session_facts(child).await.unwrap();
    assert_eq!(facts.session_kind, SessionKind::Sidechain);
    assert_eq!(facts.parent_session_id, Some(parent.to_string()));
    assert!(fresh.load(child).await.is_err());
}
