use super::*;
use base::interface::permission::{PermissionFacts, Reach, Subject};
use base::tool::Tool as _;
use serde_json::json;

#[tokio::test]
async fn the_agent_tool_delegates() {
    let tool = agent_tool_with_team(Default::default());
    let input = json!({ "description": "d", "prompt": "p" });
    let ctx = session_state::testing::tool_context_for_test(
        platform::local_providers(),
        std::path::PathBuf::from("/tmp"),
    );
    assert_eq!(
        tool.permission_facts(&input, &ctx).await,
        PermissionFacts::new(Subject::None, Reach::Delegates)
    );
}
