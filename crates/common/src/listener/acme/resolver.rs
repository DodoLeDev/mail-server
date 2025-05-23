/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs Ltd <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

use std::sync::Arc;

use rustls::{
    crypto::ring::sign::any_ecdsa_type,
    server::{ClientHello, ResolvesServerCert},
    sign::CertifiedKey,
    ServerConfig,
};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use store::{dispatch::lookup::KeyValue, write::Bincode};
use trc::AcmeEvent;

use crate::{listener::acme::directory::SerializedCert, Server, KV_ACME};

use super::{directory::ACME_TLS_ALPN_NAME, AcmeProvider, StaticResolver};

impl Server {
    pub(crate) fn set_cert(&self, provider: &AcmeProvider, cert: Arc<CertifiedKey>) {
        // Add certificates
        let mut certificates = self.inner.data.tls_certificates.load().as_ref().clone();
        for domain in provider.domains.iter() {
            certificates.insert(
                domain
                    .strip_prefix("*.")
                    .unwrap_or(domain.as_str())
                    .to_string(),
                cert.clone(),
            );
        }

        // Add default certificate
        if provider.default {
            certificates.insert("*".to_string(), cert);
        }

        self.inner.data.tls_certificates.store(certificates.into());
    }

    pub(crate) async fn build_acme_certificate(&self, domain: &str) -> Option<Arc<CertifiedKey>> {
        match self
            .in_memory_store()
            .key_get::<Bincode<SerializedCert>>(KeyValue::<()>::build_key(KV_ACME, domain))
            .await
        {
            Ok(Some(cert)) => {
                match any_ecdsa_type(&PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
                    cert.inner.private_key,
                ))) {
                    Ok(key) => Some(Arc::new(CertifiedKey::new(
                        vec![CertificateDer::from(cert.inner.certificate)],
                        key,
                    ))),
                    Err(err) => {
                        trc::event!(
                            Acme(AcmeEvent::Error),
                            Domain = domain.to_string(),
                            Reason = err.to_string(),
                            Details = "Failed to parse private key"
                        );
                        None
                    }
                }
            }
            Err(err) => {
                trc::event!(
                    Acme(AcmeEvent::Error),
                    Domain = domain.to_string(),
                    CausedBy = err
                );
                None
            }
            Ok(None) => {
                trc::event!(Acme(AcmeEvent::TokenNotFound), Domain = domain.to_string());
                None
            }
        }
    }
}

impl ResolvesServerCert for StaticResolver {
    fn resolve(&self, _: ClientHello) -> Option<Arc<CertifiedKey>> {
        self.key.clone()
    }
}

pub(crate) fn build_acme_static_resolver(key: Option<Arc<CertifiedKey>>) -> Arc<ServerConfig> {
    let mut challenge = ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(StaticResolver { key }));
    challenge.alpn_protocols.push(ACME_TLS_ALPN_NAME.to_vec());
    Arc::new(challenge)
}

pub trait IsTlsAlpnChallenge {
    fn is_tls_alpn_challenge(&self) -> bool;
}

impl IsTlsAlpnChallenge for ClientHello<'_> {
    fn is_tls_alpn_challenge(&self) -> bool {
        self.alpn().into_iter().flatten().eq([ACME_TLS_ALPN_NAME])
    }
}
