use rcgen::{generate_simple_self_signed, CertifiedKey};

/// First-run self-signed certificate for localhost + 127.0.0.1.
/// Returns (cert_der, key_der_pkcs8).
pub fn ensure_self_signed() -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    let ck: CertifiedKey = generate_simple_self_signed(vec!["localhost".into(), "127.0.0.1".into()])?;
    Ok((ck.cert.der().to_vec(), ck.key_pair.serialize_der()))
}
