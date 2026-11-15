//! `ENGINE_TOOL_NAMES` against what a built session registers by itself: a host
//! connection's tool may not take any of those names.

use super::*;
use crate::host_tools::ENGINE_TOOL_NAMES;

#[tokio::test]
async fn every_tool_the_engine_registers_by_itself_is_reserved() {
    let (agent, _rx, _tx) = Builder::new()
        .scene(Arc::new(bare_scene("engine-tools")))
        .model(Arc::new(DummyModel))
        .settings(Arc::new(test_settings()))
        .tools(Arc::new(InMemoryToolRegistry::new()))
        .skip_warmup(true)
        .build()
        .expect("build should succeed");

    let names = agent.tooling.tools().names();
    assert!(!names.is_empty());
    let unreserved: Vec<&String> = names
        .iter()
        .filter(|n| !ENGINE_TOOL_NAMES.contains(&n.as_str()))
        .collect();
    assert!(
        unreserved.is_empty(),
        "not in ENGINE_TOOL_NAMES: {unreserved:?}"
    );
}

#[test]
fn engine_tool_names_are_unique_and_sorted() {
    let mut sorted = ENGINE_TOOL_NAMES.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted, ENGINE_TOOL_NAMES);
}
