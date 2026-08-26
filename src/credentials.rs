use anyhow::{Result, anyhow};

const SERVICE: &str = "confluence-cli";

pub fn store(profile: &str, token: &str) -> Result<()> {
    entry(profile)?
        .set_password(token)
        .map_err(|error| keyring_error("store", error))
}

pub fn load(profile: &str) -> Result<String> {
    entry(profile)?
        .get_password()
        .map_err(|error| keyring_error("read", error))
}

pub fn load_optional(profile: &str) -> Result<Option<String>> {
    match entry(profile)?.get_password() {
        Ok(token) => Ok(Some(token)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(keyring_error("read", error)),
    }
}

pub fn delete(profile: &str) -> Result<bool> {
    match entry(profile)?.delete_credential() {
        Ok(()) => Ok(true),
        Err(keyring::Error::NoEntry) => Ok(false),
        Err(error) => Err(keyring_error("delete", error)),
    }
}

pub fn available() -> Result<()> {
    keyring::Entry::store_status()
        .as_ref()
        .map_err(|error| unavailable_error(&error.to_string()))
        .map(|_| ())
}

fn entry(profile: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, &format!("profile:{profile}"))
        .map_err(|error| keyring_error("open", error))
}

fn unavailable_error(detail: &str) -> anyhow::Error {
    if detail.contains("ServiceUnknown")
        && (detail.contains("org.freedesktop.secrets") || detail.contains("Secret Service"))
    {
        anyhow!("OS credential store is unavailable: no Secret Service provider is running")
    } else {
        anyhow!("OS credential store is unavailable: {detail}")
    }
}

fn keyring_error(operation: &str, error: keyring::Error) -> anyhow::Error {
    match error {
        keyring::Error::NoEntry => anyhow!(
            "credential not found for profile; run `confluence auth login` or `confluence auth migrate`"
        ),
        other => anyhow!("failed to {operation} OS-keychain credential: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_linux_secret_service_has_a_human_error() {
        let error = unavailable_error(
            "org.freedesktop.DBus.Error.ServiceUnknown: org.freedesktop.secrets was not provided",
        );
        assert!(error.to_string().contains("no Secret Service provider"));
    }
}
