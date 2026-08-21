use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::atomic;
use crate::command::Runner;

const MAX_CA_BYTES: usize = 1024 * 1024;
const MAX_CERT_BYTES: usize = 1024 * 1024;
const MAX_PRIVATE_KEY_BYTES: usize = 256 * 1024;

pub type TlsIdentity = (Vec<u8>, Vec<u8>);
pub type TlsMaterial = (Vec<u8>, Option<TlsIdentity>);

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Settings {
    pub enabled: bool,
    pub node_id: String,
    pub endpoints: Vec<String>,
    pub ca: Option<PathBuf>,
    pub cert: Option<PathBuf>,
    pub key: Option<PathBuf>,
    pub user: Option<String>,
    pub password: Option<String>,
    /// Optional administrator zone connected bidirectionally to the dedicated
    /// `meduza` zone. An empty value disables firewall integration.
    pub firewall_zone: Option<String>,
}

impl Settings {
    pub fn load<R: Runner>(runner: &R) -> Result<Self> {
        let changes = runner.output("uci", ["changes", "meduza"])?;
        if !changes.status.success() {
            bail!("could not inspect uncommitted meduza UCI changes");
        }
        if !String::from_utf8(changes.stdout)
            .context("UCI changes output was not UTF-8")?
            .trim()
            .is_empty()
        {
            bail!("uncommitted meduza UCI changes exist");
        }
        let section = runner.output("uci", ["-q", "get", "meduza.main"])?;
        if !section.status.success()
            || String::from_utf8(section.stdout)
                .context("UCI section type was not UTF-8")?
                .trim()
                != "meduza"
        {
            bail!("required UCI section meduza.main is missing or invalid");
        }
        let get = |name: &str| -> Result<Option<String>> {
            if let Ok(value) = std::env::var(format!("MEDUZA_{name}")) {
                return Ok((!value.is_empty()).then_some(value));
            }
            let expression = format!("meduza.main.{name}");
            let output = runner.output("uci", ["-q", "get", expression.as_str()])?;
            if output.status.success() {
                let value = String::from_utf8(output.stdout)
                    .context("UCI returned non-UTF-8 settings")?
                    .trim()
                    .to_owned();
                return Ok((!value.is_empty()).then_some(value));
            }
            if output.status.code() == Some(1) {
                Ok(None)
            } else {
                bail!("could not read UCI setting meduza.main.{name}")
            }
        };

        Self::from_values(
            get("enable")?.as_deref().unwrap_or("0"),
            get("NODE_ID")?.unwrap_or_default(),
            get("ETCD_ENDPOINTS")?.unwrap_or_else(|| "https://127.0.0.1:2379".into()),
            get("ETCD_CA")?,
            get("ETCD_CERT")?,
            get("ETCD_KEY")?,
            get("ETCD_USER")?,
            get("ETCD_PASS")?,
            get("VPN_FIREWALL_ZONE")?,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn from_values(
        enabled: &str,
        node_id: String,
        endpoints: String,
        ca: Option<String>,
        cert: Option<String>,
        key: Option<String>,
        user: Option<String>,
        password: Option<String>,
        firewall_zone: Option<String>,
    ) -> Result<Self> {
        validate_node_id(&node_id)?;
        let endpoints = endpoints
            .split(',')
            .filter(|value| !value.trim().is_empty())
            .map(|value| {
                let value = value.trim();
                let normalized = if value.contains("://") {
                    value.to_owned()
                } else {
                    format!("https://{value}")
                };
                let parsed = Url::parse(&normalized).context("invalid etcd endpoint")?;
                if !matches!(parsed.scheme(), "http" | "https")
                    || parsed.host_str().is_none()
                    || !has_explicit_port(&normalized)
                    || !parsed.username().is_empty()
                    || parsed.password().is_some()
                    || parsed.path() != "/"
                    || parsed.query().is_some()
                    || parsed.fragment().is_some()
                {
                    bail!("invalid etcd endpoint shape");
                }
                let scheme_end = normalized
                    .find("://")
                    .context("etcd endpoint has no scheme separator")?;
                // URL schemes are case-insensitive. Preserve the explicit
                // authority/port while canonicalizing the scheme so TLS and
                // default-CA selection cannot diverge from validation.
                Ok(format!("{}{}", parsed.scheme(), &normalized[scheme_end..]))
            })
            .collect::<Result<Vec<_>>>()?;
        if endpoints.is_empty() {
            bail!("ETCD_ENDPOINTS is required");
        }
        if cert.is_some() != key.is_some() {
            bail!("ETCD_CERT and ETCD_KEY must be configured together");
        }
        if user.is_some() != password.is_some() {
            bail!("ETCD_USER and ETCD_PASS must be configured together");
        }
        let firewall_zone = firewall_zone
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        if let Some(zone) = &firewall_zone {
            validate_firewall_zone(zone)?;
            if zone == "meduza" {
                bail!("VPN_FIREWALL_ZONE must select a zone other than meduza");
            }
        }
        let ca = ca.map(PathBuf::from).or_else(|| {
            endpoints
                .iter()
                .any(|value| value.starts_with("https://"))
                .then(|| PathBuf::from("/etc/ssl/certs/ca-certificates.crt"))
        });
        Ok(Self {
            enabled: bool_value(enabled),
            node_id,
            endpoints,
            ca,
            cert: cert.map(PathBuf::from),
            key: key.map(PathBuf::from),
            user,
            password,
            firewall_zone,
        })
    }

    pub fn tls_material(&self) -> Result<Option<TlsMaterial>> {
        let Some(ca) = &self.ca else { return Ok(None) };
        let ca = atomic::read_bounded(ca, MAX_CA_BYTES)
            .with_context(|| format!("could not read {}", ca.display()))?;
        let identity = match (&self.cert, &self.key) {
            (Some(cert), Some(key)) => Some((
                atomic::read_bounded(cert, MAX_CERT_BYTES)
                    .with_context(|| format!("could not read {}", cert.display()))?,
                atomic::read_bounded(key, MAX_PRIVATE_KEY_BYTES)
                    .with_context(|| format!("could not read {}", key.display()))?,
            )),
            _ => None,
        };
        Ok(Some((ca, identity)))
    }
}

fn has_explicit_port(value: &str) -> bool {
    let Some((_, remainder)) = value.split_once("://") else {
        return false;
    };
    let authority = remainder.split(['/', '?', '#']).next().unwrap_or_default();
    let port = if let Some(ipv6) = authority.strip_prefix('[') {
        let Some((_, suffix)) = ipv6.split_once(']') else {
            return false;
        };
        suffix.strip_prefix(':')
    } else {
        authority.rsplit_once(':').map(|(_, port)| port)
    };
    port.is_some_and(|port| {
        !port.is_empty()
            && port.bytes().all(|byte| byte.is_ascii_digit())
            && port.parse::<u16>().is_ok_and(|port| port != 0)
    })
}

pub fn bool_value(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

pub fn validate_node_id(value: &str) -> Result<()> {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        bail!("NODE_ID is required")
    };
    if value.len() > 128
        || !(first.is_ascii_alphanumeric() || first == '_')
        || !chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-'))
    {
        bail!("NODE_ID contains unsafe characters");
    }
    Ok(())
}

pub fn validate_logical_name(value: &str) -> Result<()> {
    if value.is_empty()
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    {
        bail!("invalid logical interface identifier: {value}");
    }
    Ok(())
}

pub fn validate_firewall_zone(value: &str) -> Result<()> {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        bail!("firewall zone is empty")
    };
    if value.len() > 64
        || !(first.is_ascii_alphanumeric() || first == '_')
        || !chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-'))
    {
        bail!("firewall zone contains unsafe characters");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_and_node_validation() {
        let value = Settings::from_values(
            "true",
            "router-01".into(),
            "etcd-a:2379,https://etcd-b:2379".into(),
            None,
            None,
            None,
            None,
            None,
            Some("vpn-zone".into()),
        )
        .unwrap();
        assert!(value.enabled);
        assert_eq!(value.endpoints[0], "https://etcd-a:2379");
        assert_eq!(value.firewall_zone.as_deref(), Some("vpn-zone"));
        assert!(validate_node_id("-bad").is_err());
        assert!(validate_logical_name("bad-name").is_err());
        assert!(validate_firewall_zone("vpn-zone").is_ok());
        assert!(validate_firewall_zone("bad zone").is_err());
        assert!(
            Settings::from_values(
                "false",
                "router-01".into(),
                "https://etcd.example:443".into(),
                None,
                None,
                None,
                None,
                None,
                Some("meduza".into()),
            )
            .is_err()
        );
        for endpoint in ["https://etcd.example:443", "http://etcd.example:80"] {
            Settings::from_values(
                "false",
                "router-01".into(),
                endpoint.into(),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        }
        assert!(
            Settings::from_values(
                "false",
                "router-01".into(),
                "https://etcd.example".into(),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .is_err()
        );

        let mixed_case_https = Settings::from_values(
            "false",
            "router-01".into(),
            "HTTPS://etcd.example:443".into(),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(mixed_case_https.endpoints, ["https://etcd.example:443"]);
        assert_eq!(
            mixed_case_https.ca,
            Some(PathBuf::from("/etc/ssl/certs/ca-certificates.crt"))
        );
    }

    #[test]
    fn tls_material_rejects_oversized_files() {
        let temp = tempfile::tempdir().unwrap();
        let ca = temp.path().join("ca.pem");
        let file = std::fs::File::create(&ca).unwrap();
        file.set_len((MAX_CA_BYTES + 1) as u64).unwrap();
        let settings = Settings {
            enabled: true,
            node_id: "router-01".into(),
            endpoints: vec!["https://127.0.0.1:2379".into()],
            ca: Some(ca),
            cert: None,
            key: None,
            user: None,
            password: None,
            firewall_zone: None,
        };

        assert!(settings.tls_material().is_err());
    }
}
