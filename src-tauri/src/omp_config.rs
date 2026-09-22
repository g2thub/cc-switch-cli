//! Thin adapter for Oh My Pi's native models file.
//!
//! OMP owns the active provider/model. CC Switch only manages explicit
//! `providers.<provider-id>` entries in `~/.omp/agent/models.yml` (or
//! `models.yaml`); the legacy `models.json` is read-only and every write is
//! migrated to YAML.

use crate::config::{atomic_write_private, get_home_dir};
use crate::error::AppError;
use indexmap::IndexMap;
use serde_json::{Map, Value};
use serde_yaml::{Mapping, Value as Yaml};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, MutexGuard};

const MAX_BYTES: usize = 1024 * 1024;
// ponytail: one lock for these small native files; split only if contention becomes material.
static MODELS_FILE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
#[cfg(test)]
static TEST_AGENT_DIR: LazyLock<Mutex<Option<PathBuf>>> = LazyLock::new(|| Mutex::new(None));

fn invalid(message: &str) -> AppError {
    AppError::Config(format!("OMP: {message}"))
}

pub(crate) fn get_omp_agent_dir() -> Result<PathBuf, AppError> {
    #[cfg(test)]
    if let Some(path) = TEST_AGENT_DIR
        .lock()
        .expect("lock OMP test directory")
        .clone()
    {
        return resolve_omp_agent_dir(Some(path), None, get_home_dir().join(".omp").join("agent"));
    }

    resolve_omp_agent_dir(
        crate::settings::get_omp_override_dir(),
        std::env::var_os("PI_CODING_AGENT_DIR"),
        get_home_dir().join(".omp").join("agent"),
    )
}

fn resolve_omp_agent_dir(
    settings_override: Option<PathBuf>,
    env_override: Option<std::ffi::OsString>,
    default_path: PathBuf,
) -> Result<PathBuf, AppError> {
    let (path, source) = match settings_override {
        Some(path) => (path, "OMP settings override"),
        None => match env_override {
            Some(value) if !value.is_empty() => (
                crate::settings::resolve_override_path(value.to_string_lossy().as_ref()),
                "PI_CODING_AGENT_DIR",
            ),
            _ => (default_path, "OMP default"),
        },
    };
    if !path.is_absolute() {
        return Err(AppError::InvalidInput(format!(
            "{source} must resolve to an absolute directory: {}",
            path.display()
        )));
    }
    Ok(path)
}

/// The models file CC Switch reads and writes right now: the highest-priority
/// file that exists, otherwise the file a first write would create.
pub(crate) fn get_omp_models_path() -> Result<PathBuf, AppError> {
    let dir = get_omp_agent_dir()?;
    Ok(read_source(&dir)?.path)
}

pub(crate) fn read_omp_native_providers() -> Result<IndexMap<String, Value>, AppError> {
    let _guard = lock_models_file()?;
    let document = read_source(&get_omp_agent_dir()?)?.document()?;
    Ok(providers(&document)?.into_iter().collect())
}

pub(crate) fn read_omp_native_provider(provider_key: &str) -> Result<Option<Value>, AppError> {
    let _guard = lock_models_file()?;
    let document = read_source(&get_omp_agent_dir()?)?.document()?;
    Ok(providers(&document)?.get(provider_key).cloned())
}

pub(crate) fn omp_provider_exists(provider_key: &str) -> Result<bool, AppError> {
    let _guard = lock_models_file()?;
    let document = read_source(&get_omp_agent_dir()?)?.document()?;
    Ok(providers(&document)?.contains_key(provider_key))
}

pub(crate) fn insert_omp_provider(provider_key: &str, config: &Value) -> Result<bool, AppError> {
    validate_provider_node(provider_key, config)?;
    mutate_providers(|providers| match provider_node(providers, provider_key)? {
        Some(current) if current == *config => Ok(false),
        Some(_) => Err(AppError::InvalidInput(format!(
            "Oh My Pi provider key '{provider_key}' already exists in the models file"
        ))),
        None => {
            providers.insert(Yaml::String(provider_key.into()), to_yaml(config)?);
            Ok(true)
        }
    })
}

pub(crate) fn replace_omp_provider(
    provider_key: &str,
    expected: &Value,
    replacement: &Value,
) -> Result<(), AppError> {
    validate_provider_node(provider_key, replacement)?;
    mutate_providers(|providers| {
        let current = provider_node(providers, provider_key)?.ok_or_else(|| {
            AppError::Conflict(format!(
                "Oh My Pi provider '{provider_key}' is no longer present in the models file"
            ))
        })?;
        if current != *expected {
            return Err(AppError::Conflict(format!(
                "Oh My Pi provider '{provider_key}' changed outside CC Switch"
            )));
        }
        if current == *replacement {
            return Ok(());
        }
        providers.insert(Yaml::String(provider_key.into()), to_yaml(replacement)?);
        Ok(())
    })
}

pub(crate) fn replace_omp_provider_if_present(
    provider_key: &str,
    replacement: &Value,
) -> Result<Option<Value>, AppError> {
    validate_provider_node(provider_key, replacement)?;
    mutate_providers(|providers| {
        let Some(current) = provider_node(providers, provider_key)? else {
            return Ok(None);
        };
        if current != *replacement {
            providers.insert(Yaml::String(provider_key.into()), to_yaml(replacement)?);
        }
        Ok(Some(current))
    })
}

pub(crate) fn remove_omp_provider(provider_key: &str) -> Result<Option<Value>, AppError> {
    remove_omp_provider_inner(provider_key, None)
}

pub(crate) fn remove_omp_provider_if_matches(
    provider_key: &str,
    expected: &Value,
) -> Result<bool, AppError> {
    remove_omp_provider_inner(provider_key, Some(expected)).map(|removed| removed.is_some())
}

fn remove_omp_provider_inner(
    provider_key: &str,
    expected: Option<&Value>,
) -> Result<Option<Value>, AppError> {
    mutate_providers(|providers| {
        let Some(current) = provider_node(providers, provider_key)? else {
            return Ok(None);
        };
        if expected.is_some_and(|expected| current != *expected) {
            return Err(AppError::Conflict(format!(
                "Oh My Pi provider '{provider_key}' changed outside CC Switch"
            )));
        }
        providers.remove(Yaml::String(provider_key.into()));
        Ok(Some(current))
    })
}

pub(crate) fn restore_omp_provider_if_missing(
    provider_key: &str,
    config: &Value,
) -> Result<(), AppError> {
    mutate_providers(|providers| {
        match provider_node(providers, provider_key)? {
        Some(current) if current == *config => Ok(()),
        Some(_) => Err(AppError::Conflict(format!(
            "cannot restore Oh My Pi provider '{provider_key}' because another value now owns the key"
        ))),
        None => {
            providers.insert(Yaml::String(provider_key.into()), to_yaml(config)?);
            Ok(())
        }
    }
    })
}

/// Validate the shape CC Switch can persist as one
/// `models.yml.providers.<provider_key>` node.
///
/// Provider ownership is intentionally source-based: every explicit object in
/// the native `providers` mapping is manageable, including providers built into
/// OMP. OMP account credentials live in the agent's own login state and are
/// never read here.
pub(crate) fn validate_provider_node(provider_key: &str, config: &Value) -> Result<(), AppError> {
    if provider_key.trim().is_empty() {
        return Err(AppError::InvalidInput(
            "Oh My Pi provider key cannot be empty".to_string(),
        ));
    }
    let object = config.as_object().ok_or_else(|| {
        AppError::InvalidInput("Oh My Pi provider configuration must be an object".to_string())
    })?;
    // OMP has no provider-level `name`: only models carry one. Writing it would
    // silently create a field OMP never reads.
    if object.contains_key("name") {
        return Err(AppError::InvalidInput(
            "Oh My Pi provider configuration must not carry a provider-level name".to_string(),
        ));
    }
    if serde_json::to_vec(config)
        .map_err(|_| invalid("Cannot encode provider configuration."))?
        .len()
        > MAX_BYTES
    {
        return Err(invalid("Provider configuration exceeds the 1 MiB limit."));
    }
    validate_provider(config)
}

pub(crate) fn provider_base_url(config: &Value) -> Result<String, AppError> {
    let provider = config.as_object().ok_or_else(|| {
        AppError::InvalidInput("Oh My Pi provider configuration must be an object".to_string())
    })?;
    provider
        .get("baseUrl")
        .and_then(Value::as_str)
        .filter(|base_url| !base_url.trim().is_empty())
        .or_else(|| {
            provider
                .get("models")
                .and_then(Value::as_array)
                .and_then(|models| {
                    models.iter().find_map(|model| {
                        model
                            .get("baseUrl")
                            .and_then(Value::as_str)
                            .filter(|base_url| !base_url.trim().is_empty())
                    })
                })
        })
        .map(str::to_string)
        .ok_or_else(|| AppError::InvalidInput("Oh My Pi provider has no request URL".to_string()))
}

fn lock_models_file() -> Result<MutexGuard<'static, ()>, AppError> {
    MODELS_FILE_LOCK
        .lock()
        .map_err(|error| AppError::Config(format!("OMP models file lock is poisoned: {error}")))
}

/// One candidate models file and the bytes that were read from it.
struct Source {
    path: PathBuf,
    bytes: Option<Vec<u8>>,
}

impl Source {
    fn revision(&self) -> String {
        let mut hash = Sha256::new();
        hash.update(self.path.as_os_str().as_encoded_bytes());
        hash.update([0, u8::from(self.bytes.is_some())]);
        if let Some(bytes) = &self.bytes {
            hash.update(bytes);
        }
        format!("{:x}", hash.finalize())
    }

    fn document(&self) -> Result<Yaml, AppError> {
        let Some(bytes) = &self.bytes else {
            return Ok(Yaml::Mapping(Mapping::new()));
        };
        let text = std::str::from_utf8(bytes)
            .map_err(|_| invalid("Models file must contain UTF-8 text."))?;
        let document = if self.path.extension().is_some_and(|ext| ext == "json") {
            let json: Value = json5::from_str(text)
                .map_err(|_| invalid("Models file contains invalid JSON/JSON5."))?;
            serde_yaml::to_value(json)
                .map_err(|_| invalid("Cannot decode the legacy models file."))?
        } else {
            serde_yaml::from_str(text).map_err(|_| invalid("Models file contains invalid YAML."))?
        };
        if !document.is_mapping() {
            return Err(invalid("Models document root must be an object."));
        }
        Ok(document)
    }
}

/// `models.yml` outranks `models.yaml`, which outranks the legacy `models.json`.
fn read_source(dir: &Path) -> Result<Source, AppError> {
    for filename in ["models.yml", "models.yaml", "models.json"] {
        let path = dir.join(filename);
        match fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err(invalid("Cannot inspect the models file.")),
        }
        let file = fs::File::open(&path).map_err(|_| invalid("Cannot read the models file."))?;
        let mut bytes = Vec::new();
        file.take((MAX_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| invalid("Cannot read the models file."))?;
        if bytes.len() > MAX_BYTES {
            return Err(invalid("Models file exceeds the 1 MiB limit."));
        }
        return Ok(Source {
            path,
            bytes: Some(bytes),
        });
    }
    Ok(Source {
        path: dir.join("models.yml"),
        bytes: None,
    })
}

fn ensure_revision(source: &Source, expected: &str) -> Result<(), AppError> {
    if source.revision() != expected {
        return Err(AppError::Conflict(
            "OMP models changed on disk. Reload before saving.".into(),
        ));
    }
    Ok(())
}

/// Read-modify-write the `providers` mapping of the models document.
///
/// The whole document is preserved: every other root key and every provider
/// outside the caller's target node survives the write. The file is replaced
/// atomically with private permissions only after the selected source is
/// re-read and confirmed unchanged, so an external edit (or a newly appeared
/// higher-priority file) never gets clobbered.
fn mutate_providers<T>(
    mutate: impl FnOnce(&mut Mapping) -> Result<T, AppError>,
) -> Result<T, AppError> {
    let dir = get_omp_agent_dir()?;
    let _guard = lock_models_file()?;
    let source = read_source(&dir)?;
    let mut document = source.document()?;
    // Refuse to rewrite a document holding a node JSON cannot round-trip:
    // reserializing it would coerce or drop whatever CC Switch cannot manage.
    providers(&document)?;
    let result = mutate(providers_mut(&mut document)?)?;
    let bytes = serde_yaml::to_string(&document)
        .map_err(|_| invalid("Cannot encode the models document."))?
        .into_bytes();
    if bytes.len() > MAX_BYTES {
        return Err(invalid("Updated models file exceeds the 1 MiB limit."));
    }
    ensure_revision(&read_source(&dir)?, &source.revision())?;
    // The legacy JSON file is never rewritten: it is superseded by models.yml.
    let path = if source.path.extension().is_some_and(|ext| ext == "json") {
        dir.join("models.yml")
    } else {
        source.path
    };
    atomic_write_private(&path, &bytes).map_err(|_| {
        invalid("Cannot atomically write the models file. The original was not replaced.")
    })?;
    Ok(result)
}

fn to_yaml(config: &Value) -> Result<Yaml, AppError> {
    serde_yaml::to_value(config).map_err(|_| invalid("Cannot encode provider configuration."))
}

/// Read one provider node as JSON, refusing YAML that JSON cannot round-trip
/// (tags, non-string keys, non-finite numbers) instead of coercing it.
///
/// Only schema field names are ever reported; values may be secrets.
fn json_node(config: &Yaml) -> Result<Value, AppError> {
    if !config.is_mapping() {
        return Err(invalid("Each provider must be an object."));
    }
    let json = serde_json::to_value(config)
        .map_err(|_| invalid("Provider contains values that cannot be edited as JSON."))?;
    if &to_yaml(&json)? != config {
        return Err(invalid(
            "Provider contains YAML values that cannot be edited losslessly as JSON.",
        ));
    }
    Ok(json)
}

fn provider_node(providers: &Mapping, provider_key: &str) -> Result<Option<Value>, AppError> {
    providers.get(provider_key).map(json_node).transpose()
}

fn providers(document: &Yaml) -> Result<Map<String, Value>, AppError> {
    let root = document
        .as_mapping()
        .ok_or_else(|| invalid("Models document root must be an object."))?;
    let Some(value) = root.get("providers") else {
        return Ok(Map::new());
    };
    let mapping = value
        .as_mapping()
        .ok_or_else(|| invalid("The providers field must be an object."))?;
    mapping
        .iter()
        .map(|(key, config)| {
            let key = key
                .as_str()
                .filter(|key| !key.trim().is_empty())
                .ok_or_else(|| invalid("Provider keys must be nonempty strings."))?;
            Ok((key.to_string(), json_node(config)?))
        })
        .collect()
}

fn providers_mut(document: &mut Yaml) -> Result<&mut Mapping, AppError> {
    let root = document
        .as_mapping_mut()
        .ok_or_else(|| invalid("Models document root must be an object."))?;
    root.entry(Yaml::String("providers".into()))
        .or_insert_with(|| Yaml::Mapping(Mapping::new()))
        .as_mapping_mut()
        .ok_or_else(|| invalid("The providers field must be an object."))
}

const APIS: &[&str] = &[
    "openai-completions",
    "openai-responses",
    "openai-codex-responses",
    "azure-openai-responses",
    "anthropic-messages",
    "bedrock-converse-stream",
    "google-generative-ai",
    "google-gemini-cli",
    "google-vertex",
];

fn check_fields(
    object: &Map<String, Value>,
    fields: &[&str],
    valid: fn(&Value) -> bool,
) -> Result<(), AppError> {
    for field in fields {
        if object.get(*field).is_some_and(|value| !valid(value)) {
            // Only schema field names are reported; values and user-defined keys may be secrets.
            return Err(invalid(&format!("Invalid type or value for {field}.")));
        }
    }
    Ok(())
}

fn validate_api(object: &Map<String, Value>) -> Result<(), AppError> {
    check_fields(object, &["api"], |value| {
        value.as_str().is_some_and(|api| APIS.contains(&api))
    })
}

fn positive(value: &Value) -> bool {
    value
        .as_f64()
        .is_some_and(|value| value.is_finite() && value > 0.0)
}

fn nonempty(object: &Map<String, Value>, key: &str) -> bool {
    object
        .get(key)
        .and_then(Value::as_str)
        .is_some_and(|value| !value.trim().is_empty())
}

fn validate_common(object: &Map<String, Value>) -> Result<(), AppError> {
    validate_api(object)?;
    check_fields(object, &["headers"], |value| {
        value
            .as_object()
            .is_some_and(|values| values.values().all(Value::is_string))
    })?;
    check_fields(object, &["compat", "remoteCompaction"], Value::is_object)?;
    if let Some(remote) = object.get("remoteCompaction").and_then(Value::as_object) {
        validate_api(remote)?;
        check_fields(
            remote,
            &["enabled", "v2StreamingEnabled"],
            Value::is_boolean,
        )?;
        check_fields(
            remote,
            &["endpoint", "model", "v2Endpoint", "streamingEndpoint"],
            Value::is_string,
        )?;
    }
    Ok(())
}

fn validate_model(value: &Value) -> Result<(), AppError> {
    let model = value
        .as_object()
        .ok_or_else(|| invalid("Each model or model override must be an object."))?;
    validate_common(model)?;
    check_fields(
        model,
        &[
            "id",
            "name",
            "baseUrl",
            "imageInputDecoder",
            "tokenizer",
            "contextPromotionTarget",
            "compactionModel",
        ],
        Value::is_string,
    )?;
    check_fields(
        model,
        &[
            "reasoning",
            "supportsTools",
            "omitMaxOutputTokens",
            "preferWebsockets",
        ],
        Value::is_boolean,
    )?;
    check_fields(model, &["contextWindow", "maxTokens"], positive)?;
    check_fields(model, &["thinking", "cost"], Value::is_object)?;
    check_fields(model, &["premiumMultiplier"], Value::is_number)?;
    check_fields(model, &["input"], |value| {
        value.as_array().is_some_and(|values| {
            values
                .iter()
                .all(|value| matches!(value.as_str(), Some("text" | "image")))
        })
    })?;
    if let Some(cost) = model.get("cost").and_then(Value::as_object) {
        check_fields(
            cost,
            &["input", "output", "cacheRead", "cacheWrite"],
            Value::is_number,
        )?;
    }
    Ok(())
}

fn validate_provider(value: &Value) -> Result<(), AppError> {
    let provider = value
        .as_object()
        .ok_or_else(|| invalid("Provider configuration must be an object."))?;
    validate_common(provider)?;
    check_fields(provider, &["requestMetadata"], |value| {
        value
            .as_object()
            .is_some_and(|values| values.values().all(Value::is_string))
    })?;
    check_fields(
        provider,
        &[
            "baseUrl",
            "apiKey",
            "guardrailIdentifier",
            "guardrailVersion",
            "guardrailTrace",
            "transport",
        ],
        Value::is_string,
    )?;
    check_fields(
        provider,
        &["authHeader", "disableStrictTools"],
        Value::is_boolean,
    )?;
    check_fields(provider, &["auth"], |value| {
        matches!(value.as_str(), Some("apiKey" | "none" | "oauth"))
    })?;
    check_fields(provider, &["discovery", "modelOverrides"], Value::is_object)?;
    check_fields(provider, &["models"], Value::is_array)?;
    if let Some(discovery) = provider.get("discovery").and_then(Value::as_object) {
        let kind = discovery.get("type").and_then(Value::as_str);
        if !matches!(
            kind,
            Some("ollama" | "llama.cpp" | "lm-studio" | "openai-models-list" | "proxy" | "litellm")
        ) {
            return Err(invalid("Invalid discovery type."));
        }
        check_fields(discovery, &["timeoutMs"], positive)?;
        check_fields(discovery, &["injectV1"], Value::is_boolean)?;
        if kind != Some("proxy") && !provider.contains_key("api") {
            return Err(invalid(
                "Discovery requires a provider api unless its type is proxy.",
            ));
        }
    }
    if let Some(overrides) = provider.get("modelOverrides").and_then(Value::as_object) {
        for (id, value) in overrides {
            if id.trim().is_empty() {
                return Err(invalid("Model override IDs must not be empty."));
            }
            validate_model(value)?;
        }
    }
    let models = provider.get("models").and_then(Value::as_array);
    if let Some(models) = models.filter(|models| !models.is_empty()) {
        if !nonempty(provider, "baseUrl") {
            return Err(invalid("Custom models require a provider baseUrl."));
        }
        if !matches!(
            provider.get("auth").and_then(Value::as_str),
            Some("none" | "oauth")
        ) && !nonempty(provider, "apiKey")
        {
            return Err(invalid(
                "Custom models require a provider apiKey unless auth is none or oauth.",
            ));
        }
        let mut ids = HashSet::new();
        for model in models {
            validate_model(model)?;
            let model = model.as_object().expect("validated model");
            let id = model
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.trim().is_empty())
                .ok_or_else(|| invalid("Each custom model requires a nonempty id."))?;
            if !ids.insert(id) {
                return Err(invalid("Model IDs must be unique within a provider."));
            }
            if !provider.contains_key("api") && !model.contains_key("api") {
                return Err(invalid(
                    "Each custom model requires an api at provider or model level.",
                ));
            }
        }
    } else {
        let meaningful = nonempty(provider, "baseUrl")
            || nonempty(provider, "apiKey")
            || nonempty(provider, "guardrailIdentifier")
            || provider.get("auth").and_then(Value::as_str) == Some("none")
            || provider.get("disableStrictTools").and_then(Value::as_bool) == Some(true)
            || [
                "headers",
                "compat",
                "discovery",
                "remoteCompaction",
                "requestMetadata",
            ]
            .iter()
            .any(|key| provider.contains_key(*key))
            || provider
                .get("modelOverrides")
                .and_then(Value::as_object)
                .is_some_and(|values| !values.is_empty());
        if !meaningful {
            return Err(invalid(
                "An override-only provider must define an override or discovery configuration.",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::TEST_AGENT_DIR;
    use std::path::PathBuf;

    /// Points [`super::get_omp_agent_dir`] at a directory for the duration of a
    /// test. Restores the previous override on drop.
    pub(crate) struct TestAgentDir {
        _dir: Option<tempfile::TempDir>,
        previous: Option<PathBuf>,
    }

    impl TestAgentDir {
        pub(crate) fn new() -> Self {
            let dir = tempfile::tempdir().expect("create Oh My Pi test directory");
            Self::set(dir.path().to_path_buf(), Some(dir))
        }

        fn set(agent_dir: PathBuf, dir: Option<tempfile::TempDir>) -> Self {
            let previous = TEST_AGENT_DIR
                .lock()
                .expect("lock Oh My Pi test directory")
                .replace(agent_dir);
            Self {
                _dir: dir,
                previous,
            }
        }
    }

    impl Drop for TestAgentDir {
        fn drop(&mut self) {
            *TEST_AGENT_DIR.lock().expect("lock Oh My Pi test directory") = self.previous.take();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::omp_config::test_support::TestAgentDir;
    use serde_json::json;
    use serial_test::serial;

    fn agent_dir() -> PathBuf {
        get_omp_agent_dir().expect("agent dir")
    }

    #[test]
    #[serial]
    fn path_precedence_and_legacy_migration_are_explicit() {
        let _agent = TestAgentDir::new();
        let dir = agent_dir();
        let legacy = dir.join("models.json");
        let original = br#"{extra: {preserved: true}, providers: {old: {baseUrl: 'https://old'}}}"#;
        fs::write(&legacy, original).unwrap();
        assert!(get_omp_models_path().unwrap().ends_with("models.json"));
        assert!(!dir.join("models.yml").exists());

        assert!(insert_omp_provider("new", &json!({"auth":"none"})).unwrap());
        assert!(get_omp_models_path().unwrap().ends_with("models.yml"));
        assert_eq!(fs::read(&legacy).unwrap(), original);
        let providers = read_omp_native_providers().unwrap();
        assert!(providers.contains_key("old"));
        assert!(providers.contains_key("new"));
        let document =
            serde_yaml::from_str::<Yaml>(&fs::read_to_string(dir.join("models.yml")).unwrap())
                .unwrap();
        assert_eq!(document["extra"]["preserved"].as_bool(), Some(true));

        fs::write(
            dir.join("models.yaml"),
            "providers:\n  yaml: {auth: none}\n",
        )
        .unwrap();
        // models.yml still outranks the file that just appeared.
        let preferred = read_omp_native_providers().unwrap();
        assert!(preferred.contains_key("new"));
        assert!(!preferred.contains_key("yaml"));
        fs::remove_file(dir.join("models.yml")).unwrap();
        assert!(read_omp_native_providers().unwrap().contains_key("yaml"));
        assert!(remove_omp_provider("yaml").unwrap().is_some());
        // The legacy JSON never comes back as the write target while a YAML file exists.
        assert!(!dir.join("models.yml").exists());
        assert!(dir.join("models.yaml").exists());
    }

    /// Every write re-reads the selected file and compares it against the bytes
    /// it based its edit on, so an external edit — or a higher-priority file
    /// appearing mid-write — aborts instead of being clobbered.
    #[test]
    #[serial]
    fn source_revision_tracks_the_selected_file_and_its_bytes() {
        let _agent = TestAgentDir::new();
        let dir = agent_dir();
        fs::write(dir.join("models.json"), "providers: {old: {auth: none}}").unwrap();
        let legacy = read_source(&dir).unwrap();
        let legacy_revision = legacy.revision();
        assert!(ensure_revision(&legacy, &legacy_revision).is_ok());

        let yml = dir.join("models.yml");
        fs::write(&yml, "providers: {old: {auth: none}}\n").unwrap();
        assert!(matches!(
            ensure_revision(&read_source(&dir).unwrap(), &legacy_revision),
            Err(AppError::Conflict(_))
        ));

        let selected = read_source(&dir).unwrap();
        let selected_revision = selected.revision();
        fs::write(&yml, "providers: {old: {auth: none}}\n# external edit\n").unwrap();
        assert!(matches!(
            ensure_revision(&read_source(&dir).unwrap(), &selected_revision),
            Err(AppError::Conflict(_))
        ));
    }

    #[test]
    #[serial]
    fn crud_preserves_advanced_fields_other_providers_and_secret_strings() {
        let _agent = TestAgentDir::new();
        let path = agent_dir().join("models.yml");
        fs::write(&path, "extra: !custom {node: retained}\nproviders:\n  other:\n    baseUrl: https://other\n    compat: {extraBody: {nested: [1, true, null]}}\n").unwrap();
        let original = read_source(&agent_dir()).unwrap().document().unwrap();
        let key = "true: #\nprovider";
        let config = json!({
            "baseUrl": "https://example/v1", "apiKey": "!op read secret:#\nnext", "api": "openai-responses",
            "headers": {"X-Key": "false"}, "future": {"unknown": [true, 12, null]},
            "discovery": {"type": "proxy", "future": {"key": "yes"}},
            "remoteCompaction": {"enabled": true, "model": "compact"},
            "modelOverrides": {"built-in": {"thinking": {"efforts": ["high"]}, "compat": {"extraBody": {"a": "b"}}}},
            "models": [{"id": "custom", "contextWindow": 2048, "maxTokens": 1024, "future": {"keep": true}}]
        });
        assert!(insert_omp_provider(key, &config).unwrap());
        assert_eq!(read_omp_native_provider(key).unwrap().unwrap(), config);
        // Re-inserting the same node is a no-op, a different one is a conflict.
        assert!(!insert_omp_provider(key, &config).unwrap());
        let mut other = config.clone();
        other["apiKey"] = json!("different");
        assert!(insert_omp_provider(key, &other).is_err());
        assert!(replace_omp_provider("missing", &config, &config).is_err());
        assert!(!remove_omp_provider_if_matches("missing", &config).unwrap());

        let mut edited = config.clone();
        edited["models"][0]["maxTokens"] = json!(512);
        assert_eq!(
            replace_omp_provider_if_present(key, &edited).unwrap(),
            Some(config.clone())
        );
        assert_eq!(read_omp_native_provider(key).unwrap().unwrap(), edited);
        let after = read_source(&agent_dir()).unwrap().document().unwrap();
        assert_eq!(after["extra"], original["extra"]);
        assert_eq!(after["providers"]["other"], original["providers"]["other"]);

        assert!(remove_omp_provider_if_matches(key, &edited).unwrap());
        assert!(!omp_provider_exists(key).unwrap());
        assert_eq!(
            read_source(&agent_dir()).unwrap().document().unwrap(),
            original
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    #[serial]
    fn provider_level_name_is_rejected_so_it_can_never_be_written() {
        let _agent = TestAgentDir::new();
        let config = json!({"name": "Display", "auth": "none"});
        assert!(validate_provider_node("new", &config).is_err());
        assert!(insert_omp_provider("new", &config).is_err());
        assert!(!omp_provider_exists("new").unwrap());
    }

    #[test]
    #[serial]
    fn malformed_documents_and_invalid_configs_never_replace_original() {
        let _agent = TestAgentDir::new();
        let path = agent_dir().join("models.yml");
        for malformed in [
            "secret: [",
            "[]",
            "providers: []",
            "providers: {bad: false}",
            "providers: {bad: {apiKey: !tag secret}}",
        ] {
            fs::write(&path, malformed).unwrap();
            assert!(read_omp_native_providers().is_err());
            let error = insert_omp_provider("new", &json!({"auth":"none"})).unwrap_err();
            assert!(!error.to_string().contains("secret"));
            assert_eq!(fs::read_to_string(&path).unwrap(), malformed);
        }
        fs::write(&path, "providers: {old: {auth: none}}\n").unwrap();
        let before = fs::read(&path).unwrap();
        for config in [
            json!([]),
            json!({}),
            json!({"baseUrl": false}),
            json!({"auth":"none", "api":"unsupported"}),
            json!({"baseUrl":"https://example", "api":"openai-responses", "models":[{"id":"a"}]}),
            json!({"auth":"none", "api":"openai-responses", "models":[{"id":"a"}]}),
            json!({"auth":"none", "baseUrl":"https://example", "models":[{"id":"a"}]}),
            json!({"auth":"none", "baseUrl":"https://example", "api":"openai-responses", "models":[{"id":"a"},{"id":"a"}]}),
            json!({"auth":"none", "baseUrl":"https://example", "api":"openai-responses", "models":[{"id":" "}]}),
            json!({"modelOverrides":{"a":{"contextWindow":0}}}),
            json!({"modelOverrides":{"a":{"maxTokens":-1}}}),
            json!({"headers":{"X-Secret":true}}),
            json!({"discovery":{"type":"ollama"}}),
        ] {
            assert!(insert_omp_provider("new", &config).is_err());
            assert_eq!(fs::read(&path).unwrap(), before);
        }
        assert!(insert_omp_provider(
            "new",
            &json!({"auth":"none", "baseUrl":"https://example", "models":[{"id":"a", "api":"openai-responses"}]}),
        )
        .unwrap());
        assert!(insert_omp_provider(
            "oauth",
            &json!({"auth":"oauth", "baseUrl":"https://example", "api":"openai-responses", "models":[{"id":"oauth-model"}]}),
        )
        .unwrap());
    }

    #[test]
    #[serial]
    fn oversize_input_and_output_leave_file_untouched() {
        let _agent = TestAgentDir::new();
        let path = agent_dir().join("models.yml");
        fs::write(&path, vec![b' '; MAX_BYTES + 1]).unwrap();
        assert!(read_omp_native_providers().is_err());
        fs::write(&path, "providers: {old: {auth: none}}\n").unwrap();
        let before = fs::read(&path).unwrap();
        assert!(insert_omp_provider("new", &json!({"apiKey":"x".repeat(MAX_BYTES)})).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
        // Each provider is under the limit, but their combined native document is not.
        insert_omp_provider("large", &json!({"apiKey":"x".repeat(MAX_BYTES / 2)})).unwrap();
        let before = fs::read(&path).unwrap();
        assert!(
            insert_omp_provider("another", &json!({"apiKey":"x".repeat(MAX_BYTES / 2)})).is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn agent_dir_override_requires_an_absolute_path() {
        let absolute = get_home_dir().join("omp-agent");
        assert_eq!(
            resolve_omp_agent_dir(Some(absolute.clone()), None, PathBuf::from("/ignored")).unwrap(),
            absolute
        );
        assert_eq!(
            resolve_omp_agent_dir(
                None,
                Some(std::ffi::OsString::from("")),
                PathBuf::from("/default")
            )
            .unwrap(),
            PathBuf::from("/default")
        );
        assert!(resolve_omp_agent_dir(
            None,
            Some(std::ffi::OsString::from("relative/agent")),
            PathBuf::from("/default")
        )
        .is_err());
    }
}
