use super::{ProviderService, SwitchResult};
use crate::app_config::AppType;
use crate::error::AppError;
use crate::provider::{Provider, ProviderMeta};
use crate::store::AppState;
use indexmap::IndexMap;
use serde_json::Value;

const OMP_APP: &str = "omp";

pub(super) fn list(state: &AppState) -> Result<IndexMap<String, Provider>, AppError> {
    let _guard = futures::executor::block_on(state.proxy_service.lock_switch_for_app(OMP_APP));
    match crate::omp_config::read_omp_native_providers() {
        Ok(native) => {
            if let Err(error) = sync_native_locked(state, &native) {
                log::warn!("Failed to sync Oh My Pi providers from models.yml: {error}");
            }
        }
        Err(error) => {
            log::warn!("Failed to read Oh My Pi providers; showing saved catalog: {error}");
        }
    }
    state.db.get_all_providers(OMP_APP)
}

pub(super) fn import_from_live(state: &AppState) -> Result<usize, AppError> {
    let _guard = futures::executor::block_on(state.proxy_service.lock_switch_for_app(OMP_APP));
    let native = crate::omp_config::read_omp_native_providers()?;
    sync_native_locked(state, &native)
}

pub(super) fn add(
    state: &AppState,
    mut provider: Provider,
    add_to_live: bool,
) -> Result<bool, AppError> {
    let app_type = AppType::Omp;
    let _guard =
        futures::executor::block_on(state.proxy_service.lock_switch_for_app(app_type.as_str()));
    strip_unsupported_omp_metadata(&mut provider);
    prepare_provider(&mut provider)?;
    ProviderService::validate_provider_settings(&app_type, &provider)?;
    ProviderService::normalize_usage_script_credential_overrides(&app_type, &mut provider);

    if state
        .db
        .get_provider_by_id(&provider.id, app_type.as_str())?
        .is_some()
    {
        return Err(AppError::InvalidInput(format!(
            "Oh My Pi provider '{}' already exists",
            provider.id
        )));
    }

    if !add_to_live && crate::omp_config::omp_provider_exists(&provider.id)? {
        return Err(AppError::InvalidInput(format!(
            "Oh My Pi provider key '{}' already exists in models.yml",
            provider.id
        )));
    }

    let native_inserted = if add_to_live {
        crate::omp_config::insert_omp_provider(&provider.id, &provider.settings_config)?
    } else {
        false
    };

    if let Err(error) = state.db.save_provider(app_type.as_str(), &provider) {
        if native_inserted {
            if let Err(rollback) = crate::omp_config::remove_omp_provider_if_matches(
                &provider.id,
                &provider.settings_config,
            ) {
                return Err(AppError::Config(format!(
                    "failed to save Oh My Pi provider: {error}; native rollback failed: {rollback}"
                )));
            }
        }
        return Err(error);
    }
    Ok(true)
}

pub(super) fn update(
    state: &AppState,
    original_id: Option<&str>,
    mut provider: Provider,
) -> Result<bool, AppError> {
    let app_type = AppType::Omp;
    let _guard =
        futures::executor::block_on(state.proxy_service.lock_switch_for_app(app_type.as_str()));
    let original_id = original_id.unwrap_or(&provider.id).to_string();
    if original_id != provider.id {
        return Err(AppError::InvalidInput(
            "Oh My Pi provider keys cannot be renamed".to_string(),
        ));
    }

    state
        .db
        .get_provider_by_id(&original_id, app_type.as_str())?
        .ok_or_else(|| {
            AppError::InvalidInput(format!("Oh My Pi provider '{original_id}' not found"))
        })?;
    strip_unsupported_omp_metadata(&mut provider);
    prepare_provider(&mut provider)?;
    ProviderService::validate_provider_settings(&app_type, &provider)?;
    ProviderService::normalize_usage_script_credential_overrides(&app_type, &mut provider);

    let previous_native = crate::omp_config::replace_omp_provider_if_present(
        &original_id,
        &provider.settings_config,
    )?;
    if let Err(error) = state.db.save_provider(app_type.as_str(), &provider) {
        if let Some(previous_native) = previous_native.as_ref() {
            if let Err(rollback) = crate::omp_config::replace_omp_provider(
                &original_id,
                &provider.settings_config,
                previous_native,
            ) {
                return Err(AppError::Config(format!(
                    "failed to save Oh My Pi provider: {error}; native rollback failed: {rollback}"
                )));
            }
        }
        return Err(error);
    }
    Ok(true)
}

pub(super) fn delete(state: &AppState, id: &str) -> Result<(), AppError> {
    let app_type = AppType::Omp;
    let _guard =
        futures::executor::block_on(state.proxy_service.lock_switch_for_app(app_type.as_str()));
    let Some(_) = state.db.get_provider_by_id(id, app_type.as_str())? else {
        return Ok(());
    };
    let removed = crate::omp_config::remove_omp_provider(id)?;

    if let Err(error) = state.db.delete_provider(app_type.as_str(), id) {
        if let Some(removed) = removed.as_ref() {
            if let Err(rollback) = crate::omp_config::restore_omp_provider_if_missing(id, removed) {
                return Err(AppError::Config(format!(
                    "failed to delete Oh My Pi provider: {error}; native rollback failed: {rollback}"
                )));
            }
        }
        return Err(error);
    }
    Ok(())
}

pub(super) fn remove(state: &AppState, id: &str) -> Result<(), AppError> {
    let app_type = AppType::Omp;
    let _guard =
        futures::executor::block_on(state.proxy_service.lock_switch_for_app(app_type.as_str()));
    let provider = state
        .db
        .get_provider_by_id(id, app_type.as_str())?
        .ok_or_else(|| AppError::InvalidInput(format!("Oh My Pi provider '{id}' not found")))?;
    let Some(removed) = crate::omp_config::remove_omp_provider(id)? else {
        return Ok(());
    };
    let mut synced = provider;
    merge_native_config(&mut synced, removed.clone());
    if let Err(error) = state.db.save_provider(app_type.as_str(), &synced) {
        if let Err(rollback) = crate::omp_config::restore_omp_provider_if_missing(id, &removed) {
            return Err(AppError::Config(format!(
                "failed to preserve Oh My Pi provider before removal: {error}; native rollback failed: {rollback}"
            )));
        }
        return Err(error);
    }
    Ok(())
}

pub(super) fn enable(state: &AppState, id: &str) -> Result<SwitchResult, AppError> {
    let app_type = AppType::Omp;
    let _guard =
        futures::executor::block_on(state.proxy_service.lock_switch_for_app(app_type.as_str()));
    let mut provider = state
        .db
        .get_provider_by_id(id, app_type.as_str())?
        .ok_or_else(|| AppError::InvalidInput(format!("Oh My Pi provider '{id}' not found")))?;

    if let Some(native) = crate::omp_config::read_omp_native_provider(id)? {
        merge_native_config(&mut provider, native);
        state.db.save_provider(app_type.as_str(), &provider)?;
        return Ok(SwitchResult::default());
    }

    prepare_provider(&mut provider)?;
    ProviderService::validate_provider_settings(&app_type, &provider)?;
    crate::omp_config::insert_omp_provider(id, &provider.settings_config)?;
    Ok(SwitchResult::default())
}

fn sync_native_locked(
    state: &AppState,
    native: &IndexMap<String, Value>,
) -> Result<usize, AppError> {
    let saved = state.db.get_all_providers(OMP_APP)?;
    let mut changed = 0;

    for (id, config) in native {
        let mut provider = saved.get(id).cloned().unwrap_or_else(|| {
            let mut imported = Provider::with_id(id.clone(), id.clone(), config.clone(), None);
            imported.category = Some("custom".to_string());
            imported.icon = Some("omp".to_string());
            imported
        });
        let is_new = !saved.contains_key(id);
        let previous_name = provider.name.clone();
        let previous_config = provider.settings_config.clone();
        merge_native_config(&mut provider, config.clone());
        if !is_new && provider.name == previous_name && provider.settings_config == previous_config
        {
            continue;
        }

        state.db.save_provider(OMP_APP, &provider)?;
        changed += 1;
    }

    Ok(changed)
}

/// models.yml has no provider-level display name. Keep the database name.
fn merge_native_config(provider: &mut Provider, config: Value) {
    provider.settings_config = config;
}

fn prepare_provider(provider: &mut Provider) -> Result<(), AppError> {
    provider.settings_config =
        crate::omp_config::prepare_omp_provider_config(&provider.settings_config)?;
    Ok(())
}

fn strip_unsupported_omp_metadata(provider: &mut Provider) {
    provider.in_failover_queue = false;
    let Some(meta) = provider.meta.take() else {
        return;
    };
    provider.meta = Some(ProviderMeta {
        usage_script: meta.usage_script,
        is_partner: meta.is_partner,
        partner_promotion_key: meta.partner_promotion_key,
        ..ProviderMeta::default()
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;
    use crate::omp_config::test_support::TestAgentDir;
    use crate::provider::ProviderMeta;
    use serde_json::json;
    use serial_test::serial;
    use std::fs;
    use std::sync::Arc;

    fn state() -> AppState {
        AppState::new(Arc::new(
            Database::memory().expect("create in-memory database"),
        ))
    }

    fn input() -> Provider {
        Provider {
            id: "omp-test".to_string(),
            name: "Display".to_string(),
            settings_config: json!({
                "name": "Display",
                "baseUrl": "https://api.example.com/v1",
                "apiKey": "secret",
                "api": "openai-completions",
                "models": [{
                    "id": "m",
                    "thinkingLevelMap": { "high": "high" }
                }]
            }),
            website_url: None,
            category: Some("custom".to_string()),
            created_at: Some(1),
            sort_index: None,
            notes: None,
            meta: Some(ProviderMeta::default()),
            icon: None,
            icon_color: None,
            in_failover_queue: false,
        }
    }

    #[test]
    #[serial]
    fn add_writes_models_yml_without_provider_level_name() {
        let _dir = TestAgentDir::new();
        let state = state();
        ProviderService::add(&state, AppType::Omp, input()).unwrap();

        let text = fs::read_to_string(
            crate::omp_config::get_omp_agent_dir()
                .unwrap()
                .join("models.yml"),
        )
        .unwrap();
        assert!(!text.contains("\nname:") && !text.contains("name: Display"));
        assert!(
            text.contains("efforts: [high]")
                || (text.contains("efforts:") && text.contains("- high"))
        );
        let saved = state
            .db
            .get_provider_by_id("omp-test", "omp")
            .unwrap()
            .unwrap();
        assert_eq!(saved.name, "Display");
    }
}
