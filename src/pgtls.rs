//! TLS for PostgreSQL connections (pool and LISTEN/NOTIFY), built on rustls.
//!
//! `sslmode` follows libpq semantics:
//! - `disable`: plaintext only.
//! - `prefer`: try TLS, fall back to plaintext if the server refuses it
//!   (handled by tokio-postgres); certificate not verified.
//! - `require`: TLS mandatory, certificate not verified — unless `sslrootcert`
//!   is set, in which case it behaves like `verify-ca` (as libpq does).
//! - `verify-ca`: TLS mandatory, chain verified against `sslrootcert` (or the
//!   bundled webpki roots), hostname not checked.
//! - `verify-full`: as `verify-ca`, plus the hostname must match the certificate.

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::WebPkiServerVerifier;
use rustls::crypto::{self, CryptoProvider};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme,
};
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::config::{DatabaseConfig, SslMode};

/// How the server certificate is checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verification {
    None,
    ChainOnly,
    Full,
}

fn verification_for(mode: SslMode, has_rootcert: bool) -> Verification {
    match mode {
        SslMode::Disable | SslMode::Prefer => Verification::None,
        SslMode::Require if has_rootcert => Verification::ChainOnly,
        SslMode::Require => Verification::None,
        SslMode::VerifyCa => Verification::ChainOnly,
        SslMode::VerifyFull => Verification::Full,
    }
}

/// Build the TLS connector for a database config. Used for both the
/// deadpool pool and the dedicated LISTEN connection. With `sslmode = "disable"`
/// tokio-postgres never invokes it.
pub fn make_tls_connector(db: &DatabaseConfig) -> Result<MakeRustlsConnect> {
    let provider = Arc::new(crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .context("Failed to configure TLS protocol versions")?;

    let verification = verification_for(db.sslmode, db.sslrootcert.is_some());
    let config = match verification {
        Verification::None => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerification { provider }))
            .with_no_client_auth(),
        Verification::ChainOnly | Verification::Full => {
            let roots = Arc::new(load_roots(db.sslrootcert.as_deref())?);
            let webpki = WebPkiServerVerifier::builder_with_provider(roots, provider)
                .build()
                .context("Failed to build TLS certificate verifier")?;
            if verification == Verification::Full {
                builder.with_webpki_verifier(webpki).with_no_client_auth()
            } else {
                builder
                    .dangerous()
                    .with_custom_certificate_verifier(Arc::new(ChainOnly { inner: webpki }))
                    .with_no_client_auth()
            }
        }
    };

    Ok(MakeRustlsConnect::new(config))
}

fn load_roots(sslrootcert: Option<&str>) -> Result<RootCertStore> {
    let mut store = RootCertStore::empty();
    match sslrootcert {
        Some(path) => {
            let certs = CertificateDer::pem_file_iter(path)
                .with_context(|| format!("Failed to read sslrootcert '{}'", path))?
                .collect::<std::result::Result<Vec<_>, _>>()
                .with_context(|| format!("Failed to parse PEM in sslrootcert '{}'", path))?;
            let (added, _ignored) = store.add_parsable_certificates(certs);
            if added == 0 {
                bail!("sslrootcert '{}' contains no usable certificates", path);
            }
        }
        None => store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
    }
    Ok(store)
}

/// `require`/`prefer` semantics: encrypt, but accept any server certificate.
/// Handshake signatures are still checked so the session keys are bound to
/// the presented certificate.
#[derive(Debug)]
struct NoVerification {
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for NoVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// `verify-ca` semantics: full webpki chain validation, but a hostname
/// mismatch is not an error.
#[derive(Debug)]
struct ChainOnly {
    inner: Arc<WebPkiServerVerifier>,
}

impl ServerCertVerifier for ChainOnly {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match self.inner.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        ) {
            // webpki checks the chain before the name, so a name error means
            // the chain itself was valid.
            Err(rustls::Error::InvalidCertificate(
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
            )) => Ok(ServerCertVerified::assertion()),
            other => other,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db(sslmode: SslMode, sslrootcert: Option<&str>) -> DatabaseConfig {
        DatabaseConfig {
            host: "localhost".to_string(),
            port: 5432,
            user: "postgres".to_string(),
            password: "pw".to_string(),
            dbname: "db".to_string(),
            pool_size: None,
            sslmode,
            sslrootcert: sslrootcert.map(String::from),
        }
    }

    #[test]
    fn test_verification_matrix() {
        assert_eq!(
            verification_for(SslMode::Disable, false),
            Verification::None
        );
        assert_eq!(verification_for(SslMode::Prefer, true), Verification::None);
        assert_eq!(
            verification_for(SslMode::Require, false),
            Verification::None
        );
        // libpq: require + root cert present behaves like verify-ca
        assert_eq!(
            verification_for(SslMode::Require, true),
            Verification::ChainOnly
        );
        assert_eq!(
            verification_for(SslMode::VerifyCa, false),
            Verification::ChainOnly
        );
        assert_eq!(
            verification_for(SslMode::VerifyFull, false),
            Verification::Full
        );
    }

    #[test]
    fn test_connector_builds_for_every_mode_without_rootcert() {
        for mode in [
            SslMode::Disable,
            SslMode::Prefer,
            SslMode::Require,
            SslMode::VerifyCa,
            SslMode::VerifyFull,
        ] {
            make_tls_connector(&db(mode, None))
                .unwrap_or_else(|e| panic!("mode {:?}: {:#}", mode, e));
        }
    }

    #[test]
    fn test_missing_rootcert_file_is_an_error() {
        let err = make_tls_connector(&db(
            SslMode::VerifyFull,
            Some("/nonexistent/tilefeed-root.crt"),
        ))
        .err()
        .expect("missing sslrootcert must fail");
        assert!(format!("{:#}", err).contains("sslrootcert"));
    }

    #[test]
    fn test_rootcert_without_certificates_is_an_error() {
        let path =
            std::env::temp_dir().join(format!("tilefeed-empty-root-{}.crt", std::process::id()));
        std::fs::write(&path, "not a pem file\n").unwrap();
        let err = make_tls_connector(&db(SslMode::VerifyCa, Some(path.to_str().unwrap())))
            .err()
            .expect("empty sslrootcert must fail");
        std::fs::remove_file(&path).ok();
        assert!(format!("{:#}", err).contains("no usable certificates"));
    }
}
