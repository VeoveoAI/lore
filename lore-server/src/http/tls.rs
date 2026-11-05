// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! TLS for the HTTP endpoint, when `[server.http.certificate]` configures it.
//!
//! The certificate is read at startup, as the gRPC endpoint reads its own. Clients are
//! not asked for certificates: these routes authorize a bearer token.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use anyhow::anyhow;
use axum::serve::Listener;
use lore_base::lore_spawn_net;
use lore_transport::tls::load_certs;
use lore_transport::tls::load_private_key;
use rustls::ServerConfig;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;
use tracing::debug;
use tracing::warn;

use crate::tls::CertificateSettings;

/// How long a client may take to finish its handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// How many finished handshakes may wait for the server to take them.
const READY_CONNECTIONS: usize = 64;
/// How long to wait before accepting again after a failed accept, such as running out of
/// file descriptors.
const ACCEPT_RETRY: Duration = Duration::from_secs(1);

/// The server's TLS configuration for `settings`: its certificate and key, and no client
/// certificates. axum serves HTTP/1.1 here, so that is all it offers.
pub fn server_config(settings: &CertificateSettings) -> Result<Arc<ServerConfig>> {
    let certificates = load_certs(&settings.cert_file)
        .map_err(|err| anyhow!("HTTP certificate {}: {err}", settings.cert_file.display()))?;
    let key = load_private_key(&settings.pkey_file)
        .map_err(|err| anyhow!("HTTP private key {}: {err}", settings.pkey_file.display()))?;
    let mut config =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|err| anyhow!("HTTP TLS configuration: {err}"))?
            .with_no_client_auth()
            .with_single_cert(certificates, key)
            .map_err(|err| anyhow!("HTTP certificate and key: {err}"))?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// A listener that hands axum connections whose TLS handshake has finished. Handshakes run on
/// their own tasks, so a slow client holds up nobody else.
pub struct TlsListener {
    ready: mpsc::Receiver<(TlsStream<TcpStream>, SocketAddr)>,
    local_addr: SocketAddr,
}

impl TlsListener {
    pub fn new(listener: TcpListener, config: Arc<ServerConfig>) -> io::Result<Self> {
        let local_addr = listener.local_addr()?;
        let (sender, ready) = mpsc::channel(READY_CONNECTIONS);
        lore_spawn_net!(accept(listener, TlsAcceptor::from(config), sender));
        Ok(Self { ready, local_addr })
    }
}

/// Accepts connections until the [`TlsListener`] is dropped.
async fn accept(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    sender: mpsc::Sender<(TlsStream<TcpStream>, SocketAddr)>,
) {
    loop {
        tokio::select! {
            () = sender.closed() => return,
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    let acceptor = acceptor.clone();
                    let sender = sender.clone();
                    lore_spawn_net!(async move {
                        match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
                            Ok(Ok(stream)) => {
                                let _ = sender.send((stream, peer)).await;
                            }
                            Ok(Err(err)) => debug!(%peer, %err, "HTTP TLS handshake failed"),
                            Err(_elapsed) => debug!(%peer, "HTTP TLS handshake timed out"),
                        }
                    });
                }
                Err(err) => {
                    warn!(%err, "HTTP accept failed");
                    tokio::time::sleep(ACCEPT_RETRY).await;
                }
            },
        }
    }
}

impl Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.ready.recv().await {
            Some(connection) => connection,
            // The accept task holds a sender until this listener is dropped, and it only
            // returns then, so this is not reached while anything is listening.
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        Ok(self.local_addr)
    }
}
