use super::*;
use base::interface::scene::{FixedPrompt, TokenBudget};
use std::borrow::Cow;

#[derive(Clone)]
struct TestSettings {
    prompt_append: Option<String>,
    prompt_override: Option<String>,
    managed_append: Option<String>,
    memory_enabled: bool,
}

struct TestLoaded {
    values: TestSettings,
}

fn scene_with(blocks: Vec<PromptBlock>) -> SceneSpec {
    let mut scene = SceneSpec::new("test", "Test", std::sync::Arc::new(FixedPrompt(blocks)));
    scene.token_budget = TokenBudget::new(1000, 5);
    scene
}

fn test_scene() -> SceneSpec {
    scene_with(vec![PromptBlock::system_cached("You are a test agent.")])
}

fn test_ctx() -> ScenePromptContext<'static> {
    {
        let mut ctx = ScenePromptContext::default();
        ctx.cwd = Cow::Borrowed("/tmp");
        ctx.os = Cow::Borrowed("linux");
        ctx.shell = Cow::Borrowed("bash");
        ctx.home_dir = Cow::Borrowed("/home/user");
        ctx.date = Cow::Borrowed("2026-06-10");
        ctx.model_name = Cow::Borrowed("test-model");
        ctx
    }
}

fn test_loaded(settings: TestSettings) -> TestLoaded {
    TestLoaded { values: settings }
}

fn assemble_for_test(
    registry: &dyn base::interface::prompt_registry::PromptRegistry,
    scene: &SceneSpec,
    settings: &TestSettings,
    ctx: &ScenePromptContext,
    rules: &[base::rules::RuleEntry],
    (skills_text, mcp_instructions): (Option<&str>, Option<&str>),
) -> Vec<PromptBlock> {
    let memory = settings.memory_enabled.then_some("memory instructions");
    assemble(&AssemblyRequest {
        registry,
        scene,
        prompt_override: settings.prompt_override.as_deref(),
        prompt_append: settings.prompt_append.as_deref(),
        managed_append: settings.managed_append.as_deref(),
        memory_prompt: memory,
        ctx,
        rules,
        skills_text,
        mcp_instructions,
    })
}

fn assemble_prompt(
    scene: &SceneSpec,
    settings: &TestLoaded,
    ctx: &ScenePromptContext,
    skills_text: Option<&str>,
    mcp_instructions: Option<&str>,
) -> Vec<PromptBlock> {
    assemble_for_test(
        &base::interface::prompt_registry::NoRegistrations,
        scene,
        &settings.values,
        ctx,
        &[],
        (skills_text, mcp_instructions),
    )
}

fn test_settings() -> TestSettings {
    TestSettings {
        prompt_append: None,
        prompt_override: None,
        managed_append: None,
        memory_enabled: true,
    }
}

#[test]
fn assemble_basic_prompt() {
    let scene = test_scene();
    let settings = test_settings();
    let ctx = test_ctx();

    let blocks = assemble_prompt(&scene, &test_loaded(settings), &ctx, None, None);
    assert!(!blocks.is_empty());
    assert_eq!(blocks[0].role, BlockRole::System);
}

#[test]
fn override_replaces_all() {
    let scene = test_scene();
    let mut settings = test_settings();
    settings.prompt_override = Some("OVERRIDE".into());
    let ctx = test_ctx();

    let blocks = assemble_prompt(&scene, &test_loaded(settings), &ctx, None, None);
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].content, "OVERRIDE");
}

#[test]
fn the_managed_append_follows_an_override() {
    let scene = test_scene();
    let mut settings = test_settings();
    settings.prompt_override = Some("OVERRIDE".into());
    settings.prompt_append = Some("LOWER".into());
    settings.managed_append = Some("MANAGED".into());
    let ctx = test_ctx();

    let blocks = assemble_prompt(&scene, &test_loaded(settings), &ctx, None, None);
    let contents: Vec<&str> = blocks.iter().map(|b| b.content.as_str()).collect();
    assert_eq!(contents, ["OVERRIDE", "MANAGED"]);
    assert_eq!(blocks[1].name.as_deref(), Some(names::CONFIG_PROMPT_APPEND));
}

/// Every block the kernel assembles must be addressable. An unnamed block
/// is one an extension can neither position against nor replace, and it
/// would be invisible in exactly the configuration that produced it.
#[test]
fn every_assembled_block_is_named_and_owned() {
    let scene = test_scene();
    let mut settings = test_settings();
    settings.prompt_append = Some("appended".into());
    settings.memory_enabled = true;
    let ctx = test_ctx();

    let blocks = assemble_prompt(
        &scene,
        &test_loaded(settings),
        &ctx,
        Some("skills inventory"),
        Some("mcp says hello"),
    );

    for b in &blocks {
        let name = b
            .name
            .as_deref()
            .unwrap_or_else(|| panic!("an unnamed block cannot be addressed: {b:?}"));
        assert!(!name.is_empty());
    }

    let named: Vec<&str> = blocks.iter().filter_map(|b| b.name.as_deref()).collect();
    assert!(named.contains(&names::SKILLS_CATALOG), "{named:?}");
    assert!(named.contains(&names::MEMORY_SESSION), "{named:?}");
    assert!(named.contains(&names::MCP_INSTRUCTIONS), "{named:?}");
    assert!(named.contains(&names::CONFIG_PROMPT_APPEND), "{named:?}");

    // A scene that names nothing gets the skeleton name; a scene that
    // names its sections keeps them, under the prefix.
    assert_eq!(blocks[0].name.as_deref(), Some(names::SCENE_SKELETON));

    let appended = blocks
        .iter()
        .find(|b| b.name.as_deref() == Some(names::CONFIG_PROMPT_APPEND))
        .unwrap();
    assert_eq!(
        appended.origin,
        BlockOrigin::Config,
        "a block the operator wrote is not the kernel's"
    );
    assert!(blocks[0].origin.is_kernel());
}

/// Sections a scene leaves unnamed are numbered, because a name is the
/// only handle anything downstream has on a block.
#[test]
fn unnamed_scene_sections_are_numbered_so_each_can_be_addressed() {
    let three_sections = scene_with(vec![
        PromptBlock::system("first"),
        PromptBlock::system("second"),
        PromptBlock::system("third"),
    ]);

    let settings = test_settings();
    let blocks = assemble_prompt(
        &three_sections,
        &test_loaded(settings),
        &test_ctx(),
        None,
        None,
    );

    let scene_names: Vec<&str> = blocks
        .iter()
        .filter_map(|b| b.name.as_deref())
        .filter(|n| n.starts_with(names::SCENE_SKELETON))
        .collect();
    assert_eq!(
        scene_names,
        vec!["scene.skeleton", "scene.skeleton.2", "scene.skeleton.3"],
        "every section must be addressable, and the first keeps the name \
         the documents use"
    );
}

#[test]
fn a_scene_that_names_its_sections_keeps_the_names_under_the_prefix() {
    let sectioned = scene_with(vec![
        PromptBlock::system("who you are").named("identity"),
        PromptBlock::system("what you may do").named("tools"),
    ]);

    let settings = test_settings();
    let blocks = assemble_prompt(&sectioned, &test_loaded(settings), &test_ctx(), None, None);

    assert_eq!(blocks[0].name.as_deref(), Some("scene.identity"));
    assert_eq!(blocks[1].name.as_deref(), Some("scene.tools"));
}

/// The property that makes registration safe to land before anything
/// registers: an empty registry assembles exactly what the hardcoded
/// function did.
#[test]
fn an_empty_registry_assembles_the_identical_prompt() {
    use base::interface::prompt_registry::NoRegistrations;

    let scene = test_scene();
    let mut settings = test_settings();
    settings.prompt_append = Some("appended".into());
    let ctx = test_ctx();

    let direct = assemble_prompt(
        &scene,
        &test_loaded(settings.clone()),
        &ctx,
        Some("skills"),
        Some("mcp"),
    );
    let through_registry = assemble_for_test(
        &NoRegistrations,
        &scene,
        &settings,
        &ctx,
        &[],
        (Some("skills"), Some("mcp")),
    );
    assert_eq!(direct, through_registry);
}

#[test]
fn a_registered_block_lands_where_its_order_puts_it_and_can_be_withdrawn() {
    use base::interface::prompt_registry::{
        orders, InMemoryPromptRegistry, PromptRegistry, RegisteredBlock,
    };

    let scene = test_scene();
    let settings = test_settings();
    let ctx = test_ctx();
    let registry = InMemoryPromptRegistry::new();

    // Between the scene and the skills inventory — the placement that was
    // impossible before, since a contribution could only be appended.
    let disposer = registry.register_block(
        RegisteredBlock::system("mine.preamble", orders::SCENE + 50, "read this first")
            .from_origin(BlockOrigin::Plugin("example".into())),
    );

    let blocks = assemble_for_test(
        registry.as_ref(),
        &scene,
        &settings,
        &ctx,
        &[],
        (Some("skills inventory"), None),
    );
    let names: Vec<&str> = blocks.iter().filter_map(|b| b.name.as_deref()).collect();
    let mine = names.iter().position(|n| *n == "mine.preamble").unwrap();
    let skills = names
        .iter()
        .position(|n| *n == names::SKILLS_CATALOG)
        .unwrap();
    assert!(mine < skills, "{names:?}");
    assert!(mine > 0, "the scene still comes first: {names:?}");
    assert_eq!(
        blocks[mine].origin,
        BlockOrigin::Plugin("example".into()),
        "the block must carry whose it is into the assembled prompt"
    );

    disposer.dispose();
    let after = assemble_for_test(
        registry.as_ref(),
        &scene,
        &settings,
        &ctx,
        &[],
        (Some("skills inventory"), None),
    );
    assert!(after
        .iter()
        .all(|b| b.name.as_deref() != Some("mine.preamble")));
}

#[test]
fn a_context_provider_with_nothing_to_say_contributes_no_block() {
    use base::interface::prompt_registry::{InMemoryPromptRegistry, PromptRegistry};

    let registry = InMemoryPromptRegistry::new();
    registry.register_context(
        "mine.sometimes",
        50,
        std::sync::Arc::new(|_: &ScenePromptContext<'_>| None),
    );
    let blocks = assemble_for_test(
        registry.as_ref(),
        &test_scene(),
        &test_settings(),
        &test_ctx(),
        &[],
        (None, None),
    );
    assert!(blocks
        .iter()
        .all(|b| b.name.as_deref() != Some("mine.sometimes")));
}

#[test]
fn a_registered_variable_expands_inside_every_block() {
    use base::interface::prompt_registry::{InMemoryPromptRegistry, PromptRegistry};

    let registry = InMemoryPromptRegistry::new();
    registry.register_variable(
        "project",
        std::sync::Arc::new(|_: &ScenePromptContext<'_>| Some("agix".to_string())),
    );
    registry.register_context(
        "mine.greeting",
        50,
        std::sync::Arc::new(|_: &ScenePromptContext<'_>| {
            Some("welcome to {{project}}".to_string())
        }),
    );

    let blocks = assemble_for_test(
        registry.as_ref(),
        &test_scene(),
        &test_settings(),
        &test_ctx(),
        &[],
        (None, None),
    );
    let mine = blocks
        .iter()
        .find(|b| b.name.as_deref() == Some("mine.greeting"))
        .unwrap();
    assert_eq!(mine.content, "welcome to agix");
}

/// End to end, through the real assembly: a downloaded extension that
/// declared nothing cannot delete a kernel block, and the block is still
/// in the prompt that goes out.
#[test]
fn a_declaration_free_plugin_hook_cannot_delete_a_block_from_the_real_prompt() {
    use base::interface::prompt_assembly::{
        AssemblyCapabilities, AssemblyHook, Authority, PromptAssembly,
    };
    use base::interface::prompt_registry::{InMemoryPromptRegistry, PromptRegistry};

    struct DeletesSkills;
    impl AssemblyHook for DeletesSkills {
        fn on_assemble(
            &self,
            asm: &mut PromptAssembly,
            _ctx: &ScenePromptContext<'_>,
        ) -> Result<(), String> {
            // Tries anyway, and is refused. A hook is allowed to ask.
            let _ = asm.remove(names::SKILLS_CATALOG);
            asm.push(PromptBlock::system("plugin note").named("example.note"));
            Ok(())
        }
    }

    let registry = InMemoryPromptRegistry::new();
    registry.register_assembly_hook(
        std::sync::Arc::new(DeletesSkills),
        Authority::plugin("example", AssemblyCapabilities::default()),
    );

    let blocks = assemble_for_test(
        registry.as_ref(),
        &test_scene(),
        &test_settings(),
        &test_ctx(),
        &[],
        (Some("skills inventory"), None),
    );
    let named: Vec<&str> = blocks.iter().filter_map(|b| b.name.as_deref()).collect();
    assert!(
        named.contains(&names::SKILLS_CATALOG),
        "an undeclared removal must not take effect: {named:?}"
    );
    assert!(
        named.contains(&"example.note"),
        "and the additions it was allowed to make must stand: {named:?}"
    );
}

#[test]
fn a_project_script_hook_may_rewrite_a_kernel_block_in_the_real_prompt() {
    use base::interface::prompt_assembly::{AssemblyHook, Authority, PromptAssembly};
    use base::interface::prompt_registry::{InMemoryPromptRegistry, PromptRegistry};

    struct RewritesSkills;
    impl AssemblyHook for RewritesSkills {
        fn on_assemble(
            &self,
            asm: &mut PromptAssembly,
            _ctx: &ScenePromptContext<'_>,
        ) -> Result<(), String> {
            asm.modify(names::SKILLS_CATALOG, "skills: (curated)".into())
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
    }

    let registry = InMemoryPromptRegistry::new();
    registry.register_assembly_hook(
        std::sync::Arc::new(RewritesSkills),
        Authority::local(BlockOrigin::Script("./.agix/scripts/prompt.js".into())),
    );

    let blocks = assemble_for_test(
        registry.as_ref(),
        &test_scene(),
        &test_settings(),
        &test_ctx(),
        &[],
        (Some("skills inventory"), None),
    );
    let skills = blocks
        .iter()
        .find(|b| b.name.as_deref() == Some(names::SKILLS_CATALOG))
        .unwrap();
    assert_eq!(skills.content, "skills: (curated)");
}
#[test]
fn no_rules_renders_no_block() {
    assert!(render_rules_prompt(&[]).is_none());
}

#[test]
fn the_rules_block_lists_paths_and_descriptions() {
    let text = render_rules_prompt(&[entry(
        "testing",
        "Testing conventions",
        "/p/.agix/rules/testing.md",
    )])
    .expect("expected a rules block");
    assert!(text.contains("Available Rules"));
    assert!(text.contains("testing.md"));
    assert!(text.contains("Testing conventions"));
    assert!(text.contains("Read tool"));
}

fn folded(blocks: Vec<PromptBlock>) -> Vec<PromptBlock> {
    fold_system_blocks(
        blocks,
        MergedSystemPrompt::DEFAULT_SEPARATOR,
        MergedSystemPrompt::MERGED_NAME,
    )
}

#[test]
fn every_system_block_becomes_one_block_in_the_same_order() {
    let out = folded(vec![
        PromptBlock::system("first").named("a"),
        PromptBlock::system("second").named("b"),
        PromptBlock::system("third").named("c"),
    ]);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].content, "first\n\nsecond\n\nthird");
    assert_eq!(
        out[0].name.as_deref(),
        Some(MergedSystemPrompt::MERGED_NAME)
    );
}

/// One breakpoint, at the end of everything that asked to be cached —
/// putting it at the first such block would leave the rest of the merged
/// prefix outside the cache it was asking for.
#[test]
fn the_merged_block_keeps_a_cache_breakpoint_if_any_stage_wanted_one() {
    let out = folded(vec![
        PromptBlock::system_cached("cached"),
        PromptBlock::system("plain"),
    ]);
    assert_eq!(out[0].cache_strategy, Some(CacheStrategy::Ephemeral));

    let out = folded(vec![PromptBlock::system("a"), PromptBlock::system("b")]);
    assert_eq!(out[0].cache_strategy, None);
}

#[test]
fn a_block_that_is_not_a_system_block_is_left_where_it_was() {
    let out = folded(vec![
        PromptBlock::system("s1"),
        PromptBlock::user("u"),
        PromptBlock::system("s2"),
    ]);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].content, "s1\n\ns2");
    assert_eq!(out[1].role, BlockRole::User);
}

#[test]
fn nothing_to_merge_produces_nothing() {
    assert!(folded(Vec::new()).is_empty());
    let out = folded(vec![PromptBlock::user("u")]);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].role, BlockRole::User);
}

fn entry(name: &str, description: &str, path: &str) -> RuleEntry {
    RuleEntry {
        name: name.into(),
        description: description.into(),
        path: std::path::PathBuf::from(path),
    }
}
