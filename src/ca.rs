//! Local certificate authority: mint once, reuse, and build the hudsucker
//! signing authority from it.
//!
//! The CA private key is the trust anchor for every forged leaf, so it is
//! written `0600`. The cert is what the user installs into the keychain
//! (`rtr trust`) and what CA env vars point at.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use hudsucker::certificate_authority::RcgenAuthority;
use hudsucker::rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use hudsucker::rustls::crypto::aws_lc_rs;
use sha2::{Digest, Sha256};

const LEAF_CACHE_SIZE: u64 = 1_000;

pub struct CaMaterial {
    pub cert_pem: String,
    pub key_pem: String,
    pub cert_path: PathBuf,
}

impl CaMaterial {
    pub fn fingerprint(&self) -> Result<String> {
        fingerprint_sha256(&self.cert_pem)
    }

    /// Build the hudsucker authority that signs per-host leaf certs.
    pub fn authority(&self) -> Result<RcgenAuthority> {
        let key_pair = KeyPair::from_pem(&self.key_pem).context("parsing CA private key")?;
        let issuer =
            Issuer::from_ca_cert_pem(&self.cert_pem, key_pair).context("parsing CA certificate")?;
        Ok(RcgenAuthority::new(
            issuer,
            LEAF_CACHE_SIZE,
            aws_lc_rs::default_provider(),
        ))
    }
}

/// Mint a fresh self-signed CA. Returns `(cert_pem, key_pem)`.
pub fn generate() -> Result<(String, String)> {
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "rtr local CA");
    dn.push(DnType::OrganizationName, "rtr");
    params.distinguished_name = dn;

    let key_pair = KeyPair::generate().context("generating CA key pair")?;
    let cert = params.self_signed(&key_pair).context("self-signing CA")?;
    Ok((cert.pem(), key_pair.serialize_pem()))
}

/// Load the CA if both files exist, otherwise mint and persist it.
pub fn load_or_generate(cert_path: &Path, key_path: &Path) -> Result<CaMaterial> {
    if cert_path.exists() && key_path.exists() {
        let cert_pem = std::fs::read_to_string(cert_path)
            .with_context(|| format!("reading {}", cert_path.display()))?;
        let key_pem = std::fs::read_to_string(key_path)
            .with_context(|| format!("reading {}", key_path.display()))?;
        return Ok(CaMaterial {
            cert_pem,
            key_pem,
            cert_path: cert_path.to_path_buf(),
        });
    }

    let (cert_pem, key_pem) = generate()?;
    if let Some(parent) = cert_path.parent() {
        crate::paths::create_private_dir_all(parent)?;
    }
    std::fs::write(cert_path, &cert_pem)
        .with_context(|| format!("writing {}", cert_path.display()))?;
    crate::config::write_secret_file(key_path, &key_pem)?;
    Ok(CaMaterial {
        cert_pem,
        key_pem,
        cert_path: cert_path.to_path_buf(),
    })
}

pub fn fingerprint_sha256(cert_pem: &str) -> Result<String> {
    let der = first_cert_der(cert_pem)?;
    Ok(hex_colons(&Sha256::digest(&der)))
}

fn first_cert_der(cert_pem: &str) -> Result<Vec<u8>> {
    let mut reader = std::io::BufReader::new(cert_pem.as_bytes());
    let first = rustls_pemfile::certs(&mut reader)
        .next()
        .context("no certificate found in PEM")?
        .context("parsing certificate PEM")?;
    Ok(first.as_ref().to_vec())
}

fn hex_colons(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_cert_is_a_ca_with_cert_sign() {
        let (cert_pem, _key) = generate().unwrap();
        let der = first_cert_der(&cert_pem).unwrap();
        let (_, x509) = x509_parser::parse_x509_certificate(&der).unwrap();
        assert!(x509.is_ca(), "expected CA basic constraint");
        let ku = x509.key_usage().unwrap().expect("key usage present");
        assert!(ku.value.key_cert_sign(), "expected keyCertSign");
    }

    #[test]
    fn load_or_generate_writes_key_0600_reuses_and_builds_authority() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let cert = dir.path().join("ca").join("rtr-ca.cert.pem");
        let key = dir.path().join("ca").join("rtr-ca.key.pem");

        let m1 = load_or_generate(&cert, &key).unwrap();
        let mode = std::fs::metadata(&key).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "key perms {mode:o}");

        // Second call reuses the same material (no regeneration).
        let m2 = load_or_generate(&cert, &key).unwrap();
        assert_eq!(m1.cert_pem, m2.cert_pem);

        // Fingerprint stable + well-formed (32 bytes -> 31 colons).
        let fp = m1.fingerprint().unwrap();
        assert_eq!(fp, m2.fingerprint().unwrap());
        assert_eq!(fp.matches(':').count(), 31);

        // The CA builds a usable hudsucker authority.
        m2.authority().unwrap();
    }
}
