use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use std::{io::Write, path::Path};
use tokio_tungstenite::tungstenite::http::Uri;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionProfile {
    pub id: String,
    pub name: String,
    pub endpoint: String,
    pub token: String,
    /// Pin the engine, not just the network address.
    pub device_id: String,
}

impl std::fmt::Debug for ConnectionProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionProfile")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("endpoint", &self.endpoint)
            .field("device_id", &self.device_id)
            .finish_non_exhaustive()
    }
}

impl ConnectionProfile {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.id.is_empty() && !self.device_id.is_empty(),
            "Connection identity is missing"
        );
        anyhow::ensure!(
            !self.name.trim().is_empty() && self.name.len() <= 128,
            "Computer name must be 1–128 characters"
        );
        anyhow::ensure!(
            self.token.len() == 64 && self.token.bytes().all(|b| b.is_ascii_hexdigit()),
            "Invalid connection key"
        );
        validate_endpoint(&self.endpoint)
    }
    pub fn code(&self) -> anyhow::Result<String> {
        self.validate()?;
        Ok(format!(
            "noches-connect:{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(self)?)
        ))
    }
    pub fn from_code(code: &str) -> anyhow::Result<Self> {
        anyhow::ensure!(code.len() <= 8192, "Connection code is too long");
        let encoded = code
            .trim()
            .strip_prefix("noches-connect:")
            .ok_or_else(|| anyhow::anyhow!("Paste a Noches connection code"))?;
        let profile: Self = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(encoded)?)?;
        profile.validate()?;
        Ok(profile)
    }
}

/// Plain WebSocket is restricted to WireGuard-encrypted tailnet addresses or
/// localhost. Internet endpoints must use TLS. No DNS rebinding for ws URLs.
pub fn validate_endpoint(endpoint: &str) -> anyhow::Result<()> {
    let uri: Uri = endpoint.parse()?;
    anyhow::ensure!(
        uri.path() == "/" && uri.query().is_none(),
        "Use a server address without a path or query"
    );
    let authority = uri
        .authority()
        .ok_or_else(|| anyhow::anyhow!("Missing server address"))?;
    anyhow::ensure!(
        !authority.as_str().contains('@'),
        "Credentials must not be in the address"
    );
    match uri.scheme_str() {
        Some("wss") => Ok(()),
        Some("ws") => {
            let host = uri
                .host()
                .unwrap_or("")
                .trim_start_matches('[')
                .trim_end_matches(']');
            let ip = host.parse().map_err(|_| {
                anyhow::anyhow!("Use the computer's Tailscale IP address for a direct connection")
            })?;
            anyhow::ensure!(
                super::private_ip(ip),
                "Unencrypted connections must use a Tailscale or loopback address"
            );
            Ok(())
        }
        _ => anyhow::bail!("Use ws:// for a Tailscale address or wss:// for a TLS server"),
    }
}

#[derive(Default, Clone, Serialize, Deserialize)]
pub struct Connections {
    pub hosts: Vec<ConnectionProfile>,
}
impl Connections {
    pub fn load(dir: &Path) -> anyhow::Result<Self> {
        let path = dir.join("connections.json");
        match std::fs::read(path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }
    pub fn save(&self, dir: &Path) -> anyhow::Result<()> {
        for host in &self.hosts {
            host.validate()?;
        }
        private_write(
            &dir.join("connections.json"),
            &serde_json::to_vec_pretty(self)?,
        )
    }
    pub fn add(&mut self, profile: ConnectionProfile) {
        self.hosts.retain(|h| h.id != profile.id);
        self.hosts.push(profile);
    }
}

#[derive(Default, Serialize, Deserialize)]
pub struct Credentials {
    pub clients: Vec<ConnectionProfile>,
}
impl Credentials {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        private_write(path, &serde_json::to_vec_pretty(self)?)
    }
    pub fn authorizes(&self, token: &str) -> bool {
        if token.len() != 64 {
            return false;
        }
        self.clients.iter().any(|c| {
            c.token.len() == token.len()
                && c.token
                    .bytes()
                    .zip(token.bytes())
                    .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                    == 0
        })
    }
}

pub fn private_write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_encrypted_or_tailnet_endpoints_are_accepted() {
        for url in [
            "ws://100.114.177.75:27657",
            "ws://127.0.0.1:2",
            "wss://host.example",
            "ws://[fd7a:115c:a1e0::1]:2",
        ] {
            validate_endpoint(url).unwrap();
        }
        for url in [
            "ws://example.com",
            "ws://192.168.1.1",
            "ws://0.0.0.0",
            "http://100.114.177.75",
            "ws://100.114.177.75?token=x",
            "wss://u:p@host",
        ] {
            assert!(validate_endpoint(url).is_err(), "{url}");
        }
    }
    #[test]
    fn pairing_codes_round_trip_and_secrets_are_redacted() {
        let p = ConnectionProfile {
            id: "id".into(),
            name: "Linux".into(),
            endpoint: "ws://100.114.177.75:2".into(),
            token: "a".repeat(64),
            device_id: "device".into(),
        };
        assert_eq!(
            ConnectionProfile::from_code(&p.code().unwrap())
                .unwrap()
                .token,
            p.token
        );
        assert!(!format!("{p:?}").contains(&p.token));
        let dir = tempfile::tempdir().unwrap();
        let mut store = Connections::default();
        store.add(p);
        store.save(dir.path()).unwrap();
        assert_eq!(Connections::load(dir.path()).unwrap().hosts.len(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(dir.path().join("connections.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
}
