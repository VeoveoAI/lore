// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use lore_base::runtime::LORE_CONTEXT;
use lore_base::test_util::TempDir;
use lore_server::authnz::repository_authorizer::AllowAllRepositoryAuthorizer;
use lore_server::http::LoreHttpServer;
use lore_server::http::server::LoreHttpServerSettings;
use lore_server::tls::CertificateSettings;
use lore_transport::tls::generate_self_signed;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use crate::store::test_support::test_store_create;

fn write_certificate(dir: &Path) -> (CertificateSettings, String) {
    let generated = generate_self_signed(vec!["localhost".to_string()]).unwrap();
    let cert_file = dir.join("cert.pem");
    let pkey_file = dir.join("key.pem");
    std::fs::write(&cert_file, &generated.cert_pem).unwrap();
    std::fs::write(&pkey_file, &generated.key_pem).unwrap();
    (
        CertificateSettings {
            cert_chain: None,
            cert_file,
            pkey_file,
        },
        generated.cert_pem,
    )
}

async fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}

/// Runs `check` against a server on a free port with `certificate`, and shuts it down.
async fn with_server<F: Future<Output = ()>>(
    certificate: Option<CertificateSettings>,
    check: impl FnOnce(u16) -> F,
) {
    let (immutable_store, mutable_store, execution) = test_store_create().await.unwrap();
    LORE_CONTEXT
        .scope(execution, async move {
            let port = free_port().await;
            let settings = LoreHttpServerSettings {
                certificate,
                host: "127.0.0.1".to_string(),
                port: i32::from(port),
                ..LoreHttpServerSettings::test_default()
            };
            let (stop, stopped) = oneshot::channel::<()>();
            let server = lore_base::lore_spawn!(LoreHttpServer::serve(
                settings,
                immutable_store,
                mutable_store,
                None,
                Arc::new(AllowAllRepositoryAuthorizer),
                async move {
                    let _ = stopped.await;
                },
            ));
            check(port).await;
            let _ = stop.send(());
            tokio::time::timeout(Duration::from_secs(10), server)
                .await
                .expect("the server stops")
                .unwrap()
                .unwrap();
        })
        .await;
}

async fn health(client: &reqwest::Client, url: &str) -> reqwest::Result<reqwest::StatusCode> {
    let mut last = None;
    for _ in 0..100 {
        match client.get(url).send().await {
            Ok(response) => return Ok(response.status()),
            Err(err) => last = Some(err),
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Err(last.unwrap())
}

#[tokio::test]
async fn a_configured_certificate_serves_https_and_nothing_else() {
    let dir = TempDir::new("lore-http-tls");
    let (certificate, cert_pem) = write_certificate(dir.path());
    with_server(Some(certificate), |port| async move {
        let trusted = reqwest::Client::builder()
            .add_root_certificate(reqwest::Certificate::from_pem(cert_pem.as_bytes()).unwrap())
            .build()
            .unwrap();
        let status = health(&trusted, &format!("https://localhost:{port}/health_check"))
            .await
            .unwrap();
        assert_eq!(status, reqwest::StatusCode::OK);

        // The client that does not trust the certificate is refused by its own TLS.
        let untrusted = reqwest::Client::new()
            .get(format!("https://localhost:{port}/health_check"))
            .send()
            .await;
        assert!(untrusted.is_err(), "an untrusted certificate was accepted");

        // Plain HTTP on the port gets no answer: no token may cross it in the clear.
        let plain = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap()
            .get(format!("http://localhost:{port}/health_check"))
            .send()
            .await;
        assert!(plain.is_err(), "plain HTTP reached a TLS endpoint");
    })
    .await;
}

#[tokio::test]
async fn without_a_certificate_the_endpoint_stays_plain_http() {
    with_server(None, |port| async move {
        let status = health(
            &reqwest::Client::new(),
            &format!("http://127.0.0.1:{port}/health_check"),
        )
        .await
        .unwrap();
        assert_eq!(status, reqwest::StatusCode::OK);
    })
    .await;
}

#[tokio::test]
async fn a_certificate_that_will_not_load_stops_the_server() {
    let dir = TempDir::new("lore-http-tls");
    let (immutable_store, mutable_store, execution) = test_store_create().await.unwrap();
    LORE_CONTEXT
        .scope(execution, async move {
            let settings = LoreHttpServerSettings {
                certificate: Some(CertificateSettings {
                    cert_chain: None,
                    cert_file: dir.path().join("missing-cert.pem"),
                    pkey_file: dir.path().join("missing-key.pem"),
                }),
                host: "127.0.0.1".to_string(),
                port: i32::from(free_port().await),
                ..LoreHttpServerSettings::test_default()
            };
            let result = LoreHttpServer::serve(
                settings,
                immutable_store,
                mutable_store,
                None,
                Arc::new(AllowAllRepositoryAuthorizer),
                async {},
            )
            .await;
            let err = result.expect_err("a missing certificate must stop the server");
            assert!(err.to_string().contains("missing-cert.pem"), "{err}");
        })
        .await;
}
