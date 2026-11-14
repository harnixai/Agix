//! What a team run writes down: the `team.json` declaration, the
//! `state.json` snapshot shape and the team id.

use std::collections::HashMap;

use super::TeammateLifecycle;
use crate::tool::TeamStage;

pub(super) fn team_declaration(
    team_id: &str,
    name: &str,
    scene: Option<&str>,
    project_root: String,
    owner_session_id: String,
    permission_mode: base::permission::PermissionMode,
    stages: &[TeamStage],
) -> crate::persist::TeamDeclaration {
    crate::persist::TeamDeclaration {
        schema_version: crate::persist::CURRENT_TEAM_SCHEMA_VERSION,
        team_id: team_id.to_string(),
        name: name.to_string(),
        scene: scene.unwrap_or("unknown").to_string(),
        project_root,
        owner_session_id,
        created_at: crate::persist::iso_now(),
        mode: crate::persist::TeamMode::Batch,
        permission_mode: serde_json::to_value(permission_mode)
            .ok()
            .and_then(|v| v.as_str().map(|s| s.to_string())),
        stages: stages
            .iter()
            .enumerate()
            .map(|(index, s)| crate::persist::StageDeclaration {
                index,
                name: s.name.clone(),
                aggregate: s.aggregate.and_then(|m| {
                    serde_json::to_value(m)
                        .ok()
                        .and_then(|v| v.as_str().map(|s| s.to_string()))
                }),
                members: s
                    .agents
                    .iter()
                    .map(|a| crate::persist::MemberDeclaration {
                        label: a.label.clone(),
                        agent_type: a.agent_type.clone(),
                        prompt: a.prompt.clone(),
                    })
                    .collect(),
            })
            .collect(),
    }
}

pub(super) fn build_state_snapshot(
    current_stage: usize,
    status: crate::persist::TeamStatus,
    lifecycles: &HashMap<String, TeammateLifecycle>,
) -> crate::persist::TeamStateFile {
    crate::persist::TeamStateFile {
        schema_version: crate::persist::CURRENT_STATE_SCHEMA_VERSION,
        updated_at: crate::persist::iso_now(),
        current_stage,
        status,
        members: lifecycles
            .iter()
            .map(|(label, lifecycle)| crate::persist::MemberState {
                label: label.clone(),
                lifecycle: *lifecycle,
                agent_id: None,
                session_id: None,
                idle_since: None,
                last_error: None,
            })
            .collect(),
    }
}

pub(super) fn chrono_id() -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{n:x}")
}
