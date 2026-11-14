//! 诊断报告：配置层与运行时状态的体检。

use crate::session_pool::*;

impl SessionPool {
    /// Read-only config/wiring diagnostics — see `crate::doctor`.
    pub async fn doctor_report(&self) -> crate::doctor::DoctorReport {
        let settings = self.bindings.pool_settings().await;
        let mut checks: Vec<Arc<dyn base::interface::health::HealthCheck>> = Vec::new();
        checks.push(Arc::new(crate::doctor::ManagedSettingsCheck::new(
            self.bindings.managed_settings_path().to_path_buf(),
            &settings,
        )));
        checks.extend(self.mcp.health_check());
        checks.push(Arc::new(crate::doctor::ProjectTrustCheck::new(
            serde_json::to_value(project_trust_report(
                self.cwd(),
                &settings,
                &self.plugins.agent_types(),
            ))
            .unwrap_or_default(),
        )));
        checks.push(Arc::new(
            crate::doctor_posture::UnattendedNetworkCheck::new(
                settings.clone(),
                self.unattended_scene_ids().await,
            ),
        ));
        checks.extend(self.plugins.health_check());
        checks.extend(
            self.health
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .checks()
                .iter()
                .cloned(),
        );
        let startup_scene = self.bindings.startup_scene().await;
        let mut other_scenes: Vec<(String, PathBuf)> = self
            .scenes
            .served()
            .iter()
            .filter(|scene| scene.id != startup_scene)
            .map(|scene| {
                let dir = self.bindings.scene_root(&scene.id, &startup_scene);
                (scene.id.clone(), dir)
            })
            .collect();
        other_scenes.sort();
        other_scenes.dedup();
        let mut report = crate::doctor::run_doctor(
            self.bindings.paths().as_ref(),
            self.scenes.default_id(),
            &other_scenes,
            settings,
            self.directory.store().is_some(),
            self.plugins.status(),
            &base::interface::health::HealthChecks::from_vec(checks),
        );
        report.runtime = self.runtime_report().await;
        report
    }

    async fn runtime_report(&self) -> crate::doctor::RuntimeReport {
        use crate::doctor::{FailureReport, LogReport, RunningTurnReport, SessionRuntime};
        let mut sessions = self
            .live
            .map_all(|live| SessionRuntime {
                session_id: live.core.id().to_string(),
                phase: live.turn.state(),
                generation: live.engine.generation.get(),
                turn: live.turn.running().map(|turn| RunningTurnReport {
                    turn_id: turn.turn_id.clone(),
                    running_ms: u64::try_from(turn.started.elapsed().as_millis())
                        .unwrap_or(u64::MAX),
                }),
                log: LogReport {
                    behind: live.core.log().is_behind(),
                },
                last_failure: live.core.last_failure().map(|kind| FailureReport { kind }),
            })
            .await;
        sessions.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        crate::doctor::RuntimeReport {
            sessions,
            stopping_engines: self.live.stopping_engines(),
        }
    }

    async fn unattended_scene_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .scenes
            .served()
            .iter()
            .filter(|scene| !scene.interactive)
            .map(|scene| scene.id.clone())
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }
}
