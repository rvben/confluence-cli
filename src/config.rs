use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use dialoguer::{Confirm, Input, Password, Select};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::model::ProviderKind;
use crate::output::{OutputFormat, print_json};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum AuthConfig {
    Basic { username: String, token: String },
    Bearer { token: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileConfig {
    pub provider: ProviderKind,
    pub base_url: String,
    pub api_path: String,
    pub auth: AuthConfig,
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
        let raw = serde_json::to_string_pretty(self)?;
        fs::write(&path, raw).with_context(|| format!("failed to write {}", path.display()))?;
        Ok(())
    }

    pub fn resolved_profile(&self, profile_override: Option<&str>) -> Result<ResolvedProfile> {
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

        match (stored, env_override) {
            (Some((name, stored)), Some(override_cfg)) => {
                Ok(override_cfg.merge_with(name, Some(stored)))
            }
            (Some((name, stored)), None) => Ok(ResolvedProfile::from_stored(name, stored)),
            (None, Some(override_cfg)) => {
                Ok(override_cfg
                    .merge_with(selected_name.unwrap_or_else(|| "env".to_string()), None))
            }
            (None, None) => bail!(
                "no active profile configured. Run `confluence-cli auth login` or set CONFLUENCE_* environment variables"
            ),
        }
    }

    pub fn upsert_profile(&mut self, name: String, profile: ProfileConfig) {
        self.profiles.insert(name.clone(), profile);
        self.active_profile = Some(name);
    }

    pub fn remove_profile(&mut self, name: &str) -> Result<()> {
        if self.profiles.remove(name).is_none() {
            bail!("profile `{name}` not found");
        }
        if self.active_profile.as_deref() == Some(name) {
            self.active_profile = self.profiles.keys().next().cloned();
        }
        Ok(())
    }

    pub fn set_active_profile(&mut self, name: &str) -> Result<()> {
        if !self.profiles.contains_key(name) {
            bail!("profile `{name}` not found");
        }
        self.active_profile = Some(name.to_string());
        Ok(())
    }
}

impl ResolvedProfile {
    fn from_stored(name: String, profile: ProfileConfig) -> Self {
        Self {
            name,
            provider: profile.provider,
            base_url: profile.base_url,
            api_path: profile.api_path,
            auth: profile.auth,
            read_only: profile.read_only,
        }
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
            username: username.ok_or_else(|| anyhow!("basic auth requires a username/email"))?,
            token,
        }),
        "bearer" => Ok(AuthConfig::Bearer { token }),
        other => bail!("unsupported auth type `{other}`"),
    }
}

pub fn run_login(input: LoginInput) -> Result<ResolvedProfile> {
    let mut config = AppConfig::load()?;
    let mut profile_name = input.profile.unwrap_or_else(|| "default".to_string());
    let mut domain = input.domain.map(|v| normalize_base_url(&v));
    let mut provider = input.provider;
    let mut api_path = input.api_path;
    let mut auth_type = input.auth_type;
    let mut username = input.username;
    let mut token = input.token;
    let mut read_only = input.read_only;

    if !input.non_interactive {
        if profile_name.is_empty() {
            profile_name = Input::new()
                .with_prompt("Profile name")
                .default("default".to_string())
                .interact_text()?;
        }
        if domain.is_none() {
            domain = Some(normalize_base_url(
                &Input::<String>::new()
                    .with_prompt("Confluence domain or base URL")
                    .interact_text()?,
            ));
        }
        if provider.is_none() {
            provider = Some(detect_provider(domain.as_deref().unwrap_or_default()));
        }
        if api_path.is_none() {
            api_path = Some(
                Input::new()
                    .with_prompt("REST API path")
                    .default(default_api_path(provider.unwrap()).to_string())
                    .interact_text()?,
            );
        }
        if auth_type.is_none() {
            let idx = Select::new()
                .with_prompt("Authentication type")
                .items(["basic", "bearer"])
                .default(0)
                .interact()?;
            auth_type = Some(if idx == 0 { "basic" } else { "bearer" }.to_string());
        }
        if auth_type.as_deref() == Some("basic") && username.is_none() {
            username = Some(
                Input::new()
                    .with_prompt("Username or email")
                    .interact_text()?,
            );
        }
        if token.is_none() {
            token = Some(
                Password::new()
                    .with_prompt("API token or password")
                    .with_confirmation("Confirm token", "Tokens did not match")
                    .interact()?,
            );
        }
        if read_only.is_none() {
            read_only = Some(
                Confirm::new()
                    .with_prompt("Enable read-only mode?")
                    .default(false)
                    .interact()?,
            );
        }
    }

    let domain = domain.ok_or_else(|| anyhow!("domain is required"))?;
    let provider = provider.unwrap_or_else(|| detect_provider(&domain));
    let api_path = api_path.unwrap_or_else(|| default_api_path(provider).to_string());
    let auth_type = auth_type.unwrap_or_else(|| {
        if username.is_some() {
            "basic".to_string()
        } else {
            "bearer".to_string()
        }
    });
    let token = token.ok_or_else(|| anyhow!("token is required"))?;
    let read_only = read_only.unwrap_or(false);

    let auth = build_auth(&auth_type, username, token)?;
    let stored = ProfileConfig {
        provider,
        base_url: domain,
        api_path,
        auth,
        read_only,
    };

    config.upsert_profile(profile_name.clone(), stored.clone());
    config.save()?;

    Ok(ResolvedProfile::from_stored(profile_name, stored))
}

pub fn logout(profile_override: Option<&str>) -> Result<String> {
    let mut config = AppConfig::load()?;
    let profile_name = profile_override
        .map(ToOwned::to_owned)
        .or_else(|| config.active_profile.clone())
        .ok_or_else(|| anyhow!("no active profile configured"))?;
    let Some(profile) = config.profiles.get_mut(&profile_name) else {
        bail!("profile `{profile_name}` not found");
    };
    profile.auth = AuthConfig::Bearer {
        token: String::new(),
    };
    config.save()?;
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
        let provider = env::var("CONFLUENCE_PROVIDER")
            .ok()
            .map(|v| match v.to_ascii_lowercase().as_str() {
                "cloud" => Ok(ProviderKind::Cloud),
                "dc" | "datacenter" | "data_center" | "data-center" | "server" => {
                    Ok(ProviderKind::DataCenter)
                }
                other => bail!("unsupported CONFLUENCE_PROVIDER `{other}`"),
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

        ResolvedProfile {
            name,
            provider,
            base_url,
            api_path,
            auth,
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
    if matches!(output, OutputFormat::Json) {
        return init_json();
    }
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        eprintln!(
            "Run `confluence-cli init` in an interactive terminal, \
             or use `confluence-cli init --json` for machine-readable setup instructions."
        );
        return Ok(());
    }
    init_interactive().await
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
            "CONFLUENCE_READ_ONLY": "1 to prevent write operations"
        },
        "example": {
            "profiles": {
                "cloud": {
                    "provider": "cloud",
                    "base_url": "https://mycompany.atlassian.net",
                    "api_path": "/wiki/rest/api",
                    "auth": { "type": "basic", "username": "me@example.com", "token": "ATATT3x..." },
                    "read_only": false
                },
                "datacenter": {
                    "provider": "data_center",
                    "base_url": "https://confluence.mycompany.com",
                    "api_path": "/rest/api",
                    "auth": { "type": "bearer", "token": "your-personal-access-token" },
                    "read_only": false
                }
            }
        }
    }))?;
    Ok(())
}

async fn init_interactive() -> Result<()> {
    let sep = "──────────────";
    eprintln!("Confluence CLI Setup");
    eprintln!("{sep}");
    eprintln!();

    let path = AppConfig::config_path()?;
    let mut config = AppConfig::load()?;

    // Determine intent: first run, update existing profile, or add new.
    let (target_name, existing): (Option<String>, Option<ProfileConfig>) =
        if !config.profiles.is_empty() {
            eprintln!("  Config:   {}", path.display());
            eprintln!();
            eprintln!("  Profiles:");
            for (name, profile) in &config.profiles {
                let active = config.active_profile.as_deref() == Some(name.as_str());
                let marker = if active { "* " } else { "  " };
                eprintln!("    {}{} — {}", marker, name, profile.base_url);
            }
            eprintln!();

            let action_idx = Select::new()
                .with_prompt("What would you like to do?")
                .items(["Update an existing profile", "Add a new profile"])
                .default(0)
                .interact()?;
            eprintln!();

            if action_idx == 0 {
                let names: Vec<&str> = config.profiles.keys().map(String::as_str).collect();
                let idx = if names.len() == 1 {
                    0
                } else {
                    Select::new()
                        .with_prompt("Profile to update")
                        .items(&names)
                        .default(0)
                        .interact()?
                };
                let name = names[idx].to_owned();
                let existing_profile = config.profiles.get(&name).cloned();
                (Some(name), existing_profile)
            } else {
                (None, None)
            }
        } else {
            // First run — no profiles yet.
            (Some("default".to_owned()), None)
        };

    // URL
    let default_url = existing.as_ref().map(|p| p.base_url.as_str()).unwrap_or("");
    let raw_url: String = Input::new()
        .with_prompt("Confluence URL")
        .with_initial_text(default_url)
        .interact_text()?;
    let base_url = normalize_base_url(&raw_url);

    // Auto-detect provider from URL
    let detected_provider = detect_provider(&base_url);
    let provider = if let Some(ref existing_cfg) = existing {
        // Keep existing provider unless URL changed
        if base_url == existing_cfg.base_url {
            existing_cfg.provider
        } else {
            detected_provider
        }
    } else {
        detected_provider
    };
    eprintln!(
        "  Detected: {} ({})",
        match provider {
            ProviderKind::Cloud => "Confluence Cloud",
            ProviderKind::DataCenter => "Confluence Data Center / Server",
        },
        match provider {
            ProviderKind::Cloud => "atlassian.net",
            ProviderKind::DataCenter => "self-hosted",
        }
    );
    eprintln!();

    // Auth
    let (auth_type, username, token) = match provider {
        ProviderKind::Cloud => {
            eprintln!("  Token: https://id.atlassian.com/manage-profile/security/api-tokens");
            let default_email = existing
                .as_ref()
                .and_then(|p| match &p.auth {
                    AuthConfig::Basic { username, .. } => Some(username.as_str()),
                    _ => None,
                })
                .unwrap_or("");
            let email: String = Input::new()
                .with_prompt("Email")
                .with_initial_text(default_email)
                .interact_text()?;
            let has_existing_token = existing
                .as_ref()
                .map(|p| match &p.auth {
                    AuthConfig::Basic { token, .. } | AuthConfig::Bearer { token } => {
                        !token.is_empty()
                    }
                })
                .unwrap_or(false);
            let token_hint = if has_existing_token {
                " (Enter to keep)"
            } else {
                ""
            };
            let raw_token: String = Password::new()
                .with_prompt(format!("API token{token_hint}"))
                .allow_empty_password(has_existing_token)
                .interact()?;
            let token = if raw_token.is_empty() && has_existing_token {
                match &existing.as_ref().unwrap().auth {
                    AuthConfig::Basic { token, .. } | AuthConfig::Bearer { token } => token.clone(),
                }
            } else {
                raw_token
            };
            ("basic", Some(email), token)
        }
        ProviderKind::DataCenter => {
            let dc_host = base_url
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .split('/')
                .next()
                .unwrap_or(&base_url);
            eprintln!(
                "  Token:  https://{dc_host}/plugins/servlet/de.resolution.apitokenauth/admin"
            );
            eprintln!("  Or PAT: https://{dc_host}/plugins/servlet/manage-api-tokens");
            eprintln!();

            // For DC: offer basic (username + token) or bearer (PAT only)
            let default_auth_idx = existing
                .as_ref()
                .map(|p| match &p.auth {
                    AuthConfig::Basic { .. } => 0usize,
                    AuthConfig::Bearer { .. } => 1usize,
                })
                .unwrap_or(1);
            let auth_idx = Select::new()
                .with_prompt("Authentication type")
                .items([
                    "Basic (username + password/token)",
                    "Bearer (Personal Access Token)",
                ])
                .default(default_auth_idx)
                .interact()?;
            eprintln!();

            if auth_idx == 0 {
                let default_user = existing
                    .as_ref()
                    .and_then(|p| match &p.auth {
                        AuthConfig::Basic { username, .. } => Some(username.as_str()),
                        _ => None,
                    })
                    .unwrap_or("");
                let username: String = Input::new()
                    .with_prompt("Username")
                    .with_initial_text(default_user)
                    .interact_text()?;
                let has_existing_token = existing
                    .as_ref()
                    .map(|p| match &p.auth {
                        AuthConfig::Basic { token, .. } | AuthConfig::Bearer { token } => {
                            !token.is_empty()
                        }
                    })
                    .unwrap_or(false);
                let hint = if has_existing_token {
                    " (Enter to keep)"
                } else {
                    ""
                };
                let raw: String = Password::new()
                    .with_prompt(format!("Password or token{hint}"))
                    .allow_empty_password(has_existing_token)
                    .interact()?;
                let token = if raw.is_empty() && has_existing_token {
                    match &existing.as_ref().unwrap().auth {
                        AuthConfig::Basic { token, .. } | AuthConfig::Bearer { token } => {
                            token.clone()
                        }
                    }
                } else {
                    raw
                };
                ("basic", Some(username), token)
            } else {
                let has_existing_token = existing
                    .as_ref()
                    .map(|p| match &p.auth {
                        AuthConfig::Basic { token, .. } | AuthConfig::Bearer { token } => {
                            !token.is_empty()
                        }
                    })
                    .unwrap_or(false);
                let hint = if has_existing_token {
                    " (Enter to keep)"
                } else {
                    ""
                };
                let raw: String = Password::new()
                    .with_prompt(format!("Personal Access Token{hint}"))
                    .allow_empty_password(has_existing_token)
                    .interact()?;
                let token = if raw.is_empty() && has_existing_token {
                    match &existing.as_ref().unwrap().auth {
                        AuthConfig::Basic { token, .. } | AuthConfig::Bearer { token } => {
                            token.clone()
                        }
                    }
                } else {
                    raw
                };
                ("bearer", None, token)
            }
        }
    };

    // Read-only
    let default_ro = existing.as_ref().map(|p| p.read_only).unwrap_or(false);
    let read_only = Confirm::new()
        .with_prompt("Enable read-only mode? (prevents accidental writes)")
        .default(default_ro)
        .interact()?;
    eprintln!();

    // Verify credentials
    eprint!("  Verifying credentials...");
    std::io::stderr().flush().ok();

    let api_path = existing
        .as_ref()
        .map(|p| p.api_path.clone())
        .unwrap_or_else(|| default_api_path(provider).to_string());
    let auth = build_auth(auth_type, username, token.clone())?;
    let test_profile = crate::config::ResolvedProfile {
        name: "init-check".to_string(),
        provider,
        base_url: base_url.clone(),
        api_path: api_path.clone(),
        auth: auth.clone(),
        read_only: false,
    };
    let test_provider = crate::provider::build_provider(test_profile);
    let verified = match test_provider.ping().await {
        Err(e) => {
            eprintln!(" ✗ {e}");
            eprintln!();
            Confirm::new()
                .with_prompt("Save profile anyway?")
                .default(false)
                .interact()?
        }
        Ok(()) => match test_provider.list_spaces(1).await {
            Ok(_) => {
                eprintln!(" ✓ Connected");
                true
            }
            Err(e) => {
                eprintln!(" ✗ Authentication failed: {e}");
                eprintln!();
                Confirm::new()
                    .with_prompt("Save profile anyway?")
                    .default(false)
                    .interact()?
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
            let raw: String = Input::new()
                .with_prompt("Profile name")
                .default("default".to_owned())
                .interact_text()?;
            let trimmed = raw.trim().to_owned();
            if trimmed.is_empty() {
                "default".to_owned()
            } else {
                trimmed
            }
        }
    };

    // Save
    let stored = ProfileConfig {
        provider,
        base_url,
        api_path,
        auth,
        read_only,
    };
    config.upsert_profile(profile_name.clone(), stored);
    config.save()?;

    eprintln!();
    eprintln!("  Saved profile `{profile_name}` → {}", path.display());
    eprintln!();
    eprintln!("  What's next:");
    eprintln!("    confluence-cli space list            # browse spaces");
    eprintln!("    confluence-cli page list --space KEY # list pages");
    eprintln!("    confluence-cli doctor                # verify setup");
    eprintln!();
    eprintln!("{sep}");
    Ok(())
}

fn auth_type_name(profile: &ProfileConfig) -> String {
    match profile.auth {
        AuthConfig::Basic { .. } => "basic".to_string(),
        AuthConfig::Bearer { .. } => "bearer".to_string(),
    }
}
