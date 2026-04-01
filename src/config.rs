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
        if api_path.is_none() {
            let default_path = default_api_path(provider.unwrap()).to_string();
            api_path = Some(prompt("REST API path", "", Some(&default_path))?);
        }
        if auth_type.is_none() {
            let idx = prompt_select("Auth type", &["basic", "bearer"], 0)?;
            auth_type = Some(if idx == 0 { "basic" } else { "bearer" }.to_string());
        }
        if auth_type.as_deref() == Some("basic") && username.is_none() {
            username = Some(prompt_required("Username or email", "")?);
        }
        if token.is_none() {
            token = Some(prompt_required("API token or password", "")?);
        }
        if read_only.is_none() {
            read_only = Some(prompt_bool("Enable read-only mode?", false)?);
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
    let sep = sym_dim("──────────────────");
    eprintln!("Confluence CLI");
    eprintln!("{sep}");
    eprintln!();

    let path = AppConfig::config_path()?;
    let mut config = AppConfig::load()?;

    // Determine intent: first run, update existing profile, or add new.
    let (target_name, existing): (Option<String>, Option<ProfileConfig>) =
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

            let action_idx = prompt_select("Action", &["update", "add"], 0)?;
            eprintln!();

            if action_idx == 0 {
                let names: Vec<&str> = config.profiles.keys().map(String::as_str).collect();
                let idx = if names.len() == 1 {
                    0
                } else {
                    prompt_select("Profile to update", &names, 0)?
                };
                let name = names[idx].to_owned();
                let existing_profile = config.profiles.get(&name).cloned();
                (Some(name), existing_profile)
            } else {
                (None, None)
            }
        } else {
            // First run — no profiles yet, use "default" silently.
            (Some("default".to_owned()), None)
        };

    // URL
    let default_url = existing.as_ref().map(|p| p.base_url.as_str()).unwrap_or("");
    let raw_url = prompt(
        "URL",
        "e.g. https://mycompany.atlassian.net",
        if default_url.is_empty() {
            None
        } else {
            Some(default_url)
        },
    )?;
    let raw_url = if raw_url.is_empty() && !default_url.is_empty() {
        default_url.to_owned()
    } else {
        raw_url
    };
    if raw_url.is_empty() {
        bail!("Confluence URL is required");
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
    eprintln!("  {} Detected: {provider_label}", sym_ok());
    eprintln!();

    // Auth
    let existing_token = existing.as_ref().map(|p| match &p.auth {
        AuthConfig::Basic { token, .. } | AuthConfig::Bearer { token } => token.clone(),
    });
    let has_token = existing_token
        .as_ref()
        .map(|t| !t.is_empty())
        .unwrap_or(false);

    let (auth_type, username, token) = match provider {
        ProviderKind::Cloud => {
            let token_url = "https://id.atlassian.com/manage-profile/security/api-tokens";
            eprintln!("  {}", sym_dim(token_url));
            eprintln!();
            let default_email = existing
                .as_ref()
                .and_then(|p| match &p.auth {
                    AuthConfig::Basic { username, .. } => Some(username.as_str()),
                    _ => None,
                })
                .unwrap_or("");
            let email = prompt(
                "Email",
                "",
                if default_email.is_empty() {
                    None
                } else {
                    Some(default_email)
                },
            )?;
            let email = if email.is_empty() {
                default_email.to_owned()
            } else {
                email
            };
            let token_hint = if has_token {
                "Enter to keep existing"
            } else {
                ""
            };
            let raw_token = prompt(
                "Token",
                if token_hint.is_empty() {
                    ""
                } else {
                    token_hint
                },
                None,
            )?;
            let token = if raw_token.is_empty() && has_token {
                existing_token.unwrap()
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
                "  {}",
                sym_dim(&format!(
                    "Tokens: https://{dc_host}/plugins/servlet/manage-api-tokens"
                ))
            );
            eprintln!();

            let default_auth_idx = existing
                .as_ref()
                .map(|p| match &p.auth {
                    AuthConfig::Basic { .. } => 0usize,
                    AuthConfig::Bearer { .. } => 1usize,
                })
                .unwrap_or(1);
            let auth_idx = prompt_select("Auth", &["basic", "bearer"], default_auth_idx)?;
            eprintln!();

            if auth_idx == 0 {
                let default_user = existing
                    .as_ref()
                    .and_then(|p| match &p.auth {
                        AuthConfig::Basic { username, .. } => Some(username.as_str()),
                        _ => None,
                    })
                    .unwrap_or("");
                let username = prompt(
                    "Username",
                    "",
                    if default_user.is_empty() {
                        None
                    } else {
                        Some(default_user)
                    },
                )?;
                let username = if username.is_empty() {
                    default_user.to_owned()
                } else {
                    username
                };
                let token_hint = if has_token {
                    "Enter to keep existing"
                } else {
                    ""
                };
                let raw = prompt(
                    "Token",
                    if token_hint.is_empty() {
                        ""
                    } else {
                        token_hint
                    },
                    None,
                )?;
                let token = if raw.is_empty() && has_token {
                    existing_token.unwrap()
                } else {
                    raw
                };
                ("basic", Some(username), token)
            } else {
                let token_hint = if has_token {
                    "Enter to keep existing"
                } else {
                    ""
                };
                let raw = prompt(
                    "Personal Access Token",
                    if token_hint.is_empty() {
                        ""
                    } else {
                        token_hint
                    },
                    None,
                )?;
                let token = if raw.is_empty() && has_token {
                    existing_token.unwrap()
                } else {
                    raw
                };
                ("bearer", None, token)
            }
        }
    };

    // Read-only
    let default_ro = existing.as_ref().map(|p| p.read_only).unwrap_or(false);
    let read_only = prompt_bool("Read-only mode?", default_ro)?;
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
            let name = prompt("Profile name", "", Some("default"))?;
            if name.is_empty() {
                "default".to_owned()
            } else {
                name
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
    eprintln!("  {} Saved profile `{profile_name}`", sym_ok());
    eprintln!("  {}", sym_dim(&format!("Config: {}", path.display())));
    eprintln!();
    eprintln!("{sep}");
    eprintln!("  What's next:");
    eprintln!(
        "    {}",
        sym_dim("confluence-cli space list            # browse spaces")
    );
    eprintln!(
        "    {}",
        sym_dim("confluence-cli page list --space KEY # list pages")
    );
    eprintln!(
        "    {}",
        sym_dim("confluence-cli doctor                # verify setup")
    );
    eprintln!("{sep}");
    Ok(())
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

/// Print `? Label  hint [default]: ` to stderr and read a line from stdin.
/// Returns the trimmed input, or `default` if the user pressed Enter with no input.
fn prompt(label: &str, hint: &str, default: Option<&str>) -> Result<String> {
    let hint_part = if hint.is_empty() {
        String::new()
    } else {
        format!("  {}", sym_dim(hint))
    };
    let default_part = match default {
        Some(d) if !d.is_empty() => format!(" {}", sym_dim(&format!("[{d}]"))),
        _ => String::new(),
    };
    eprint!("{} {label}{hint_part}{default_part}: ", sym_q());
    std::io::stderr().flush().ok();
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

/// Print a selection prompt with slash-separated options. Accepts any unambiguous prefix.
/// Falls back to `default_idx` on unrecognised input.
fn prompt_select(label: &str, options: &[&str], default_idx: usize) -> Result<usize> {
    let opts_str = options.join("/");
    let default_opt = options.get(default_idx).copied().unwrap_or("");
    let raw = prompt(label, &format!("[{opts_str}]"), Some(default_opt))?;
    for (i, opt) in options.iter().enumerate() {
        if raw.eq_ignore_ascii_case(opt) || opt.starts_with(&raw.to_ascii_lowercase()) {
            return Ok(i);
        }
    }
    Ok(default_idx)
}

/// Print a yes/no prompt. Accepts y/yes/n/no (case-insensitive). Falls back to `default`.
fn prompt_bool(label: &str, default: bool) -> Result<bool> {
    let default_str = if default { "y" } else { "n" };
    let raw = prompt(label, "[y/n]", Some(default_str))?;
    Ok(match raw.to_ascii_lowercase().as_str() {
        "y" | "yes" | "true" | "1" => true,
        "n" | "no" | "false" | "0" => false,
        _ => default,
    })
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn auth_type_name(profile: &ProfileConfig) -> String {
    match profile.auth {
        AuthConfig::Basic { .. } => "basic".to_string(),
        AuthConfig::Bearer { .. } => "bearer".to_string(),
    }
}
