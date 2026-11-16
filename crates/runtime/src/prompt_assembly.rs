//! The two `PromptAssembler`s the engine ships: the default assembly, which
//! places the kernel's stages and registered contributions by order, and
//! `MergedSystemPrompt`, which folds another assembler's system blocks into one.

use base::interface::prompt_assembler::{AssemblyRequest, PromptAssembler};
use base::interface::scene::{ScenePromptContext, SceneSpec};
use base::prompt::{names, BlockOrigin, BlockRole, CacheStrategy, PromptBlock};
use base::rules::RuleEntry;

/// The engine's own assembly, unchanged.
pub struct DefaultAssembler;

impl PromptAssembler for DefaultAssembler {
    fn assemble(&self, request: &AssemblyRequest<'_>) -> Vec<PromptBlock> {
        assemble(request)
    }
}

/// Assembles as `inner` does, then folds the system blocks into one.
///
/// A real configuration rather than a stub. Anthropic allows four
/// `cache_control` breakpoints per request; the number of system blocks is not
/// bounded by anything, because plugins, scripts and config each contribute
/// their own. A deployment that has more contributors than breakpoints wants
/// one cached prefix rather than an arbitrary four of them, and providers that
/// take a single system string are going to concatenate these anyway — better
/// to decide where the joins are than to let an adapter decide.
///
/// The fold is order-preserving and touches nothing else: the blocks arrive
/// from `inner` already sorted, interpolated and hooked, and non-system blocks
/// keep their positions relative to the merged one.
pub struct MergedSystemPrompt {
    inner: Box<dyn PromptAssembler>,
    separator: String,
    /// The name the merged block carries.
    name: String,
}

impl MergedSystemPrompt {
    /// Two newlines between blocks, which is the join a reader of the
    /// resulting prompt would expect between sections.
    pub const DEFAULT_SEPARATOR: &'static str = "\n\n";
    pub const MERGED_NAME: &'static str = "system.merged";

    pub fn new(inner: Box<dyn PromptAssembler>) -> Self {
        Self {
            inner,
            separator: Self::DEFAULT_SEPARATOR.to_string(),
            name: Self::MERGED_NAME.to_string(),
        }
    }
}

impl Default for MergedSystemPrompt {
    fn default() -> Self {
        Self::new(Box::new(DefaultAssembler))
    }
}

impl PromptAssembler for MergedSystemPrompt {
    fn assemble(&self, request: &AssemblyRequest<'_>) -> Vec<PromptBlock> {
        fold_system_blocks(self.inner.assemble(request), &self.separator, &self.name)
    }
}

fn fold_system_blocks(blocks: Vec<PromptBlock>, separator: &str, name: &str) -> Vec<PromptBlock> {
    let mut systems: Vec<String> = Vec::new();
    // The one breakpoint the merged block gets goes at its end, so it covers
    // every stage that asked to be cached rather than the first of them.
    let mut cache: Option<CacheStrategy> = None;
    let mut merged_at: Option<usize> = None;
    let mut out: Vec<PromptBlock> = Vec::new();

    for block in blocks {
        if block.role != BlockRole::System {
            out.push(block);
            continue;
        }
        if let Some(strategy) = block.cache_strategy {
            cache = Some(strategy);
        }
        if merged_at.is_none() {
            merged_at = Some(out.len());
            out.push(PromptBlock::system(String::new()));
        }
        systems.push(block.content);
    }

    if let Some(at) = merged_at {
        out[at] = PromptBlock {
            role: BlockRole::System,
            content: systems.join(separator),
            cache_strategy: cache,
            name: Some(name.to_string()),
            origin: BlockOrigin::Kernel,
        };
    }
    out
}

/// Builds the system prompt, merging in whatever registered a contribution.
///
/// The kernel's own stages are registrations too — they are placed at the
/// orders in [`orders`](base::interface::prompt_registry::orders) and sorted
/// alongside everything else, rather than being concatenated first and having
/// contributions appended after. That is what makes "before the skills
/// inventory" a thing an extension can ask for.
///
/// `prompt_override` still wins over all of it. Overriding the prompt means
/// overriding the prompt; a contribution surviving it would make the setting
/// mean something narrower than it says.
fn assemble(request: &AssemblyRequest<'_>) -> Vec<PromptBlock> {
    let AssemblyRequest {
        registry,
        scene,
        prompt_override,
        prompt_append,
        memory_prompt,
        ctx,
        rules,
        skills_text,
        mcp_instructions,
    } = *request;
    use base::interface::prompt_registry::orders;

    // If override is set, return it as the sole system block.
    if let Some(ov) = prompt_override {
        return vec![PromptBlock::system(ov.to_string())
            .named(names::CONFIG_PROMPT_OVERRIDE)
            .from_origin(BlockOrigin::Config)];
    }

    let mut kernel: Vec<(i32, PromptBlock)> = Vec::new();

    kernel.extend(scene_blocks(scene, ctx));

    // 2. Skills
    if let Some(text) = skills_text {
        if !text.is_empty() {
            kernel.push((
                orders::SKILLS_CATALOG,
                PromptBlock::user(text.to_string()).named(names::SKILLS_CATALOG),
            ));
        }
    }

    // 3. Memory — how to write memories, and the session store's index.
    if let Some(text) = memory_prompt {
        kernel.push((
            orders::MEMORY_SESSION,
            PromptBlock::system(text.to_string()).named(names::MEMORY_SESSION),
        ));
    }

    // 3b. Rules — lightweight discovery index only (filenames + first-line
    // description), not full content. Omitted entirely when no `<project dir>/rules/`
    // files exist anywhere, so sessions that don't use this feature pay zero
    // extra tokens.
    if let Some(text) = render_rules_prompt(rules) {
        kernel.push((orders::RULES, PromptBlock::system(text).named(names::RULES)));
    }

    // 4. MCP instructions
    if let Some(text) = mcp_instructions {
        if !text.is_empty() {
            kernel.push((
                orders::MCP_INSTRUCTIONS,
                PromptBlock::system(text.to_string()).named(names::MCP_INSTRUCTIONS),
            ));
        }
    }

    // 5. User append
    if let Some(append) = prompt_append {
        kernel.push((
            orders::CONFIG_PROMPT_APPEND,
            PromptBlock::system(append.to_string())
                .named(names::CONFIG_PROMPT_APPEND)
                .from_origin(BlockOrigin::Config),
        ));
    }

    // Registered contributions join the kernel's stages and the whole lot
    // sorts together. Stable, so blocks sharing an order keep the sequence
    // they arrived in — which is what preserves a scene's own section order.
    kernel.extend(registered_blocks(registry, ctx));
    kernel.sort_by_key(|(order, _)| *order);

    let variables = registry.variables();
    let assembled: Vec<PromptBlock> = kernel
        .into_iter()
        .map(|(_, mut b)| {
            b.content = base::interface::prompt_registry::interpolate(&b.content, &variables, ctx);
            b
        })
        .collect();

    // Last: the passes that see the whole thing. Interpolation runs before
    // them so a hook reads the text the model will read, not a template.
    base::interface::prompt_assembly::run_assembly_hooks(assembled, &registry.assembly_hooks(), ctx)
}

/// A scene names its own sections; only fill in for one that doesn't, so a
/// scene's own labels are never overwritten here. Sections a scene did not
/// name are numbered after the first: a name is how everything downstream
/// addresses a block, so identical names would send an edit or a removal aimed
/// at one of them to whichever came first.
fn scene_blocks(scene: &SceneSpec, ctx: &ScenePromptContext<'_>) -> Vec<(i32, PromptBlock)> {
    use base::interface::prompt_registry::orders;
    let mut unnamed = 0usize;
    scene
        .system_prompt(ctx)
        .into_iter()
        .map(|b| match &b.name {
            // Prefixed so a scene's label can never collide with a kernel block's.
            Some(section) if !section.starts_with(names::SCENE_PREFIX) => {
                let prefixed = format!("{}{section}", names::SCENE_PREFIX);
                b.named(prefixed)
            }
            Some(_) => b,
            None => {
                unnamed += 1;
                match unnamed {
                    1 => b.named(names::SCENE_SKELETON),
                    n => b.named(format!("{}.{n}", names::SCENE_SKELETON)),
                }
            }
        })
        .map(|b| (orders::SCENE, b))
        .collect()
}

fn registered_blocks(
    registry: &dyn base::interface::prompt_registry::PromptRegistry,
    ctx: &ScenePromptContext<'_>,
) -> Vec<(i32, PromptBlock)> {
    use base::interface::prompt_registry::{PromptContent, RegisteredBlock};
    let mut out = Vec::new();
    for block in registry.blocks() {
        let RegisteredBlock {
            name,
            role,
            order,
            content,
            cache_strategy,
            origin,
        } = block;
        let text = match content {
            PromptContent::Static(s) => Some(s),
            PromptContent::Provider(p) => p(ctx),
        };
        // A provider with nothing to say contributes nothing, rather than an
        // empty block that costs a separator and a cache boundary.
        let Some(text) = text.filter(|t| !t.is_empty()) else {
            continue;
        };
        out.push((
            order,
            PromptBlock {
                role,
                content: text,
                cache_strategy,
                name: Some(name),
                origin,
            },
        ));
    }
    out
}

/// Render the "Available Rules" system prompt block. Returns `None` when no
/// rule documents exist anywhere — keeps token cost at zero for sessions
/// that don't use this feature at all, matching the "宽进严出" philosophy
/// used elsewhere in the config system (unused config = zero runtime cost).
pub fn render_rules_prompt(entries: &[RuleEntry]) -> Option<String> {
    if entries.is_empty() {
        return None;
    }
    let mut lines = vec![
        "## Available Rules".to_string(),
        String::new(),
        "The following rule documents are available. They are NOT \
         auto-loaded — use the Read tool to load one when it's relevant to the \
         current task."
            .to_string(),
        String::new(),
    ];
    for e in entries {
        lines.push(format!("- `{}`: {}", e.path.display(), e.description));
    }
    Some(lines.join("\n"))
}

#[cfg(test)]
mod tests;
