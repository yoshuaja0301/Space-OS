//! The lab's certificate authority and the certificates of its TLS services, made
//! fresh for every run (ADR-0017).
//!
//! The guest trusts exactly one authority: the one written to its data disk as
//! `/spaceos/tls/labca.der`. Every lab TLS service presents one of the leaves
//! below, and each leaf is wrong in at most one way, so a client that accepts it
//! has skipped exactly one check.

use std::fs;
use std::path::{Path, PathBuf};

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use time::{Duration, OffsetDateTime};

/// The name the lab's TLS services answer to.
pub const NAME: &str = "tls.lab.test";

/// File stem, subject name, validity (days from now: from, to), signed by the lab
/// authority (else self-signed).
const LEAVES: &[(&str, &str, i64, i64, bool)] = &[
    ("good", NAME, -1, 30, true),
    ("expired", NAME, -30, -1, true),
    ("wrongname", "other.lab.test", -1, 30, true),
    ("untrusted", NAME, -1, 30, false),
];

pub fn dir() -> PathBuf {
    super::root().join("build/lab-pki")
}

fn err(e: impl std::fmt::Display) -> String {
    format!("lab PKI: {e}")
}

/// Make the authority and every leaf; returns the authority's certificate (DER).
pub fn generate() -> Result<Vec<u8>, String> {
    generate_in(&dir())
}

/// [`generate`] into `d`. The authority's key is never written anywhere: nothing
/// can be signed by it once this returns.
fn generate_in(d: &Path) -> Result<Vec<u8>, String> {
    fs::create_dir_all(d).map_err(err)?;
    let now = OffsetDateTime::now_utc();
    let ca_key = KeyPair::generate().map_err(err)?;
    let mut ca = CertificateParams::new(Vec::<String>::new()).map_err(err)?;
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca.distinguished_name.push(DnType::CommonName, "Space OS lab CA");
    ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    ca.not_before = now - Duration::days(1);
    ca.not_after = now + Duration::days(365);
    let ca_cert = ca.self_signed(&ca_key).map_err(err)?;
    let issuer = Issuer::new(ca, ca_key);
    for (stem, name, from, to, by_ca) in LEAVES {
        let key = KeyPair::generate().map_err(err)?;
        let mut p = CertificateParams::new(vec![name.to_string()]).map_err(err)?;
        p.distinguished_name.push(DnType::CommonName, *name);
        p.not_before = now + Duration::days(*from);
        p.not_after = now + Duration::days(*to);
        p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let cert = if *by_ca { p.signed_by(&key, &issuer) } else { p.self_signed(&key) }.map_err(err)?;
        fs::write(d.join(format!("{stem}.der")), cert.der()).map_err(err)?;
        fs::write(d.join(format!("{stem}.key")), key.serialize_der()).map_err(err)?;
    }
    fs::write(d.join("ca.der"), ca_cert.der()).map_err(err)?;
    Ok(ca_cert.der().to_vec())
}

/// A leaf and its key, as a lab service presents it.
pub fn load(stem: &str) -> Result<(CertificateDer<'static>, PrivateKeyDer<'static>), String> {
    load_from(&dir(), stem)
}

fn load_from(d: &Path, stem: &str) -> Result<(CertificateDer<'static>, PrivateKeyDer<'static>), String> {
    let cert = fs::read(d.join(format!("{stem}.der"))).map_err(err)?;
    let key = fs::read(d.join(format!("{stem}.key"))).map_err(err)?;
    Ok((CertificateDer::from(cert), PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key))))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rustls::client::WebPkiServerVerifier;
    use rustls::client::danger::ServerCertVerifier;
    use rustls::pki_types::{ServerName, UnixTime};

    use super::*;

    /// Each leaf fails a standard verifier for exactly the reason it was made for,
    /// and the good one passes: checked on the host, with a different crypto
    /// implementation (ring) than the guest's. In a directory of its own, so a
    /// running lab keeps the certificates its guest's disk was made with.
    #[test]
    fn leaves_verify_as_intended() {
        let d = std::env::temp_dir().join(format!("spaceos-lab-pki-test-{}", std::process::id()));
        let ca = generate_in(&d).unwrap();
        let load = |stem: &str| load_from(&d, stem);
        let mut roots = rustls::RootCertStore::empty();
        roots.add(CertificateDer::from(ca)).unwrap();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let v = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider).build().unwrap();
        let name = ServerName::try_from(NAME).unwrap();
        let check = |stem: &str| {
            let (cert, _) = load(stem).unwrap();
            v.verify_server_cert(&cert, &[], &name, &[], UnixTime::now()).map(|_| ())
        };
        assert!(check("good").is_ok());
        let e = |stem: &str| format!("{:?}", check(stem).unwrap_err());
        assert!(e("expired").contains("Expired"), "{}", e("expired"));
        assert!(e("wrongname").contains("NotValidForName"), "{}", e("wrongname"));
        assert!(e("untrusted").contains("UnknownIssuer"), "{}", e("untrusted"));
        fs::remove_dir_all(&d).ok();
    }
}
