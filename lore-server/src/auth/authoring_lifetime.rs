// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
//! Hard expiry for scoped RPCs, including idle bidirectional storage sessions.
use bytes::Bytes;
use http::{Request, Response};
use http_body::{Body, Frame, SizeHint};
use std::{
    pin::Pin,
    task::{Context, Poll},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tonic::{Status, body::Body as TonicBody, server::NamedService};
use tower::Service;

use super::{authoring, jwt::AuthorizationToken};

struct ExpiringBody<B> {
    inner: Pin<Box<B>>,
    expires: Pin<Box<tokio::time::Sleep>>,
}
impl<B: Body<Data = Bytes>> Body for ExpiringBody<B> {
    type Data = Bytes;
    type Error = Status;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Status>>> {
        if self.expires.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Some(Err(Status::unauthenticated(
                "Authoring grant expired",
            ))));
        }
        self.inner.as_mut().poll_frame(cx).map(|frame| {
            frame.map(|frame| frame.map_err(|_| Status::cancelled("Scoped stream closed")))
        })
    }
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[derive(Clone)]
pub struct AuthoringLifetimeService<S>(pub S);

impl<S, B> Service<Request<B>> for AuthoringLifetimeService<S>
where
    B: Body<Data = Bytes> + Send + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    S: Service<Request<TonicBody>, Response = Response<TonicBody>> + Clone + Send + 'static,
    S::Future: Send,
{
    type Response = Response<TonicBody>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready(cx)
    }
    fn call(&mut self, request: Request<B>) -> Self::Future {
        let clone = self.0.clone();
        let mut inner = std::mem::replace(&mut self.0, clone);
        Box::pin(async move {
            let expiration = match request.extensions().get::<AuthorizationToken>() {
                Some(token) => match authoring::scope(token) {
                    Ok(Some(_)) => Some(token.expires),
                    Ok(None) => None,
                    Err(status) => return Ok(status.into_http()),
                },
                None => None,
            };
            let Some(expiration) = expiration else {
                return inner.call(request.map(TonicBody::new)).await;
            };
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default();
            let remaining = Duration::from_secs(expiration).saturating_sub(now);
            let deadline = tokio::time::Instant::now() + remaining;
            let request = request.map(|body| {
                TonicBody::new(ExpiringBody {
                    inner: Box::pin(body),
                    expires: Box::pin(tokio::time::sleep_until(deadline)),
                })
            });
            let response = match tokio::time::timeout_at(deadline, inner.call(request)).await {
                Ok(response) => response?,
                Err(_) => return Ok(Status::unauthenticated("Authoring grant expired").into_http()),
            };
            Ok(response.map(|body| {
                TonicBody::new(ExpiringBody {
                    inner: Box::pin(body),
                    expires: Box::pin(tokio::time::sleep_until(deadline)),
                })
            }))
        })
    }
}
impl<S: NamedService> NamedService for AuthoringLifetimeService<S> {
    const NAME: &'static str = S::NAME;
}
