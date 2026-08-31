use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use owo_colors::OwoColorize;

use crate::model::ProviderKind;
use crate::output::{OutputFormat, print_json, use_color};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum AuthConfig {
    Basic {
        username: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        token: String,
    },
    Bearer {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        token: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileConfig {
    pub provider: ProviderKind,
    pub base_url: String,
    pub api_path: String,
    pub auth: AuthConfig,
    #[serde(default)]
    pub credential_store: Option<String>,
    #[serde(default)]
    pub cloud_id: Option<String>,
    #[serde(default)]
    pub token_kind: Option<String>,
    #[serde(default)]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    pub active_profile: Option<String>,
    #[serde(default)]
    pub profiles: BTreeMap<String, ProfileConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedProfile {
    pub name: String,
    pub provider: ProviderKind,
    pub base_url: String,
    pub api_path: String,
    pub auth: AuthConfig,
    pub credential_store: String,
    pub cloud_id: Option<String>,
    pub token_kind: String,
    pub expires_at: Option<String>,
    pub read_only: bool,
}

#[derive(Debug, Clone, Default)]
pub struct LoginInput {
    pub profile: Option<String>,
    pub provider: Option<ProviderKind>,
    pub domain: Option<String>,
    pub api_path: Option<String>,
    pub auth_type: Option<String>,
    pub username: Option<String>,
    pub token: Option<String>,
    pub read_only: Option<bool>,
    pub non_interactive: bool,
    pub insecure_storage: bool,
    pub cloud_id: Option<String>,
    pub token_kind: Option<String>,
    pub expires_at: Option<String>,
}

impl AppConfig {
    pub fn config_dir() -> Result<PathBuf> {
        let dirs = ProjectDirs::from("dev", "ruben", "confluence-cli")
            .ok_or_else(|| anyhow!("failed to determine configuration directory"))?;
        Ok(dirs.config_dir().to_path_buf())
    }

    pub fn config_path() -> Result<PathBuf> {
        Ok(Self::config_dir()?.join("config.json"))
    }

    pub fn load() -> Result<Self> {
        let path = Self::config_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read config file {}", path.display()))?;
        let config = serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse config file {}", path.display()))?;
        Ok(config)
    }

    pub fn save(&self) -> Result<()> {
        let dir = Self::config_dir()?;
        fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create config directory {}", dir.display()))?;
        let path = Self::config_path()?;
        let raw = serde_json::to_vec_pretty(self)?;
        let mut temp = tempfile::Builder::new()
            .prefix(".config-")
            .suffix(".json.tmp")
            .tempfile_in(&dir)
            .with_context(|| format!("failed to create temporary config in {}", dir.display()))?;
        temp.write_all(&raw)
            .with_context(|| format!("failed to write temporary config for {}", path.display()))?;
        temp.flush()
            .with_context(|| format!("failed to flush temporary config for {}", path.display()))?;
        set_private_permissions(temp.path())?;
        temp.persist(&path)
            .map_err(|error| error.error)
            .with_context(|| format!("failed to replace {}", path.display()))?;
        Ok(())
    }

    pub fn resolved_profile(&self, profile_override: Option<&str>) -> Result<ResolvedProfile> {
        if let Some(name) = profile_override
            && !self.profiles.contains_key(name)
        {
            return Err(crate::output::typed_error_with_hint(
                crate::output::ErrorKind::NotFound,
                format!("profile `{name}` not found"),
                "run `confluence profile list` to see configured profiles",
            ));
        }
        let env_profile = env::var("CONFLUENCE_PROFILE").ok();
        let selected_name = profile_override
            .map(ToOwned::to_owned)
            .or(env_profile)
            .or_else(|| self.active_profile.clone())
            .or_else(|| self.profiles.keys().next().cloned());

        let stored = if let Some(name) = selected_name.clone() {
            self.profiles
                .get(&name)
                .cloned()
                .map(|profile| (name, profile))
        } else {
            None
        };

        let env_override = EnvOverride::from_env()?;

        let resolved = match (stored, env_override) {
            (Some((name, stored)), Some(override_cfg)) => {
                let uses_env_token = override_cfg.token.is_some();
                let credential_store = if uses_env_token {
                    "environment".to_string()
                } else {
                    credential_source(&stored)
                };
                let stored = if uses_env_token {
                    stored
                } else {
                    hydrate_stored_credential(&name, stored)?
                };
                let mut resolved = override_cfg.merge_with(name, Some(stored));
                resolved.credential_store = credential_store;
                resolved
            }
            (Some((name, stored)), None) => ResolvedProfile::from_stored(name, stored)?,
            (None, Some(override_cfg)) => {
                let mut resolved = override_cfg
                    .merge_with(selected_name.unwrap_or_else(|| "env".to_string()), None);
                resolved.credential_store = "environment".to_string();
                resolved
            }
            (None, None) => {
                return Err(crate::output::typed_error_with_hint(
                    crate::output::ErrorKind::Auth,
                    "no active profile configured",
                    "run `confluence auth login` or set CONFLUENCE_* environment variables",
                ));
            }
        };
        if !matches!(resolved.token_kind.as_str(), "classic" | "scoped") {
            return Err(crate::output::typed_error(
                crate::output::ErrorKind::InvalidInput,
                format!(
                    "unsupported token kind `{}`; expected classic or scoped",
                    resolved.token_kind
                ),
            ));
        }
        if resolved.token_kind == "scoped"
            && resolved.cloud_id.as_deref().unwrap_or_default().is_empty()
        {
            return Err(crate::output::typed_error_with_hint(
                crate::output::ErrorKind::Auth,
                "scoped Cloud token requires a Cloud ID",
                "run `confluence auth login`",
            ));
        }
        if let Some(value) = resolved.expires_at.as_deref() {
            chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
                .with_context(|| format!("invalid expires_at `{value}`; expected YYYY-MM-DD"))?;
        }
        Ok(resolved)
    }

    pub fn upsert_profile(&mut self, name: String, profile: ProfileConfig) {
        self.profiles.insert(name.clone(), profile);
        self.active_profile = Some(name);
    }

    pub fn remove_profile(&mut self, name: &str) -> Result<()> {
        if self.profiles.remove(name).is_none() {
            return Err(crate::output::typed_error(
                crate::output::ErrorKind::NotFound,
                format!("profile `{name}` not found"),
            ));
        }
        if self.active_profile.as_deref() == Some(name) {
            self.active_profile = self.profiles.keys().next().cloned();
        }
        Ok(())
    }

    pub fn set_active_profile(&mut self, name: &str) -> Result<()> {
        if !self.profiles.contains_key(name) {
            return Err(crate::output::typed_error(
                crate::output::ErrorKind::NotFound,
                format!("profile `{name}` not found"),
            ));
        }
        self.active_profile = Some(name.to_string());
        Ok(())
    }
}

impl ResolvedProfile {
    fn from_stored(name: String, profile: ProfileConfig) -> Result<Self> {
        let credential_store = credential_source(&profile);
        let profile = hydrate_stored_credential(&name, profile)?;
        Ok(Self {
            name,
            provider: profile.provider,
            base_url: profile.base_url,
            api_path: profile.api_path,
            auth: profile.auth,
            credential_store,
            cloud_id: profile.cloud_id,
            token_kind: profile.token_kind.unwrap_or_else(|| "classic".to_string()),
            expires_at: profile.expires_at,
            read_only: profile.read_only,
        })
    }

    pub fn redact(&self) -> Self {
        let auth = match &self.auth {
            AuthConfig::Basic { username, .. } => AuthConfig::Basic {
                username: username.clone(),
                token: "***".to_string(),
            },
            AuthConfig::Bearer { .. } => AuthConfig::Bearer {
                token: "***".to_string(),
            },
        };
        Self {
            name: self.name.clone(),
            provider: self.provider,
            base_url: self.base_url.clone(),
            api_path: self.api_path.clone(),
            auth,
            credential_store: self.credential_store.clone(),
            cloud_id: self.cloud_id.clone(),
            token_kind: self.token_kind.clone(),
            expires_at: self.expires_at.clone(),
            read_only: self.read_only,
        }
    }

    pub fn web_path_prefix(&self) -> String {
        let trimmed = self.api_path.trim();
        if let Some(prefix) = trimmed.strip_suffix("/rest/api") {
            prefix.to_string()
        } else if let Some(prefix) = trimmed.strip_suffix("rest/api") {
            prefix.trim_end_matches('/').to_string()
        } else {
            String::new()
        }
    }
}

fn auth_token(auth: &AuthConfig) -> &str {
    match auth {
        AuthConfig::Basic { token, .. } | AuthConfig::Bearer { token } => token,
    }
}

fn set_auth_token(auth: &mut AuthConfig, value: String) {
    match auth {
        AuthConfig::Basic { token, .. } | AuthConfig::Bearer { token } => *token = value,
    }
}

fn credential_source(profile: &ProfileConfig) -> String {
    match profile.credential_store.as_deref() {
        Some("keyring") => "os-keychain",
        Some("file") => "config-file",
        Some(other) => other,
        None if !auth_token(&profile.auth).is_empty() => "legacy-config",
        None => "none",
    }
    .to_string()
}

fn hydrate_stored_credential(name: &str, mut profile: ProfileConfig) -> Result<ProfileConfig> {
    match profile.credential_store.as_deref() {
        Some("keyring") => set_auth_token(&mut profile.auth, crate::credentials::load(name)?),
        Some("file") | None => {}
        Some(other) => {
            return Err(crate::output::typed_error(
                crate::output::ErrorKind::InvalidInput,
                format!("unsupported credential_store `{other}` for profile `{name}`"),
            ));
        }
    }
    if auth_token(&profile.auth).is_empty() {
        return Err(crate::output::typed_error_with_hint(
            crate::output::ErrorKind::Auth,
            format!("credential not found for profile `{name}`"),
            "run `confluence auth login`",
        ));
    }
    Ok(profile)
}

pub fn expiration_status(expires_at: Option<&str>) -> &'static str {
    let Some(expires_at) = expires_at else {
        return "unknown";
    };
    let Ok(date) = chrono::NaiveDate::parse_from_str(expires_at, "%Y-%m-%d") else {
        return "invalid";
    };
    let days = date
        .signed_duration_since(chrono::Utc::now().date_naive())
        .num_days();
    if days < 0 {
        "expired"
    } else if days <= 30 {
        "expiring-soon"
    } else {
        "valid"
    }
}

pub fn normalize_base_url(value: &str) -> String {
    let trimmed = value.trim().trim_end_matches('/');
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    }
}

pub fn detect_provider(base_url: &str) -> ProviderKind {
    let host = base_url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or_default();
    if host.ends_with(".atlassian.net") || host == "api.atlassian.com" {
        ProviderKind::Cloud
    } else {
        ProviderKind::DataCenter
    }
}

pub fn default_api_path(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Cloud => "/wiki/rest/api",
        ProviderKind::DataCenter => "/rest/api",
    }
}

pub fn build_auth(auth_type: &str, username: Option<String>, token: String) -> Result<AuthConfig> {
    match auth_type {
        "basic" => Ok(AuthConfig::Basic {
            username: username.ok_or_else(|| {
                crate::output::typed_error(
                    crate::output::ErrorKind::InvalidInput,
                    "basic auth requires a username or email",
                )
            })?,
            token,
        }),
        "bearer" => Ok(AuthConfig::Bearer { token }),
        other => Err(crate::output::typed_error(
            crate::output::ErrorKind::InvalidInput,
            format!("unsupported auth type `{other}`"),
        )),
    }
}

pub fn run_login(input: LoginInput) -> Result<ResolvedProfile> {
    let mut config = AppConfig::load()?;
    let mut profile_name = input.profile.unwrap_or_else(|| "default".to_string());
    let mut domain = input.domain.map(|v| normalize_base_url(&v));
    let mut provider = input.provider;
    let api_path = input.api_path;
    let mut auth_type = input.auth_type;
    let mut username = input.username;
    let mut token = input.token.or_else(token_from_env);
    let mut read_only = input.read_only;
    let insecure_storage = input.insecure_storage;
    let cloud_id = input.cloud_id;
    let token_kind = input.token_kind.unwrap_or_else(|| "classic".to_string());
    let expires_at = input.expires_at;

    if !input.non_interactive {
        if profile_name.is_empty() {
            profile_name = prompt("Profile name", "", Some("default"))?;
            if profile_name.is_empty() {
                profile_name = "default".to_string();
            }
        }
        if domain.is_none() {
            let raw = prompt_required("Confluence URL", "e.g. https://mycompany.atlassian.net")?;
            domain = Some(normalize_base_url(&raw));
        }
        if provider.is_none() {
            provider = Some(detect_provider(domain.as_deref().unwrap_or_default()));
        }
        if auth_type.is_none() {
            auth_type = Some(match provider.expect("provider was inferred above") {
                ProviderKind::Cloud => "basic".to_string(),
                ProviderKind::DataCenter => "bearer".to_string(),
            });
        }
        if auth_type.as_deref() == Some("basic") && username.is_none() {
            username = Some(prompt_required("Username or email", "")?);
        }
        if token.is_none() {
            token = Some(prompt_secret_required("API token or password", "")?);
        }
        if read_only.is_none() {
            read_only = Some(prompt_bool("Enable read-only mode?", false)?);
        }
    }

    let domain = domain.ok_or_else(|| {
        crate::output::typed_error(crate::output::ErrorKind::InvalidInput, "domain is required")
    })?;
    let provider = provider.unwrap_or_else(|| detect_provider(&domain));
    let api_path = api_path.unwrap_or_else(|| default_api_path(provider).to_string());
    let auth_type = auth_type.unwrap_or_else(|| {
        if username.is_some() {
            "basic".to_string()
        } else {
            "bearer".to_string()
        }
    });
    let token = token.ok_or_else(|| {
        crate::output::typed_error(crate::output::ErrorKind::Auth, "token is required")
    })?;
    let read_only = read_only.unwrap_or(false);

    if !matches!(token_kind.as_str(), "classic" | "scoped") {
        return Err(crate::output::typed_error(
            crate::output::ErrorKind::InvalidInput,
            format!("unsupported token kind `{token_kind}`; expected classic or scoped"),
        ));
    }
    if token_kind == "scoped" && cloud_id.as_deref().unwrap_or_default().is_empty() {
        return Err(crate::output::typed_error_with_hint(
            crate::output::ErrorKind::Auth,
            "scoped Cloud token requires a Cloud ID",
            "run `confluence auth login`",
        ));
    }

    let auth = build_auth(&auth_type, username, token)?;
    let file_storage = if insecure_storage {
        true
    } else {
        match crate::credentials::available() {
            Ok(()) => false,
            Err(error) if input.non_interactive => {
                bail!("{error}; pass --insecure-storage to use the protected config file")
            }
            Err(error) => {
                eprintln!("  {} {error}", sym_fail());
                if prompt_bool("Use the protected config-file fallback instead?", false)? {
                    true
                } else {
                    bail!(
                        "credential storage cancelled; start an OS credential service or use CONFLUENCE_API_TOKEN for this session"
                    )
                }
            }
        }
    };
    let stored_auth = match &auth {
        AuthConfig::Basic { username, token } => AuthConfig::Basic {
            username: username.clone(),
            token: if file_storage {
                token.clone()
            } else {
                String::new()
            },
        },
        AuthConfig::Bearer { token } => AuthConfig::Bearer {
            token: if file_storage {
                token.clone()
            } else {
                String::new()
            },
        },
    };
    let stored = ProfileConfig {
        provider,
        base_url: domain,
        api_path,
        auth: stored_auth,
        credential_store: Some(if file_storage { "file" } else { "keyring" }.to_string()),
        cloud_id,
        token_kind: Some(token_kind),
        expires_at,
        read_only,
    };

    let previous_keyring = if file_storage {
        None
    } else {
        crate::credentials::load_optional(&profile_name)?
    };
    if !file_storage {
        crate::credentials::store(&profile_name, auth_token(&auth))?;
    }
    config.upsert_profile(profile_name.clone(), stored.clone());
    if let Err(error) = config.save() {
        if !file_storage {
            match previous_keyring {
                Some(previous) => {
                    let _ = crate::credentials::store(&profile_name, &previous);
                }
                None => {
                    let _ = crate::credentials::delete(&profile_name);
                }
            }
        }
        return Err(error);
    }
    if file_storage {
        let _ = crate::credentials::delete(&profile_name);
    }

    config.resolved_profile(Some(&profile_name))
}

fn token_from_env() -> Option<String> {
    [
        "CONFLUENCE_API_TOKEN",
        "CONFLUENCE_TOKEN",
        "CONFLUENCE_PASSWORD",
        "CONFLUENCE_BEARER_TOKEN",
    ]
    .into_iter()
    .find_map(|name| env::var(name).ok().filter(|value| !value.is_empty()))
}

#[cfg(unix)]
fn set_private_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to chmod 0600 {}", path.display()))
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

pub fn logout(profile_override: Option<&str>) -> Result<String> {
    let mut config = AppConfig::load()?;
    let profile_name = profile_override
        .map(ToOwned::to_owned)
        .or_else(|| config.active_profile.clone())
        .ok_or_else(|| {
            crate::output::typed_error(
                crate::output::ErrorKind::Auth,
                "no active profile configured",
            )
        })?;
    let Some(profile) = config.profiles.get_mut(&profile_name) else {
        return Err(crate::output::typed_error(
            crate::output::ErrorKind::NotFound,
            format!("profile `{profile_name}` not found"),
        ));
    };
    let was_keyring = profile.credential_store.as_deref() == Some("keyring");
    let previous_keyring = if was_keyring {
        crate::credentials::load_optional(&profile_name)?
    } else {
        None
    };
    if was_keyring {
        crate::credentials::delete(&profile_name)?;
    }
    set_auth_token(&mut profile.auth, String::new());
    profile.credential_store = None;
    if let Err(error) = config.save() {
        if let Some(previous) = previous_keyring {
            let _ = crate::credentials::store(&profile_name, &previous);
        }
        return Err(error);
    }
    Ok(profile_name)
}

pub fn migrate_credential(profile_override: Option<&str>) -> Result<String> {
    let mut config = AppConfig::load()?;
    let profile_name = profile_override
        .map(ToOwned::to_owned)
        .or_else(|| config.active_profile.clone())
        .ok_or_else(|| {
            crate::output::typed_error(
                crate::output::ErrorKind::Auth,
                "no active profile configured",
            )
        })?;
    let Some(profile) = config.profiles.get_mut(&profile_name) else {
        return Err(crate::output::typed_error(
            crate::output::ErrorKind::NotFound,
            format!("profile `{profile_name}` not found"),
        ));
    };
    if profile.credential_store.as_deref() == Some("keyring") {
        return Err(crate::output::typed_error(
            crate::output::ErrorKind::Conflict,
            format!("profile `{profile_name}` already uses the operating-system keychain"),
        ));
    }
    let token = auth_token(&profile.auth).to_string();
    if token.is_empty() {
        return Err(crate::output::typed_error(
            crate::output::ErrorKind::NotFound,
            format!("profile `{profile_name}` does not contain an inline token to migrate"),
        ));
    }
    crate::credentials::available()?;
    let previous = crate::credentials::load_optional(&profile_name)?;
    crate::credentials::store(&profile_name, &token)?;
    set_auth_token(&mut profile.auth, String::new());
    profile.credential_store = Some("keyring".to_string());
    if let Err(error) = config.save() {
        match previous {
            Some(previous) => {
                let _ = crate::credentials::store(&profile_name, &previous);
            }
            None => {
                let _ = crate::credentials::delete(&profile_name);
            }
        }
        return Err(error);
    }
    Ok(profile_name)
}

pub fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct EnvOverride {
    provider: Option<ProviderKind>,
    base_url: Option<String>,
    api_path: Option<String>,
    auth_type: Option<String>,
    username: Option<String>,
    token: Option<String>,
    cloud_id: Option<String>,
    token_kind: Option<String>,
    read_only: Option<bool>,
}

impl EnvOverride {
    fn from_env() -> Result<Option<Self>> {
        let domain = env::var("CONFLUENCE_DOMAIN").ok();
        let api_path = env::var("CONFLUENCE_API_PATH").ok();
        let auth_type = env::var("CONFLUENCE_AUTH_TYPE").ok();
        let email = env::var("CONFLUENCE_EMAIL")
            .ok()
            .or_else(|| env::var("CONFLUENCE_USERNAME").ok());
        let token = env::var("CONFLUENCE_API_TOKEN")
            .ok()
            .or_else(|| env::var("CONFLUENCE_PASSWORD").ok())
            .or_else(|| env::var("CONFLUENCE_TOKEN").ok())
            .or_else(|| env::var("CONFLUENCE_BEARER_TOKEN").ok());
        let cloud_id = env::var("CONFLUENCE_CLOUD_ID").ok();
        let token_kind = env::var("CONFLUENCE_TOKEN_KIND").ok();
        let provider = env::var("CONFLUENCE_PROVIDER")
            .ok()
            .map(|v| match v.to_ascii_lowercase().as_str() {
                "cloud" => Ok(ProviderKind::Cloud),
                "dc" | "datacenter" | "data_center" | "data-center" | "server" => {
                    Ok(ProviderKind::DataCenter)
                }
                other => Err(crate::output::typed_error(
                    crate::output::ErrorKind::InvalidInput,
                    format!("unsupported CONFLUENCE_PROVIDER `{other}`"),
                )),
            })
            .transpose()?;
        let read_only = env::var("CONFLUENCE_READ_ONLY")
            .ok()
            .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "on"));

        if domain.is_none()
            && api_path.is_none()
            && auth_type.is_none()
            && email.is_none()
            && token.is_none()
            && cloud_id.is_none()
            && token_kind.is_none()
            && provider.is_none()
            && read_only.is_none()
        {
            return Ok(None);
        }

        Ok(Some(Self {
            provider,
            base_url: domain.map(|v| normalize_base_url(&v)),
            api_path,
            auth_type,
            username: email,
            token,
            cloud_id,
            token_kind,
            read_only,
        }))
    }

    fn merge_with(self, name: String, stored: Option<ProfileConfig>) -> ResolvedProfile {
        let stored_provider = stored.as_ref().map(|p| p.provider);
        let base_url = self
            .base_url
            .clone()
            .or_else(|| stored.as_ref().map(|p| p.base_url.clone()))
            .unwrap_or_else(|| "https://example.invalid".to_string());
        let provider = self
            .provider
            .or(stored_provider)
            .unwrap_or_else(|| detect_provider(&base_url));
        let api_path = self
            .api_path
            .clone()
            .or_else(|| stored.as_ref().map(|p| p.api_path.clone()))
            .unwrap_or_else(|| default_api_path(provider).to_string());

        let auth = match (
            self.auth_type
                .or_else(|| stored.as_ref().map(auth_type_name))
                .unwrap_or_else(|| {
                    if self.username.is_some()
                        || stored
                            .as_ref()
                            .and_then(|profile| match &profile.auth {
                                AuthConfig::Basic { .. } => Some(()),
                                AuthConfig::Bearer { .. } => None,
                            })
                            .is_some()
                    {
                        "basic".to_string()
                    } else {
                        "bearer".to_string()
                    }
                })
                .as_str(),
            self.username.or_else(|| {
                stored.as_ref().and_then(|profile| match &profile.auth {
                    AuthConfig::Basic { username, .. } => Some(username.clone()),
                    AuthConfig::Bearer { .. } => None,
                })
            }),
            self.token.or_else(|| {
                stored.as_ref().map(|profile| match &profile.auth {
                    AuthConfig::Basic { token, .. } => token.clone(),
                    AuthConfig::Bearer { token } => token.clone(),
                })
            }),
        ) {
            ("basic", Some(username), Some(token)) => AuthConfig::Basic { username, token },
            ("bearer", _, Some(token)) => AuthConfig::Bearer { token },
            ("basic", _, None) => AuthConfig::Basic {
                username: String::new(),
                token: String::new(),
            },
            ("bearer", _, None) => AuthConfig::Bearer {
                token: String::new(),
            },
            _ => AuthConfig::Bearer {
                token: String::new(),
            },
        };

        let read_only = self
            .read_only
            .or_else(|| stored.as_ref().map(|p| p.read_only))
            .unwrap_or(false);

        let cloud_id = self
            .cloud_id
            .or_else(|| stored.as_ref().and_then(|profile| profile.cloud_id.clone()));
        let token_kind = self
            .token_kind
            .or_else(|| {
                stored
                    .as_ref()
                    .and_then(|profile| profile.token_kind.clone())
            })
            .unwrap_or_else(|| "classic".to_string());
        let expires_at = stored
            .as_ref()
            .and_then(|profile| profile.expires_at.clone());

        ResolvedProfile {
            name,
            provider,
            base_url,
            api_path,
            auth,
            credential_store: "none".to_string(),
            cloud_id,
            token_kind,
            expires_at,
            read_only,
        }
    }
}

/// Top-level `confluence-cli init` command.
///
/// In JSON mode: prints machine-readable setup instructions and exits.
/// In a non-interactive terminal: prints guidance and exits.
/// Otherwise: runs the interactive setup wizard.
pub async fn init(output: OutputFormat) -> Result<()> {
    if output.is_json() {
        return init_json();
    }
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        return Err(crate::output::typed_error_with_hint(
            crate::output::ErrorKind::TtyRequired,
            "interactive setup requires a terminal; use `confluence auth login --non-interactive` for unattended setup",
            "run `confluence init --output json` for setup instructions, or use `confluence auth login --non-interactive` with CONFLUENCE_* environment variables",
        ));
    }
    init_interactive().await
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InitIntent {
    Refresh,
    Reconfigure,
    Add,
}

fn init_json() -> Result<()> {
    let path = AppConfig::config_path()?;
    print_json(&serde_json::json!({
        "configPath": path.display().to_string(),
        "configExists": path.exists(),
        "cloudTokenUrl": "https://id.atlassian.com/manage-profile/security/api-tokens",
        "dcPatDocs": "https://confluence.atlassian.com/enterprise/using-personal-access-tokens-1026032365.html",
        "envVars": {
            "CONFLUENCE_DOMAIN": "Base URL (e.g. https://mycompany.atlassian.net or http://confluence.internal)",
            "CONFLUENCE_PROVIDER": "cloud or datacenter",
            "CONFLUENCE_EMAIL": "Username or email (basic auth)",
            "CONFLUENCE_API_TOKEN": "API token or personal access token",
            "CONFLUENCE_AUTH_TYPE": "basic or bearer",
            "CONFLUENCE_CLOUD_ID": "Atlassian Cloud ID required by a scoped token",
            "CONFLUENCE_TOKEN_KIND": "scoped or classic",
            "CONFLUENCE_READ_ONLY": "1 to prevent write operations"
        },
        "example": {
            "profiles": {
                "cloud": {
                    "provider": "cloud",
                    "base_url": "https://mycompany.atlassian.net",
                    "api_path": "/wiki/rest/api",
                    "auth": { "type": "basic", "username": "me@example.com" },
                    "credential_store": "keyring",
                    "cloud_id": "your-atlassian-cloud-id",
                    "token_kind": "scoped",
                    "expires_at": "2026-11-24",
                    "read_only": false
                },
                "datacenter": {
                    "provider": "data_center",
                    "base_url": "https://confluence.mycompany.com",
                    "api_path": "/rest/api",
                    "auth": { "type": "bearer" },
                    "credential_store": "keyring",
                    "token_kind": "classic",
                    "expires_at": "2026-11-24",
                    "read_only": false
                }
            }
        }
    }))?;
    Ok(())
}

async fn init_interactive() -> Result<()> {
    let sep = sym_dim("──────────────────");
    eprintln!("Confluence CLI");
    eprintln!("{sep}");
    eprintln!();

    let path = AppConfig::config_path()?;
    let mut config = AppConfig::load()?;

    // Returning users usually need to renew a token. Keep that path short,
    // while retaining explicit routes for connection changes and new profiles.
    let (target_name, existing, intent): (Option<String>, Option<ProfileConfig>, InitIntent) =
        if !config.profiles.is_empty() {
            eprintln!("  {}", sym_dim(&format!("Config: {}", path.display())));
            eprintln!();
            eprintln!("  Profiles:");
            for (name, profile) in &config.profiles {
                let active = config.active_profile.as_deref() == Some(name.as_str());
                let marker = if active { "* " } else { "  " };
                eprintln!(
                    "    {}",
                    sym_dim(&format!("{marker}{name} — {}", profile.base_url))
                );
            }
            eprintln!();

            eprintln!(
                "  {}",
                sym_dim(
                    "Refresh replaces only the credential; reconfigure changes connection settings."
                )
            );
            let action_idx = prompt_select(
                "What would you like to do?",
                &["refresh", "reconfigure", "add"],
                0,
            )?;
            eprintln!();

            if action_idx < 2 {
                let names: Vec<&str> = config.profiles.keys().map(String::as_str).collect();
                let idx = if names.len() == 1 {
                    0
                } else {
                    prompt_select("Profile to update", &names, 0)?
                };
                let name = names[idx].to_owned();
                let existing_profile = config.profiles.get(&name).cloned();
                (
                    Some(name),
                    existing_profile,
                    if action_idx == 0 {
                        InitIntent::Refresh
                    } else {
                        InitIntent::Reconfigure
                    },
                )
            } else {
                (None, None, InitIntent::Add)
            }
        } else {
            // First run — no profiles yet, use "default" silently.
            (Some("default".to_owned()), None, InitIntent::Add)
        };

    // URL
    let default_url = existing.as_ref().map(|p| p.base_url.as_str()).unwrap_or("");
    if intent == InitIntent::Refresh {
        let name = target_name.as_deref().unwrap_or("default");
        eprintln!("  Refreshing profile `{name}`");
        eprintln!("  {}", sym_dim(default_url));
        eprintln!();
    }
    let raw_url = if intent == InitIntent::Refresh {
        default_url.to_owned()
    } else {
        prompt(
            "Confluence URL",
            "e.g. https://mycompany.atlassian.net",
            if default_url.is_empty() {
                None
            } else {
                Some(default_url)
            },
        )?
    };
    let raw_url = if raw_url.is_empty() && !default_url.is_empty() {
        default_url.to_owned()
    } else {
        raw_url
    };
    if raw_url.is_empty() {
        return Err(crate::output::typed_error(
            crate::output::ErrorKind::InvalidInput,
            "Confluence URL is required",
        ));
    }
    let base_url = normalize_base_url(&raw_url);

    // Auto-detect provider from URL
    let detected_provider = detect_provider(&base_url);
    let provider = if let Some(ref existing_cfg) = existing {
        if base_url == existing_cfg.base_url {
            existing_cfg.provider
        } else {
            detected_provider
        }
    } else {
        detected_provider
    };
    let provider_label = match provider {
        ProviderKind::Cloud => "Confluence Cloud",
        ProviderKind::DataCenter => "Confluence Data Center",
    };
    if intent != InitIntent::Refresh {
        eprintln!("  {} Detected: {provider_label}", sym_ok());
        eprintln!();
    }

    // Auth
    let existing_token = match (
        existing
            .as_ref()
            .and_then(|profile| profile.credential_store.as_deref()),
        target_name.as_deref(),
    ) {
        (Some("keyring"), Some(name)) => crate::credentials::load_optional(name)?,
        _ => existing
            .as_ref()
            .map(|profile| auth_token(&profile.auth).to_string())
            .filter(|token| !token.is_empty()),
    };
    let has_token = existing_token.is_some();

    let (auth_type, username, token, cloud_id, token_kind, expires_at) = if intent
        == InitIntent::Refresh
    {
        let profile = existing
            .as_ref()
            .expect("refresh requires an existing profile");
        let credential_label = match (profile.provider, &profile.auth) {
            (ProviderKind::Cloud, _) => "New API token",
            (ProviderKind::DataCenter, AuthConfig::Basic { .. }) => "New password",
            (ProviderKind::DataCenter, AuthConfig::Bearer { .. }) => "New personal access token",
        };
        let (auth_type, username) = match &profile.auth {
            AuthConfig::Basic { username, .. } => ("basic", Some(username.clone())),
            AuthConfig::Bearer { .. } => ("bearer", None),
        };
        let raw = prompt_secret(
            credential_label,
            if has_token {
                "Enter to keep the current token"
            } else {
                "required; no stored token was found"
            },
        )?;
        let kept_existing = raw.is_empty();
        let token = if kept_existing {
            existing_token.clone().ok_or_else(|| {
                crate::output::typed_error(
                    crate::output::ErrorKind::Auth,
                    "token is required because no stored credential was found",
                )
            })?
        } else {
            raw
        };
        (
            auth_type,
            username,
            token,
            profile.cloud_id.clone(),
            profile
                .token_kind
                .clone()
                .unwrap_or_else(|| "classic".to_string()),
            if kept_existing {
                profile.expires_at.clone()
            } else {
                None
            },
        )
    } else {
        match provider {
            ProviderKind::Cloud => {
                const TOKEN_URL: &str =
                    "https://id.atlassian.com/manage-profile/security/api-tokens";
                let default_email = existing
                    .as_ref()
                    .and_then(|profile| match &profile.auth {
                        AuthConfig::Basic { username, .. } => Some(username.as_str()),
                        AuthConfig::Bearer { .. } => None,
                    })
                    .unwrap_or("");
                let email = prompt_required_with_default("Atlassian account email", default_email)?;

                eprintln!(
                    "  {}",
                    sym_dim("Choose the token type shown on Atlassian's token page.")
                );
                eprintln!(
                    "  {}",
                    sym_dim("Scoped is recommended; classic supports older API tokens.")
                );
                let default_kind = existing
                    .as_ref()
                    .and_then(|profile| profile.token_kind.as_deref())
                    .unwrap_or("scoped");
                let kind_idx = prompt_select(
                    "API token type",
                    &["scoped", "classic"],
                    usize::from(default_kind == "classic"),
                )?;
                let token_kind = if kind_idx == 1 {
                    "classic".to_string()
                } else {
                    "scoped".to_string()
                };
                let can_keep_token = has_token
                    && existing.as_ref().is_some_and(|profile| {
                        profile.provider == ProviderKind::Cloud
                            && profile.token_kind.as_deref().unwrap_or("classic") == token_kind
                    });
                let cloud_id = if token_kind == "scoped" {
                    eprint!("  Discovering Cloud ID...");
                    std::io::stderr().flush().ok();
                    let id = discover_cloud_id(&base_url).await?;
                    eprintln!(" {}", sym_ok());
                    Some(id)
                } else {
                    None
                };

                eprintln!(
                    "  {}",
                    sym_dim(if can_keep_token {
                        "Manage or replace your API token:"
                    } else {
                        "Create an API token, then paste it below:"
                    })
                );
                eprintln!("  {}", sym_dim(&format!("→ {TOKEN_URL}")));
                if token_kind == "scoped" {
                    eprintln!(
                        "  {}",
                        sym_dim("Grant only the Confluence scopes this profile needs.")
                    );
                }
                let raw = if can_keep_token {
                    prompt_secret("API token", "Enter to keep the current token")?
                } else {
                    prompt_secret_required("API token", "required")?
                };
                let kept_existing = raw.is_empty();
                let token = if kept_existing {
                    existing_token.clone().expect("checked above")
                } else {
                    raw
                };
                let expires_at = if kept_existing {
                    existing
                        .as_ref()
                        .and_then(|profile| profile.expires_at.clone())
                } else {
                    None
                };
                (
                    "basic",
                    Some(email),
                    token,
                    cloud_id,
                    token_kind,
                    expires_at,
                )
            }
            ProviderKind::DataCenter => {
                let pat_url = data_center_pat_url(&base_url);
                let has_data_center_token = has_token
                    && existing
                        .as_ref()
                        .is_some_and(|profile| profile.provider == ProviderKind::DataCenter);
                let (auth_type, username, token, expires_at) = if has_data_center_token {
                    let existing_basic = matches!(
                        existing.as_ref().map(|profile| &profile.auth),
                        Some(AuthConfig::Basic { .. })
                    );
                    eprintln!(
                        "  {}",
                        sym_dim("PAT is recommended; password uses basic authentication.")
                    );
                    let auth_idx = prompt_select(
                        "Authentication",
                        &["PAT", "password"],
                        usize::from(existing_basic),
                    )?;
                    let username = if auth_idx == 1 {
                        let default = existing
                            .as_ref()
                            .and_then(|profile| match &profile.auth {
                                AuthConfig::Basic { username, .. } => Some(username.as_str()),
                                AuthConfig::Bearer { .. } => None,
                            })
                            .unwrap_or("");
                        Some(prompt_required_with_default("Username", default)?)
                    } else {
                        None
                    };
                    let can_keep_credential = existing_basic == (auth_idx == 1);
                    let credential_label = if auth_idx == 1 {
                        "Password"
                    } else {
                        "Personal access token"
                    };
                    let raw = if can_keep_credential {
                        prompt_secret(credential_label, "Enter to keep the current credential")?
                    } else {
                        prompt_secret_required(
                            credential_label,
                            "required after changing authentication",
                        )?
                    };
                    let kept_existing = raw.is_empty();
                    let token = if kept_existing {
                        existing_token.clone().expect("checked above")
                    } else {
                        raw
                    };
                    let expires_at = if kept_existing {
                        existing
                            .as_ref()
                            .and_then(|profile| profile.expires_at.clone())
                    } else {
                        None
                    };
                    (
                        if auth_idx == 1 { "basic" } else { "bearer" },
                        username,
                        token,
                        expires_at,
                    )
                } else {
                    eprintln!(
                        "  {}",
                        sym_dim(
                            "Paste an existing PAT, or create a dedicated PAT through Confluence."
                        )
                    );
                    let setup_idx = prompt_select("PAT setup", &["paste", "create"], 0)?;
                    if setup_idx == 1 {
                        let method_idx = prompt_select(
                            "Authenticate once using",
                            &["password", "existing PAT"],
                            0,
                        )?;
                        let use_pat = method_idx == 1;
                        let username = if use_pat {
                            None
                        } else {
                            Some(prompt_required("Username", "")?)
                        };
                        let secret = prompt_secret(
                            if use_pat { "Existing PAT" } else { "Password" },
                            "used once and never stored",
                        )?;
                        let days = prompt_expiration_days(90)?;
                        eprint!("  Creating dedicated PAT...");
                        std::io::stderr().flush().ok();
                        match create_data_center_pat(
                            &base_url,
                            username.as_deref(),
                            &secret,
                            target_name.as_deref().unwrap_or("confluence-cli"),
                            days,
                        )
                        .await
                        {
                            Ok(token) => {
                                eprintln!(" {}", sym_ok());
                                ("bearer", None, token, Some(expiration_date(days)))
                            }
                            Err(error) => {
                                eprintln!(" {} {error}", sym_fail());
                                eprintln!(
                                    "  Could not create a PAT automatically. Paste one instead."
                                );
                                print_data_center_pat_link(&pat_url);
                                (
                                    "bearer",
                                    None,
                                    prompt_secret_required("Personal access token", "")?,
                                    None,
                                )
                            }
                        }
                    } else {
                        print_data_center_pat_link(&pat_url);
                        (
                            "bearer",
                            None,
                            prompt_secret_required("Personal access token", "")?,
                            None,
                        )
                    }
                };
                (
                    auth_type,
                    username,
                    token,
                    None,
                    "classic".to_string(),
                    expires_at,
                )
            }
        }
    };

    // Default new profiles to the safest useful mode. Returning profiles retain
    // their current policy unless the user explicitly reconfigures them.
    let read_only = if intent == InitIntent::Refresh {
        existing.as_ref().is_some_and(|profile| profile.read_only)
    } else {
        let default_allow_writes = existing.as_ref().is_some_and(|profile| !profile.read_only);
        if existing.is_none() {
            eprintln!(
                "  {}",
                sym_dim("Start read-only unless this profile needs to create or change content.")
            );
        }
        !prompt_bool("Allow commands to change Confluence?", default_allow_writes)?
    };
    eprintln!();

    // Verify credentials
    eprint!("  Verifying credentials...");
    std::io::stderr().flush().ok();

    let api_path = existing
        .as_ref()
        .filter(|profile| profile.provider == provider && profile.base_url == base_url)
        .map(|p| p.api_path.clone())
        .unwrap_or_else(|| default_api_path(provider).to_string());
    let auth = build_auth(auth_type, username, token.clone())?;
    let test_profile = crate::config::ResolvedProfile {
        name: "init-check".to_string(),
        provider,
        base_url: base_url.clone(),
        api_path: api_path.clone(),
        auth: auth.clone(),
        credential_store: "session".to_string(),
        cloud_id: cloud_id.clone(),
        token_kind: token_kind.clone(),
        expires_at: expires_at.clone(),
        read_only: false,
    };
    let test_provider = crate::provider::build_provider(test_profile);
    let verified = match test_provider.ping().await {
        Err(e) => {
            eprintln!(" {} {e}", sym_fail());
            eprintln!();
            prompt_bool("Save profile anyway?", false)?
        }
        Ok(()) => match test_provider.list_spaces(1).await {
            Ok(_) => {
                eprintln!(" {} Connected", sym_ok());
                true
            }
            Err(e) => {
                eprintln!(" {} Authentication failed: {e}", sym_fail());
                eprintln!();
                prompt_bool("Save profile anyway?", false)?
            }
        },
    };

    if !verified {
        eprintln!();
        eprintln!("{sep}");
        return Ok(());
    }

    // Profile name — ask only when adding a new named profile
    let profile_name = match target_name {
        Some(name) => name,
        None => {
            eprintln!();
            let mut suffix = 1;
            let suggestion = loop {
                let candidate = if suffix == 1 {
                    "new".to_string()
                } else {
                    format!("new-{suffix}")
                };
                if !config.profiles.contains_key(&candidate) {
                    break candidate;
                }
                suffix += 1;
            };
            loop {
                let name = prompt_required_with_default("Profile name", &suggestion)?;
                if config.profiles.contains_key(&name) {
                    eprintln!("{} Profile `{name}` already exists", sym_fail());
                } else {
                    break name;
                }
            }
        }
    };

    // Save
    let file_storage = if existing
        .as_ref()
        .and_then(|profile| profile.credential_store.as_deref())
        == Some("file")
    {
        true
    } else {
        choose_credential_storage()?
    };
    let mut stored_auth = auth;
    if !file_storage {
        set_auth_token(&mut stored_auth, String::new());
    }
    let stored = ProfileConfig {
        provider,
        base_url,
        api_path,
        auth: stored_auth,
        credential_store: Some(if file_storage { "file" } else { "keyring" }.to_string()),
        cloud_id,
        token_kind: Some(token_kind),
        expires_at,
        read_only,
    };
    let previous_keyring = if file_storage {
        None
    } else {
        crate::credentials::load_optional(&profile_name)?
    };
    if !file_storage {
        crate::credentials::store(&profile_name, &token)?;
    }
    config.upsert_profile(profile_name.clone(), stored);
    if let Err(error) = config.save() {
        if !file_storage {
            match previous_keyring {
                Some(previous) => {
                    let _ = crate::credentials::store(&profile_name, &previous);
                }
                None => {
                    let _ = crate::credentials::delete(&profile_name);
                }
            }
        }
        return Err(error);
    }
    if file_storage {
        let _ = crate::credentials::delete(&profile_name);
    }

    eprintln!();
    eprintln!("  {} Saved profile `{profile_name}`", sym_ok());
    eprintln!("  {}", sym_dim(&format!("Config: {}", path.display())));
    eprintln!(
        "  {}",
        sym_dim(if file_storage {
            "Credential storage: protected config file; treat it as a secret"
        } else {
            "Credential storage: operating-system keychain"
        })
    );
    eprintln!(
        "  {}",
        sym_dim(if read_only {
            "Access: read-only; commands that change Confluence are blocked"
        } else {
            "Access: write commands enabled"
        })
    );
    eprintln!();
    eprintln!("{sep}");
    eprintln!("  What's next:");
    eprintln!(
        "    {}",
        sym_dim("confluence space list            # browse spaces")
    );
    eprintln!(
        "    {}",
        sym_dim("confluence page list --space KEY # list pages")
    );
    eprintln!(
        "    {}",
        sym_dim("confluence doctor                # verify setup")
    );
    eprintln!("{sep}");
    Ok(())
}

fn prompt_expiration_days(default: u64) -> Result<u64> {
    loop {
        let value = prompt(
            "Dedicated PAT lifetime",
            "[1-365] days",
            Some(&default.to_string()),
        )?;
        match value.parse::<u64>() {
            Ok(days @ 1..=365) => return Ok(days),
            _ => eprintln!("  {} Expiry must be between 1 and 365 days.", sym_fail()),
        }
    }
}

const DATA_CENTER_PAT_PATH: &str = "/plugins/personalaccesstokens/usertokens.action";
const DATA_CENTER_PAT_NAVIGATION: &str = "Avatar → Settings → Personal access tokens";

fn data_center_pat_url(base_url: &str) -> String {
    format!("{}{DATA_CENTER_PAT_PATH}", base_url.trim_end_matches('/'))
}

fn print_data_center_pat_link(url: &str) {
    eprintln!("  {}", sym_dim(&format!("→ {url}")));
    eprintln!("  {}", sym_dim(DATA_CENTER_PAT_NAVIGATION));
}

fn expiration_date(days: u64) -> String {
    (chrono::Utc::now() + chrono::Duration::days(days as i64))
        .date_naive()
        .to_string()
}

fn choose_credential_storage() -> Result<bool> {
    match crate::credentials::available() {
        Ok(()) => Ok(false),
        Err(error) => {
            eprintln!("  {} {error}", sym_fail());
            if prompt_bool("Use the protected config-file fallback instead?", false)? {
                Ok(true)
            } else {
                bail!(
                    "credential storage cancelled; start an OS credential service or use CONFLUENCE_API_TOKEN for this session"
                )
            }
        }
    }
}

async fn discover_cloud_id(base_url: &str) -> Result<String> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct TenantInfo {
        cloud_id: String,
    }

    let info = reqwest::Client::new()
        .get(format!(
            "{}/_edge/tenant_info",
            base_url.trim_end_matches('/')
        ))
        .send()
        .await?
        .error_for_status()?
        .json::<TenantInfo>()
        .await?;
    if info.cloud_id.trim().is_empty() {
        return Err(crate::output::typed_error(
            crate::output::ErrorKind::Api,
            "Atlassian returned an empty Cloud ID",
        ));
    }
    Ok(info.cloud_id)
}

async fn create_data_center_pat(
    base_url: &str,
    username: Option<&str>,
    bootstrap_secret: &str,
    profile_name: &str,
    expiration_days: u64,
) -> Result<String> {
    let request = reqwest::Client::new()
        .post(format!(
            "{}/rest/pat/latest/tokens",
            base_url.trim_end_matches('/')
        ))
        .json(&serde_json::json!({
            "name": format!("confluence-cli / {profile_name}"),
            "expirationDuration": expiration_days,
        }));
    let request = match username {
        Some(username) => request.basic_auth(username, Some(bootstrap_secret)),
        None => request.bearer_auth(bootstrap_secret),
    };
    let response = request.send().await?;
    let status = response.status();
    if !status.is_success() {
        return Err(crate::output::http_error(
            status,
            format!("PAT creation failed with HTTP {status}"),
        ));
    }
    let body: serde_json::Value = response.json().await?;
    ["rawToken", "token"]
        .into_iter()
        .find_map(|field| body.get(field).and_then(serde_json::Value::as_str))
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            crate::output::typed_error(
                crate::output::ErrorKind::Api,
                "PAT creation response did not contain the one-time token",
            )
        })
}

// ── Interactive prompt helpers ────────────────────────────────────────────────

fn sym_q() -> String {
    if use_color() {
        "?".green().bold().to_string()
    } else {
        "?".to_owned()
    }
}

fn sym_ok() -> String {
    if use_color() {
        "✔".green().to_string()
    } else {
        "✔".to_owned()
    }
}

fn sym_fail() -> String {
    if use_color() {
        "✖".red().to_string()
    } else {
        "✖".to_owned()
    }
}

fn sym_dim(s: &str) -> String {
    if use_color() {
        s.dimmed().to_string()
    } else {
        s.to_owned()
    }
}

fn print_prompt(label: &str, hint: &str, default: Option<&str>) {
    let hint_part = if hint.is_empty() {
        String::new()
    } else {
        format!("  {}", sym_dim(hint))
    };
    let default_part = match default {
        Some(default) if !default.is_empty() => {
            format!(" {}", sym_dim(&format!("[{default}]")))
        }
        _ => String::new(),
    };
    eprint!("{} {label}{hint_part}{default_part}: ", sym_q());
    std::io::stderr().flush().ok();
}

/// Print `? Label  hint [default]: ` to stderr and read a line from stdin.
/// Returns the trimmed input, or `default` if the user pressed Enter with no input.
fn prompt(label: &str, hint: &str, default: Option<&str>) -> Result<String> {
    print_prompt(label, hint, default);
    let mut buf = String::new();
    std::io::stdin().read_line(&mut buf)?;
    let trimmed = buf.trim().to_owned();
    if trimmed.is_empty() {
        Ok(default.unwrap_or("").to_owned())
    } else {
        Ok(trimmed)
    }
}

/// Like `prompt`, but loops until the user enters a non-empty value.
fn prompt_required(label: &str, hint: &str) -> Result<String> {
    loop {
        let val = prompt(label, hint, None)?;
        if !val.is_empty() {
            return Ok(val);
        }
        eprintln!("{} {label} is required", sym_fail());
    }
}

fn prompt_required_with_default(label: &str, default: &str) -> Result<String> {
    loop {
        let value = prompt(
            label,
            "",
            if default.is_empty() {
                None
            } else {
                Some(default)
            },
        )?;
        if !value.is_empty() {
            return Ok(value);
        }
        eprintln!("{} {label} is required", sym_fail());
    }
}

/// Prompt for a credential without echoing it to the terminal.
fn prompt_secret(label: &str, hint: &str) -> Result<String> {
    print_prompt(label, hint, None);
    Ok(crate::terminal::read_password()?.trim().to_owned())
}

fn prompt_secret_required(label: &str, hint: &str) -> Result<String> {
    loop {
        let value = prompt_secret(label, hint)?;
        if !value.is_empty() {
            return Ok(value);
        }
        eprintln!("{} {label} is required", sym_fail());
    }
}

/// Print a selection prompt with slash-separated options. Accepts any unambiguous prefix.
fn prompt_select(label: &str, options: &[&str], default_idx: usize) -> Result<usize> {
    let opts_str = options.join("/");
    let default_opt = options.get(default_idx).copied().unwrap_or("");
    loop {
        let raw = prompt(label, &format!("[{opts_str}]"), Some(default_opt))?;
        if let Some(selection) = resolve_selection(&raw, options) {
            return Ok(selection);
        }
        eprintln!("{} Enter one of: {opts_str}", sym_fail());
    }
}

fn resolve_selection(raw: &str, options: &[&str]) -> Option<usize> {
    if let Some(exact) = options
        .iter()
        .position(|option| raw.eq_ignore_ascii_case(option))
    {
        return Some(exact);
    }
    let raw = raw.to_ascii_lowercase();
    let mut matches = options
        .iter()
        .enumerate()
        .filter(|(_, option)| option.to_ascii_lowercase().starts_with(&raw));
    match (matches.next(), matches.next()) {
        (Some((index, _)), None) => Some(index),
        _ => None,
    }
}

/// Print a yes/no prompt. Accepts y/yes/n/no (case-insensitive).
fn prompt_bool(label: &str, default: bool) -> Result<bool> {
    let default_str = if default { "y" } else { "n" };
    loop {
        let raw = prompt(label, "[y/n]", Some(default_str))?;
        match raw.to_ascii_lowercase().as_str() {
            "y" | "yes" | "true" | "1" => return Ok(true),
            "n" | "no" | "false" | "0" => return Ok(false),
            _ => eprintln!("{} Enter yes or no", sym_fail()),
        }
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn auth_type_name(profile: &ProfileConfig) -> String {
    match profile.auth {
        AuthConfig::Basic { .. } => "basic".to_string(),
        AuthConfig::Bearer { .. } => "bearer".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::env;

    use serial_test::serial;
    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn init_text_mode_requires_a_terminal() {
        let error = init(OutputFormat::Text).await.unwrap_err();
        assert!(error.to_string().contains("--non-interactive"));
    }

    // ── normalize_base_url ────────────────────────────────────────────────────

    #[test]
    fn normalize_strips_trailing_slash() {
        assert_eq!(
            normalize_base_url("https://example.atlassian.net/"),
            "https://example.atlassian.net"
        );
    }

    #[test]
    fn normalize_strips_multiple_trailing_slashes() {
        assert_eq!(
            normalize_base_url("https://example.com///"),
            "https://example.com"
        );
    }

    #[test]
    fn normalize_prepends_https_when_scheme_missing() {
        assert_eq!(
            normalize_base_url("example.atlassian.net"),
            "https://example.atlassian.net"
        );
    }

    #[test]
    fn normalize_preserves_http_scheme() {
        assert_eq!(
            normalize_base_url("http://localhost:8090"),
            "http://localhost:8090"
        );
    }

    #[test]
    fn normalize_trims_surrounding_whitespace() {
        assert_eq!(
            normalize_base_url("  https://example.com  "),
            "https://example.com"
        );
    }

    #[test]
    fn normalize_preserves_path_segment() {
        assert_eq!(
            normalize_base_url("https://example.com/confluence"),
            "https://example.com/confluence"
        );
    }

    #[test]
    fn data_center_pat_url_builds_personal_token_page() {
        assert_eq!(
            data_center_pat_url("https://example.com/confluence/"),
            "https://example.com/confluence/plugins/personalaccesstokens/usertokens.action"
        );
    }

    // ── detect_provider ───────────────────────────────────────────────────────

    #[test]
    fn detect_provider_atlassian_net_is_cloud() {
        assert_eq!(
            detect_provider("https://mycompany.atlassian.net"),
            ProviderKind::Cloud
        );
    }

    #[test]
    fn detect_provider_api_atlassian_com_is_cloud() {
        assert_eq!(
            detect_provider("https://api.atlassian.com"),
            ProviderKind::Cloud
        );
    }

    #[test]
    fn detect_provider_self_hosted_is_datacenter() {
        assert_eq!(
            detect_provider("https://confluence.mycompany.com"),
            ProviderKind::DataCenter
        );
    }

    #[test]
    fn detect_provider_localhost_is_datacenter() {
        assert_eq!(
            detect_provider("http://localhost:8090"),
            ProviderKind::DataCenter
        );
    }

    #[test]
    fn selection_matching_is_case_insensitive_and_requires_an_unambiguous_prefix() {
        let options = ["PAT", "password"];
        assert_eq!(resolve_selection("pat", &options), Some(0));
        assert_eq!(resolve_selection("pass", &options), Some(1));
        assert_eq!(resolve_selection("p", &options), None);
        assert_eq!(resolve_selection("unknown", &options), None);
    }

    #[test]
    fn detect_provider_http_atlassian_net_is_cloud() {
        assert_eq!(
            detect_provider("http://mycompany.atlassian.net"),
            ProviderKind::Cloud
        );
    }

    // ── default_api_path ──────────────────────────────────────────────────────

    #[test]
    fn default_api_path_cloud() {
        assert_eq!(default_api_path(ProviderKind::Cloud), "/wiki/rest/api");
    }

    #[test]
    fn default_api_path_datacenter() {
        assert_eq!(default_api_path(ProviderKind::DataCenter), "/rest/api");
    }

    // ── build_auth ────────────────────────────────────────────────────────────

    #[test]
    fn build_auth_basic_succeeds_with_username() {
        let auth = build_auth(
            "basic",
            Some("user@example.com".to_string()),
            "tok".to_string(),
        )
        .unwrap();
        match auth {
            AuthConfig::Basic { username, token } => {
                assert_eq!(username, "user@example.com");
                assert_eq!(token, "tok");
            }
            _ => panic!("expected Basic auth"),
        }
    }

    #[test]
    fn build_auth_basic_fails_without_username() {
        let err = build_auth("basic", None, "tok".to_string()).unwrap_err();
        assert!(err.to_string().contains("username"));
    }

    #[test]
    fn build_auth_bearer_ignores_username() {
        let auth = build_auth("bearer", None, "my-pat".to_string()).unwrap();
        match auth {
            AuthConfig::Bearer { token } => assert_eq!(token, "my-pat"),
            _ => panic!("expected Bearer auth"),
        }
    }

    #[test]
    fn build_auth_unknown_type_returns_error() {
        let err = build_auth("oauth2", None, "tok".to_string()).unwrap_err();
        assert!(err.to_string().contains("oauth2"));
    }

    // ── AppConfig save/load roundtrip ─────────────────────────────────────────

    #[test]
    fn appconfig_save_and_load_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.json");

        // Build config manually and write it
        let mut config = AppConfig::default();
        config.upsert_profile(
            "default".to_string(),
            ProfileConfig {
                provider: ProviderKind::DataCenter,
                base_url: "https://confluence.example.com".to_string(),
                api_path: "/rest/api".to_string(),
                auth: AuthConfig::Basic {
                    username: "alice".to_string(),
                    token: "secret".to_string(),
                },
                credential_store: None,
                cloud_id: None,
                token_kind: None,
                expires_at: None,
                read_only: false,
            },
        );
        let json = serde_json::to_string_pretty(&config).unwrap();
        std::fs::write(&path, json).unwrap();

        let loaded: AppConfig =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(loaded.active_profile.as_deref(), Some("default"));
        let profile = loaded.profiles.get("default").unwrap();
        assert_eq!(profile.base_url, "https://confluence.example.com");
        match &profile.auth {
            AuthConfig::Basic { username, token } => {
                assert_eq!(username, "alice");
                assert_eq!(token, "secret");
            }
            _ => panic!("expected Basic auth"),
        }
    }

    #[test]
    fn appconfig_upsert_sets_active_profile() {
        let mut config = AppConfig::default();
        assert!(config.active_profile.is_none());
        config.upsert_profile(
            "work".to_string(),
            ProfileConfig {
                provider: ProviderKind::Cloud,
                base_url: "https://work.atlassian.net".to_string(),
                api_path: "/wiki/rest/api".to_string(),
                auth: AuthConfig::Bearer {
                    token: "tok".to_string(),
                },
                credential_store: None,
                cloud_id: None,
                token_kind: None,
                expires_at: None,
                read_only: false,
            },
        );
        assert_eq!(config.active_profile.as_deref(), Some("work"));
    }

    #[test]
    fn appconfig_remove_profile_clears_active_when_last() {
        let mut config = AppConfig::default();
        config.upsert_profile(
            "solo".to_string(),
            ProfileConfig {
                provider: ProviderKind::Cloud,
                base_url: "https://example.atlassian.net".to_string(),
                api_path: "/wiki/rest/api".to_string(),
                auth: AuthConfig::Bearer {
                    token: "t".to_string(),
                },
                credential_store: None,
                cloud_id: None,
                token_kind: None,
                expires_at: None,
                read_only: false,
            },
        );
        config.remove_profile("solo").unwrap();
        assert!(config.active_profile.is_none());
        assert!(config.profiles.is_empty());
    }

    #[test]
    fn appconfig_remove_profile_nonexistent_returns_error() {
        let mut config = AppConfig::default();
        assert!(config.remove_profile("ghost").is_err());
    }

    // ── EnvOverride ───────────────────────────────────────────────────────────

    // Env var tests mutate process-global state. #[serial] ensures they run one at a time
    // so parallel test threads don't see each other's env mutations. The unsafe blocks are
    // required by Rust 2024.

    #[test]
    #[serial]
    fn env_override_none_when_no_vars_set() {
        unsafe {
            for var in &[
                "CONFLUENCE_DOMAIN",
                "CONFLUENCE_API_PATH",
                "CONFLUENCE_AUTH_TYPE",
                "CONFLUENCE_EMAIL",
                "CONFLUENCE_USERNAME",
                "CONFLUENCE_API_TOKEN",
                "CONFLUENCE_PASSWORD",
                "CONFLUENCE_TOKEN",
                "CONFLUENCE_BEARER_TOKEN",
                "CONFLUENCE_PROVIDER",
                "CONFLUENCE_READ_ONLY",
            ] {
                env::remove_var(var);
            }
        }
        assert!(EnvOverride::from_env().unwrap().is_none());
    }

    #[test]
    #[serial]
    fn env_override_read_only_accepts_truthy_values() {
        for val in &["1", "true", "TRUE", "yes", "on"] {
            unsafe {
                env::set_var("CONFLUENCE_DOMAIN", "https://example.atlassian.net");
                env::set_var("CONFLUENCE_READ_ONLY", val);
            }
            let ov = EnvOverride::from_env().unwrap().unwrap();
            assert!(
                ov.read_only == Some(true),
                "CONFLUENCE_READ_ONLY={val} should be true"
            );
        }
        unsafe {
            env::remove_var("CONFLUENCE_DOMAIN");
            env::remove_var("CONFLUENCE_READ_ONLY");
        }
    }

    #[test]
    #[serial]
    fn env_override_read_only_false_for_unrecognised_value() {
        unsafe {
            env::set_var("CONFLUENCE_DOMAIN", "https://example.atlassian.net");
            env::set_var("CONFLUENCE_READ_ONLY", "false");
        }
        let ov = EnvOverride::from_env().unwrap().unwrap();
        assert_eq!(ov.read_only, Some(false));
        unsafe {
            env::remove_var("CONFLUENCE_DOMAIN");
            env::remove_var("CONFLUENCE_READ_ONLY");
        }
    }

    #[test]
    #[serial]
    fn env_override_provider_cloud_variants() {
        unsafe {
            env::set_var("CONFLUENCE_DOMAIN", "https://example.atlassian.net");
            env::set_var("CONFLUENCE_PROVIDER", "cloud");
        }
        let ov = EnvOverride::from_env().unwrap().unwrap();
        assert_eq!(ov.provider, Some(ProviderKind::Cloud));
        unsafe {
            env::remove_var("CONFLUENCE_DOMAIN");
            env::remove_var("CONFLUENCE_PROVIDER");
        }
    }

    #[test]
    #[serial]
    fn env_override_provider_datacenter_variants() {
        for val in &["dc", "datacenter", "data_center", "data-center", "server"] {
            unsafe {
                env::set_var("CONFLUENCE_DOMAIN", "https://confluence.example.com");
                env::set_var("CONFLUENCE_PROVIDER", val);
            }
            let ov = EnvOverride::from_env().unwrap().unwrap();
            assert_eq!(ov.provider, Some(ProviderKind::DataCenter), "val={val}");
        }
        unsafe {
            env::remove_var("CONFLUENCE_DOMAIN");
            env::remove_var("CONFLUENCE_PROVIDER");
        }
    }

    #[test]
    #[serial]
    fn env_override_normalizes_base_url() {
        unsafe {
            env::set_var("CONFLUENCE_DOMAIN", "mycompany.atlassian.net/");
        }
        let ov = EnvOverride::from_env().unwrap().unwrap();
        assert_eq!(
            ov.base_url.as_deref(),
            Some("https://mycompany.atlassian.net")
        );
        unsafe {
            env::remove_var("CONFLUENCE_DOMAIN");
        }
    }

    // ── resolved_profile env priority ─────────────────────────────────────────

    #[test]
    #[serial]
    fn resolved_profile_env_vars_override_stored_profile() {
        unsafe {
            for var in &[
                "CONFLUENCE_API_PATH",
                "CONFLUENCE_AUTH_TYPE",
                "CONFLUENCE_EMAIL",
                "CONFLUENCE_USERNAME",
                "CONFLUENCE_PASSWORD",
                "CONFLUENCE_TOKEN",
                "CONFLUENCE_BEARER_TOKEN",
                "CONFLUENCE_PROVIDER",
                "CONFLUENCE_READ_ONLY",
                "CONFLUENCE_PROFILE",
            ] {
                env::remove_var(var);
            }
            env::set_var("CONFLUENCE_DOMAIN", "https://override.atlassian.net");
            env::set_var("CONFLUENCE_API_TOKEN", "env-token");
        }

        let mut config = AppConfig::default();
        config.upsert_profile(
            "default".to_string(),
            ProfileConfig {
                provider: ProviderKind::DataCenter,
                base_url: "https://stored.example.com".to_string(),
                api_path: "/rest/api".to_string(),
                auth: AuthConfig::Bearer {
                    token: "stored-token".to_string(),
                },
                credential_store: None,
                cloud_id: None,
                token_kind: None,
                expires_at: None,
                read_only: false,
            },
        );

        let resolved = config.resolved_profile(None).unwrap();
        assert_eq!(resolved.base_url, "https://override.atlassian.net");
        match &resolved.auth {
            AuthConfig::Bearer { token } => assert_eq!(token, "env-token"),
            _ => panic!("expected Bearer auth from env"),
        }

        unsafe {
            env::remove_var("CONFLUENCE_DOMAIN");
            env::remove_var("CONFLUENCE_API_TOKEN");
        }
    }
}
