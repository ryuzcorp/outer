//! TLS termination with Cloudflare origin pull (mTLS).
//!
//! When the `OUTER_TLS_CERT` / `OUTER_TLS_KEY` / `OUTER_ORIGIN_PULL_CA`
//! triple is set, the listener serves HTTPS and REQUIRES a client certificate
//! chaining to the origin-pull CA. Config validation only checks the triple
//! is present and consistent; reading and parsing the PEM files happens here,
//! at serve time, with errors that name the offending variable.

use std::path::PathBuf;
use std::sync::Arc;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

/// The three PEM files that make up TLS termination. All three or none:
/// [`crate::config::Config::validate`] enforces that before this is built.
#[derive(Debug, Clone)]
pub struct TlsFiles {
    /// Origin-cert chain (`OUTER_TLS_CERT`).
    pub cert: PathBuf,
    /// Origin-cert private key (`OUTER_TLS_KEY`).
    pub key: PathBuf,
    /// Origin-pull client CA (`OUTER_ORIGIN_PULL_CA`).
    pub ca: PathBuf,
}

/// Read the triple off disk and build a server config that requires a client
/// certificate chaining to the origin-pull CA. Every failure names the
/// variable whose file could not be read or parsed. The crypto provider is
/// passed explicitly (aws-lc-rs, the one reqwest's `rustls` feature already
/// pulls), so nothing depends on rustls' process-wide default.
pub fn load_server_config(files: &TlsFiles) -> anyhow::Result<rustls::ServerConfig> {
    let cert_pem = std::fs::read(&files.cert)
        .map_err(|err| anyhow::anyhow!("OUTER_TLS_CERT={:?}: {err}", files.cert))?;
    let key_pem = std::fs::read(&files.key)
        .map_err(|err| anyhow::anyhow!("OUTER_TLS_KEY={:?}: {err}", files.key))?;
    let ca_pem = std::fs::read(&files.ca)
        .map_err(|err| anyhow::anyhow!("OUTER_ORIGIN_PULL_CA={:?}: {err}", files.ca))?;

    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(&cert_pem)
        .collect::<Result<_, _>>()
        .map_err(|err| anyhow::anyhow!("OUTER_TLS_CERT={:?}: {err}", files.cert))?;
    if certs.is_empty() {
        anyhow::bail!("OUTER_TLS_CERT={:?}: no certificates found", files.cert);
    }
    let key = PrivateKeyDer::from_pem_slice(&key_pem)
        .map_err(|err| anyhow::anyhow!("OUTER_TLS_KEY={:?}: {err}", files.key))?;

    let mut roots = rustls::RootCertStore::empty();
    for cert in CertificateDer::pem_slice_iter(&ca_pem) {
        let cert =
            cert.map_err(|err| anyhow::anyhow!("OUTER_ORIGIN_PULL_CA={:?}: {err}", files.ca))?;
        roots
            .add(cert)
            .map_err(|err| anyhow::anyhow!("OUTER_ORIGIN_PULL_CA={:?}: {err}", files.ca))?;
    }
    if roots.is_empty() {
        anyhow::bail!("OUTER_ORIGIN_PULL_CA={:?}: no certificates found", files.ca);
    }
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
        Arc::new(roots),
        provider.clone(),
    )
    .build()
    .map_err(|err| anyhow::anyhow!("OUTER_ORIGIN_PULL_CA={:?}: {err}", files.ca))?;

    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .map_err(|err| anyhow::anyhow!("TLS config: {err}"))?
        .with_client_cert_verifier(verifier)
        .with_single_cert(certs, key)
        .map_err(|err| anyhow::anyhow!("OUTER_TLS_CERT/OUTER_TLS_KEY: {err}"))?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    use axum::routing::get;

    struct TestPki {
        ca_pem: Vec<u8>,
        server_cert_pem: Vec<u8>,
        server_key_pem: Vec<u8>,
        client_cert_pem: Vec<u8>,
        client_key_pem: Vec<u8>,
        wrong_cert_pem: Vec<u8>,
        wrong_key_pem: Vec<u8>,
    }

    fn issuer(common_name: &str) -> (rcgen::Certificate, rcgen::Issuer<'static, rcgen::KeyPair>) {
        let mut params = rcgen::CertificateParams::new(vec![common_name.to_string()]).unwrap();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        let issuer = rcgen::Issuer::new(params, key);
        (cert, issuer)
    }

    fn leaf(
        issuer: &rcgen::Issuer<'_, rcgen::KeyPair>,
        sans: Vec<String>,
        ip: Option<IpAddr>,
    ) -> (Vec<u8>, Vec<u8>) {
        let mut params = rcgen::CertificateParams::new(sans).unwrap();
        if let Some(ip) = ip {
            params.subject_alt_names.push(rcgen::SanType::IpAddress(ip));
        }
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.signed_by(&key, issuer).unwrap();
        (cert.pem().into_bytes(), key.serialize_pem().into_bytes())
    }

    /// One origin-pull CA (server cert, good client cert) plus an unrelated
    /// CA for the wrong client cert.
    fn test_pki() -> TestPki {
        let (ca_cert, ca) = issuer("outer test origin-pull CA");
        let (_, other) = issuer("someone else's CA");
        let (server_cert_pem, server_key_pem) = leaf(
            &ca,
            vec!["outer.test".to_string()],
            Some(IpAddr::from([127, 0, 0, 1])),
        );
        let (client_cert_pem, client_key_pem) =
            leaf(&ca, vec!["cloudflare.test".to_string()], None);
        let (wrong_cert_pem, wrong_key_pem) =
            leaf(&other, vec!["cloudflare.test".to_string()], None);
        TestPki {
            ca_pem: ca_cert.pem().into_bytes(),
            server_cert_pem,
            server_key_pem,
            client_cert_pem,
            client_key_pem,
            wrong_cert_pem,
            wrong_key_pem,
        }
    }

    #[test]
    fn missing_and_malformed_pem_files_name_the_variable() {
        let dir = tempfile::tempdir().unwrap();
        let files = TlsFiles {
            cert: dir.path().join("cert.pem"),
            key: dir.path().join("key.pem"),
            ca: dir.path().join("origin-pull-ca.pem"),
        };
        let pki = test_pki();
        let err = format!("{:?}", load_server_config(&files).unwrap_err());
        assert!(err.contains("OUTER_TLS_CERT"), "{err}");

        std::fs::write(&files.cert, "not pem at all\n").unwrap();
        let err = format!("{:?}", load_server_config(&files).unwrap_err());
        assert!(err.contains("OUTER_TLS_KEY"), "{err}");

        std::fs::write(&files.key, "not pem at all\n").unwrap();
        let err = format!("{:?}", load_server_config(&files).unwrap_err());
        assert!(err.contains("OUTER_ORIGIN_PULL_CA"), "{err}");

        std::fs::write(&files.cert, &pki.server_cert_pem).unwrap();
        std::fs::write(&files.key, &pki.server_key_pem).unwrap();
        std::fs::write(&files.ca, "not pem at all\n").unwrap();
        let err = format!("{:?}", load_server_config(&files).unwrap_err());
        assert!(err.contains("OUTER_ORIGIN_PULL_CA"), "{err}");

        std::fs::write(&files.ca, &pki.ca_pem).unwrap();
        std::fs::write(&files.cert, "not pem at all\n").unwrap();
        let err = format!("{:?}", load_server_config(&files).unwrap_err());
        assert!(err.contains("OUTER_TLS_CERT"), "{err}");

        std::fs::write(&files.cert, &pki.server_cert_pem).unwrap();
        std::fs::write(&files.key, "not pem at all\n").unwrap();
        let err = format!("{:?}", load_server_config(&files).unwrap_err());
        assert!(err.contains("OUTER_TLS_KEY"), "{err}");
    }

    fn write_files(pki: &TestPki, dir: &std::path::Path) -> TlsFiles {
        let cert = dir.join("cert.pem");
        let key = dir.join("key.pem");
        let ca = dir.join("origin-pull-ca.pem");
        std::fs::write(&cert, &pki.server_cert_pem).unwrap();
        std::fs::write(&key, &pki.server_key_pem).unwrap();
        std::fs::write(&ca, &pki.ca_pem).unwrap();
        TlsFiles { cert, key, ca }
    }

    fn https_client(ca_pem: &[u8], identity: Option<(&[u8], &[u8])>) -> reqwest::Client {
        let mut builder = reqwest::Client::builder()
            .add_root_certificate(reqwest::Certificate::from_pem(ca_pem).unwrap());
        if let Some((cert, key)) = identity {
            let pem = [cert, key].concat();
            builder = builder.identity(reqwest::Identity::from_pem(&pem).unwrap());
        }
        builder.build().unwrap()
    }

    /// The full origin-pull round trip against a real TLS listener: a client
    /// with the right cert gets 200, clients with no cert or a wrong-CA cert
    /// fail the handshake, and the server keeps serving good clients after
    /// the bad handshakes.
    #[tokio::test]
    async fn origin_pull_round_trip() {
        let pki = test_pki();
        let dir = tempfile::tempdir().unwrap();
        let server_config = load_server_config(&write_files(&pki, dir.path())).unwrap();

        let app = axum::Router::new().route("/", get(|| async { "ok" }));
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = std_listener.local_addr().unwrap();
        std_listener.set_nonblocking(true).unwrap();
        let rustls_config =
            axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(server_config));
        let server = axum_server::tls_rustls::from_tcp_rustls(std_listener, rustls_config)
            .unwrap()
            .serve(app.into_make_service());
        let _server = tokio::spawn(server);
        let url = format!("https://{addr}/");

        let good = https_client(
            &pki.ca_pem,
            Some((&pki.client_cert_pem, &pki.client_key_pem)),
        );
        let res = good.get(&url).send().await.unwrap();
        assert_eq!(res.status(), 200);

        let no_cert = https_client(&pki.ca_pem, None);
        assert!(
            no_cert.get(&url).send().await.is_err(),
            "a client with no certificate must fail the handshake"
        );

        let wrong_ca = https_client(&pki.ca_pem, Some((&pki.wrong_cert_pem, &pki.wrong_key_pem)));
        assert!(
            wrong_ca.get(&url).send().await.is_err(),
            "a client certified by another CA must fail the handshake"
        );

        let res = good.get(&url).send().await.unwrap();
        assert_eq!(res.status(), 200, "the server survives bad handshakes");
    }
}
