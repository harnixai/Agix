//! `config.*`。

use super::*;
use crate::rpc::methods::config::{
    ConfigGetParams, ConfigGetProviderParams, ConfigListModelsParams, ConfigSetParams,
    ConfigSetProviderParams,
};
use crate::rpc::methods::ConfigTarget;
use crate::session_pool::api::{
    ConfigTier, ModelList, ProviderConfigView, ProviderPatch, ProviderSaved, SettingsChange,
    SettingsReloaded, SettingsWritten,
};

rpc_methods! {
    "config.get" => get(ConfigGetParams) -> ConfigTier;
    "config.set" => set(ConfigSetParams) -> SettingsWritten;
    "config.getProvider" => get_provider(ConfigGetProviderParams) -> ProviderConfigView;
    "config.setProvider" => set_provider(ConfigSetProviderParams) -> ProviderSaved;
    "config.listModels" => list_models(ConfigListModelsParams) -> ModelList;
    "config.reload" => reload(ConfigTarget) -> SettingsReloaded;
}

async fn get(s: &DaemonServer, _: &Call, params: ConfigGetParams) -> Result<ConfigTier, PoolError> {
    s.pool
        .config_tier(
            &params.scene,
            params.project_root.as_deref(),
            params.tier,
            params.include_secrets,
        )
        .await
}

async fn set(
    s: &DaemonServer,
    _: &Call,
    params: ConfigSetParams,
) -> Result<SettingsWritten, PoolError> {
    let (project_root, scene) = s.target_of(params.target);
    s.pool
        .set_settings(
            project_root.as_deref(),
            &scene,
            SettingsChange {
                tier: params.tier,
                changes: params.changes,
                dry_run: params.dry_run,
                if_revision: params.if_revision,
            },
        )
        .await
}

async fn get_provider(
    s: &DaemonServer,
    _: &Call,
    params: ConfigGetProviderParams,
) -> Result<ProviderConfigView, PoolError> {
    let (project_root, scene) = s.target_of(params.target);
    Ok(s.pool
        .get_providers(project_root.as_deref(), &scene, params.include_secrets)
        .await)
}

/// Abandoned when the connection that asked goes away, or the daemon stops.
async fn list_models(
    s: &DaemonServer,
    cx: &Call,
    params: ConfigListModelsParams,
) -> Result<ModelList, PoolError> {
    let (project_root, scene) = s.target_of(params.target);
    let listing = s
        .pool
        .list_models(project_root.as_deref(), &scene, &params.provider);
    tokio::select! {
        listed = listing => listed,
        _ = cx.client.closed() => Err(PoolError::Internal("the connection closed".into())),
        _ = s.shutdown_token.cancelled() => Err(PoolError::Internal("the daemon is stopping".into())),
    }
}

async fn set_provider(
    s: &DaemonServer,
    _: &Call,
    params: ConfigSetProviderParams,
) -> Result<ProviderSaved, PoolError> {
    let (project_root, scene) = s.target_of(params.target);
    s.pool
        .set_provider(
            project_root.as_deref(),
            &scene,
            ProviderPatch {
                tier: params.tier,
                dry_run: params.dry_run,
                provider_id: params.provider_id,
                delete: params.delete,
                config: params.config,
                default_provider: params.default_provider,
                task_models: params.task_models,
            },
        )
        .await
}

/// Re-reads all three settings.json tiers from disk as they are, for a
/// person (or another process) who edited a file by hand.
async fn reload(
    s: &DaemonServer,
    _: &Call,
    target: ConfigTarget,
) -> Result<SettingsReloaded, PoolError> {
    let (project_root, scene) = s.target_of(target);
    Ok(s.pool
        .reload_settings(project_root.as_deref(), &scene)
        .await)
}
