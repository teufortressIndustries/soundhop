//! TLS certificate generation, persistence, and `rustls` server configuration for Web Broadcast.

use std::collections::BTreeSet;
use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose,
};
use rustls::ServerConfig;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};

use crate::util::{atomic_write, hostname_name, local_addresses};

use super::{WebError, WebResult};

const CA_CERT_FILENAME: &str = "ca.crt";
const CA_KEY_FILENAME: &str = "ca.key";
const LEAF_CERT_FILENAME: &str = "leaf.crt";
const LEAF_KEY_FILENAME: &str = "leaf.key";
const LEAF_META_FILENAME: &str = "leaf.meta";

/// Reuse persisted leaf certs for up to 300 days (365-day validity).
const LEAF_MAX_AGE_SECS: u64 = 300 * 24 * 60 * 60;
/// Reuse persisted CA certs for up to 9 years (10-year validity).
const CA_MAX_AGE_SECS: u64 = 9 * 365 * 24 * 60 * 60;

/// Loaded or generated TLS material ready for `HttpServerHandle`.
#[derive(Clone)]
pub struct TlsServerMaterial {
    pub server_config: Arc<ServerConfig>,
    /// Public Root CA PEM bytes (served on `GET /ca.crt` when auto-generated).
    pub ca_cert_pem: Option<Vec<u8>>,
}

/// Default directory for persisted Web Broadcast TLS certificates (`<state_dir>/web-tls`).
pub fn default_web_tls_dir() -> PathBuf {
    crate::util::state_dir().join("web-tls")
}

/// Load a user-supplied PEM certificate chain and private key into a `rustls::ServerConfig`.
pub fn load_custom_tls(cert_path: &Path, key_path: &Path) -> WebResult<TlsServerMaterial> {
    let cert_pem = fs::read(cert_path).map_err(|e| {
        WebError::InvalidConfig(format!(
            "cannot read TLS certificate {}: {e}",
            cert_path.display()
        ))
    })?;
    let key_pem = fs::read(key_path).map_err(|e| {
        WebError::InvalidConfig(format!(
            "cannot read TLS private key {}: {e}",
            key_path.display()
        ))
    })?;

    let certs = parse_cert_chain(&cert_pem)?;
    let key = parse_private_key(&key_pem)?;
    let server_config = build_server_config(certs, key)?;

    Ok(TlsServerMaterial {
        server_config,
        ca_cert_pem: None,
    })
}

/// Load or generate a persistent SoundHop Local Root CA and a LAN leaf certificate with
/// SANs covering all current non-loopback LAN IPs, loopback addresses, `localhost`, and `<host>.local`.
pub fn load_or_generate_lan_tls(
    tls_dir: &Path,
    device_name: &str,
    extra_sans: &[String],
) -> WebResult<TlsServerMaterial> {
    fs::create_dir_all(tls_dir)?;

    let ca_cert_path = tls_dir.join(CA_CERT_FILENAME);
    let ca_key_path = tls_dir.join(CA_KEY_FILENAME);
    let leaf_cert_path = tls_dir.join(LEAF_CERT_FILENAME);
    let leaf_key_path = tls_dir.join(LEAF_KEY_FILENAME);
    let leaf_meta_path = tls_dir.join(LEAF_META_FILENAME);

    let (ca_pem, ca_key_pem, ca_reused) =
        load_or_create_ca(&ca_cert_path, &ca_key_path, device_name)?;
    let ca_key = KeyPair::from_pem(&ca_key_pem)
        .map_err(|e| WebError::InvalidConfig(format!("invalid CA key PEM: {e}")))?;
    let issuer = Issuer::from_ca_cert_pem(&ca_pem, ca_key)
        .map_err(|e| WebError::InvalidConfig(format!("invalid CA cert PEM: {e}")))?;

    let sans = collect_lan_sans(extra_sans);

    let (leaf_pem, leaf_key_pem) = if ca_reused {
        if let Some(reused) =
            try_load_valid_leaf(&leaf_cert_path, &leaf_key_path, &leaf_meta_path, &sans)
        {
            reused
        } else {
            generate_and_save_leaf(
                &leaf_cert_path,
                &leaf_key_path,
                &leaf_meta_path,
                &sans,
                &issuer,
            )?
        }
    } else {
        generate_and_save_leaf(
            &leaf_cert_path,
            &leaf_key_path,
            &leaf_meta_path,
            &sans,
            &issuer,
        )?
    };

    let mut certs = parse_cert_chain(leaf_pem.as_bytes())?;
    certs.extend(parse_cert_chain(ca_pem.as_bytes())?);
    let key = parse_private_key(leaf_key_pem.as_bytes())?;
    let server_config = build_server_config(certs, key)?;

    Ok(TlsServerMaterial {
        server_config,
        ca_cert_pem: Some(ca_pem.into_bytes()),
    })
}

/// Collect sorted, deduplicated SANs for the current host (LAN IPs, loopback, `localhost`, `<host>.local`).
pub fn collect_lan_sans(extra_sans: &[String]) -> Vec<String> {
    let mut set = BTreeSet::new();
    set.insert("127.0.0.1".to_string());
    set.insert("::1".to_string());
    set.insert("localhost".to_string());

    for addr in local_addresses(false) {
        let trimmed = addr.trim();
        if !trimmed.is_empty() {
            set.insert(trimmed.to_string());
        }
    }

    let host = hostname_name();
    let host_clean = host.trim().trim_end_matches('.');
    if !host_clean.is_empty() && !host_clean.eq_ignore_ascii_case("localhost") {
        set.insert(host_clean.to_string());
        if !host_clean.ends_with(".local") {
            set.insert(format!("{host_clean}.local"));
        }
    }

    for extra in extra_sans {
        let trimmed = extra.trim();
        if !trimmed.is_empty() {
            set.insert(trimmed.to_string());
        }
    }

    set.into_iter().collect()
}

fn load_or_create_ca(
    ca_cert_path: &Path,
    ca_key_path: &Path,
    device_name: &str,
) -> WebResult<(String, String, bool)> {
    if ca_cert_path.exists()
        && ca_key_path.exists()
        && !is_file_older_than(ca_cert_path, CA_MAX_AGE_SECS)
    {
        if let (Ok(cert_pem), Ok(key_pem)) = (
            fs::read_to_string(ca_cert_path),
            fs::read_to_string(ca_key_path),
        ) {
            if let Ok(key) = KeyPair::from_pem(&key_pem) {
                if Issuer::from_ca_cert_pem(&cert_pem, key).is_ok() {
                    ensure_private_key_permissions(ca_key_path);
                    return Ok((cert_pem, key_pem, true));
                }
            }
        }
    }

    let mut params = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    let label = if device_name.trim().is_empty() {
        "SoundHop".to_string()
    } else {
        device_name.trim().to_string()
    };
    dn.push(DnType::CommonName, format!("SoundHop Local CA ({label})"));
    dn.push(DnType::OrganizationName, "SoundHop");
    params.distinguished_name = dn;
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    set_validity_days(&mut params, 3650);

    let ca_key = KeyPair::generate()
        .map_err(|e| WebError::InvalidConfig(format!("failed to generate CA key: {e}")))?;
    let ca_cert = params
        .self_signed(&ca_key)
        .map_err(|e| WebError::InvalidConfig(format!("failed to self-sign CA cert: {e}")))?;

    let ca_pem = ca_cert.pem();
    let ca_key_pem = ca_key.serialize_pem();

    atomic_write(ca_cert_path, &ca_pem)?;
    write_private_key(ca_key_path, &ca_key_pem)?;

    Ok((ca_pem, ca_key_pem, false))
}

fn try_load_valid_leaf(
    leaf_cert_path: &Path,
    leaf_key_path: &Path,
    leaf_meta_path: &Path,
    expected_sans: &[String],
) -> Option<(String, String)> {
    let meta = fs::read_to_string(leaf_meta_path).ok()?;
    let mut lines = meta.lines();
    let issued_secs: u64 = lines.next()?.trim().parse().ok()?;
    let now_secs = now_unix_secs();
    if now_secs.saturating_sub(issued_secs) >= LEAF_MAX_AGE_SECS {
        return None;
    }
    let stored_sans: Vec<String> = lines
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if stored_sans != expected_sans {
        return None;
    }

    let cert_pem = fs::read_to_string(leaf_cert_path).ok()?;
    let key_pem = fs::read_to_string(leaf_key_path).ok()?;
    if parse_cert_chain(cert_pem.as_bytes()).is_err()
        || parse_private_key(key_pem.as_bytes()).is_err()
    {
        return None;
    }
    ensure_private_key_permissions(leaf_key_path);
    Some((cert_pem, key_pem))
}

fn generate_and_save_leaf(
    leaf_cert_path: &Path,
    leaf_key_path: &Path,
    leaf_meta_path: &Path,
    sans: &[String],
    issuer: &Issuer<'_, KeyPair>,
) -> WebResult<(String, String)> {
    let mut params = CertificateParams::new(sans.to_vec())
        .map_err(|e| WebError::InvalidConfig(format!("invalid SANs for TLS cert: {e}")))?;
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "SoundHop Web Broadcast");
    dn.push(DnType::OrganizationName, "SoundHop");
    params.distinguished_name = dn;
    params.is_ca = IsCa::NoCa;
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    set_validity_days(&mut params, 365);

    let leaf_key = KeyPair::generate()
        .map_err(|e| WebError::InvalidConfig(format!("failed to generate leaf key: {e}")))?;
    let leaf_cert = params
        .signed_by(&leaf_key, issuer)
        .map_err(|e| WebError::InvalidConfig(format!("failed to sign leaf cert: {e}")))?;

    let leaf_pem = leaf_cert.pem();
    let leaf_key_pem = leaf_key.serialize_pem();

    let mut meta = format!("{}\n", now_unix_secs());
    for san in sans {
        meta.push_str(san);
        meta.push('\n');
    }

    atomic_write(leaf_cert_path, &leaf_pem)?;
    write_private_key(leaf_key_path, &leaf_key_pem)?;
    atomic_write(leaf_meta_path, &meta)?;

    Ok((leaf_pem, leaf_key_pem))
}

fn set_validity_days(params: &mut CertificateParams, days: i64) {
    let now = rcgen::date_time_ymd(2025, 1, 1);
    let _ = now;
    // Use current time via SystemTime -> time::OffsetDateTime if available; rcgen defaults to
    // 1975..4096 when unset, so we compute explicit not_before/not_after from SystemTime.
    let now_secs = now_unix_secs();
    if let Ok(not_before) = unix_secs_to_rcgen_time(now_secs.saturating_sub(3600)) {
        params.not_before = not_before;
    }
    let duration_secs = u64::try_from(days.max(1)).unwrap_or(365) * 24 * 60 * 60;
    if let Ok(not_after) = unix_secs_to_rcgen_time(now_secs.saturating_add(duration_secs)) {
        params.not_after = not_after;
    }
}

fn unix_secs_to_rcgen_time(secs: u64) -> Result<time::OffsetDateTime, time::error::ComponentRange> {
    let ts = i64::try_from(secs).unwrap_or(i64::MAX);
    time::OffsetDateTime::from_unix_timestamp(ts)
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn is_file_older_than(path: &Path, max_age_secs: u64) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return true;
    };
    let Ok(modified) = meta.modified() else {
        return true;
    };
    let Ok(age) = SystemTime::now().duration_since(modified) else {
        return false;
    };
    age.as_secs() >= max_age_secs
}

fn write_private_key(path: &Path, pem: &str) -> WebResult<()> {
    atomic_write(path, pem)?;
    ensure_private_key_permissions(path);
    Ok(())
}

fn ensure_private_key_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

fn parse_cert_chain(pem: &[u8]) -> WebResult<Vec<CertificateDer<'static>>> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| WebError::InvalidConfig(format!("invalid PEM certificate: {e}")))?;
    if certs.is_empty() {
        return Err(WebError::InvalidConfig(
            "TLS certificate file contains no certificates".into(),
        ));
    }
    Ok(certs)
}

fn parse_private_key(pem: &[u8]) -> WebResult<PrivateKeyDer<'static>> {
    PrivateKeyDer::from_pem_slice(pem)
        .map_err(|e| WebError::InvalidConfig(format!("invalid PEM private key: {e}")))
}

fn build_server_config(
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> WebResult<Arc<ServerConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| WebError::InvalidConfig(format!("TLS protocol version error: {e}")))?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| WebError::InvalidConfig(format!("TLS cert/key mismatch: {e}")))?;
    Ok(Arc::new(config))
}

/// Returns `true` when `ip` is a loopback address (`127.0.0.1`, `::1`, or IPv4-mapped loopback).
pub(crate) fn is_loopback_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(v6))
            .is_loopback(),
        IpAddr::V4(_) => ip.is_loopback(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};
    use tempfile::TempDir;

    #[test]
    fn generates_persists_and_reloads_ca_and_leaf_cert() {
        let dir = TempDir::new().expect("tempdir");
        let mat1 = load_or_generate_lan_tls(dir.path(), "TestHost", &["10.20.30.40".into()])
            .expect("generate tls");
        assert!(mat1.ca_cert_pem.is_some());
        assert!(dir.path().join(CA_CERT_FILENAME).exists());
        assert!(dir.path().join(CA_KEY_FILENAME).exists());
        assert!(dir.path().join(LEAF_CERT_FILENAME).exists());
        assert!(dir.path().join(LEAF_KEY_FILENAME).exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let ca_mode = fs::metadata(dir.path().join(CA_KEY_FILENAME))
                .expect("ca key meta")
                .permissions()
                .mode()
                & 0o777;
            let leaf_mode = fs::metadata(dir.path().join(LEAF_KEY_FILENAME))
                .expect("leaf key meta")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(ca_mode, 0o600);
            assert_eq!(leaf_mode, 0o600);
        }

        let leaf_pem_1 = fs::read_to_string(dir.path().join(LEAF_CERT_FILENAME)).expect("leaf pem");
        let ca_pem_1 = fs::read_to_string(dir.path().join(CA_CERT_FILENAME)).expect("ca pem");

        // Reloading with identical SANs reuses both CA and leaf certs.
        let mat2 = load_or_generate_lan_tls(dir.path(), "TestHost", &["10.20.30.40".into()])
            .expect("reload tls");
        assert_eq!(mat2.ca_cert_pem.as_deref(), Some(ca_pem_1.as_bytes()));
        let leaf_pem_2 =
            fs::read_to_string(dir.path().join(LEAF_CERT_FILENAME)).expect("leaf pem2");
        assert_eq!(leaf_pem_1, leaf_pem_2);

        // Changing SANs regenerates the leaf cert while preserving the CA cert.
        let _mat3 = load_or_generate_lan_tls(dir.path(), "TestHost", &["10.20.30.41".into()])
            .expect("regen leaf");
        let ca_pem_3 = fs::read_to_string(dir.path().join(CA_CERT_FILENAME)).expect("ca pem3");
        let leaf_pem_3 =
            fs::read_to_string(dir.path().join(LEAF_CERT_FILENAME)).expect("leaf pem3");
        assert_eq!(ca_pem_1, ca_pem_3);
        assert_ne!(leaf_pem_1, leaf_pem_3);
    }

    #[test]
    fn loads_custom_cert_and_key_without_ca_endpoint() {
        let dir = TempDir::new().expect("tempdir");
        let _ = load_or_generate_lan_tls(dir.path(), "CustomHost", &[]).expect("gen");
        let custom = load_custom_tls(
            &dir.path().join(LEAF_CERT_FILENAME),
            &dir.path().join(LEAF_KEY_FILENAME),
        )
        .expect("load custom tls");
        assert!(custom.ca_cert_pem.is_none());
    }

    #[test]
    fn collect_lan_sans_includes_loopback_localhost_and_extra() {
        let sans = collect_lan_sans(&["192.168.99.99".into()]);
        assert!(sans.contains(&"127.0.0.1".to_string()));
        assert!(sans.contains(&"::1".to_string()));
        assert!(sans.contains(&"localhost".to_string()));
        assert!(sans.contains(&"192.168.99.99".to_string()));
    }

    #[test]
    fn is_loopback_ip_handles_v4_v6_and_mapped() {
        assert!(is_loopback_ip(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert!(is_loopback_ip(IpAddr::V6(Ipv6Addr::LOCALHOST)));
        assert!(is_loopback_ip(
            "::ffff:127.0.0.1".parse::<IpAddr>().expect("mapped")
        ));
        assert!(!is_loopback_ip(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10))));
    }
}
