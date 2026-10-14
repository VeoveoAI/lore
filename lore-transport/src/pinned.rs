//! A token an installation pins to one Lore server.
//!
//! A sandboxed client receives its token from the installation rather than from
//! the user running it. The process presents that token to the server the
//! installation names and refuses to connect anywhere else, whatever remote a
//! repository config or command line supplies. Scheme, host and port must all
//! match.

use std::sync::OnceLock;

use url::Url;

use crate::error::ProtocolError;

static PINNED: OnceLock<Pinned> = OnceLock::new();

struct Pinned {
    scheme: String,
    host: String,
    port: Option<u16>,
    token: String,
}

impl Pinned {
    fn admits(&self, remote_url: &Url) -> bool {
        remote_url.scheme() == self.scheme
            && remote_url
                .host_str()
                .is_some_and(|host| host.eq_ignore_ascii_case(&self.host))
            && remote_url.port() == self.port
    }

    fn server(&self) -> String {
        match self.port {
            Some(port) => format!("{}://{}:{port}", self.scheme, self.host),
            None => format!("{}://{}", self.scheme, self.host),
        }
    }
}

/// Pins `token` to the server `remote` names, for the life of the process.
///
/// Fails when `remote` names no host, when the token is empty, or when this
/// process already pinned a token.
pub fn pin(remote: &str, token: String) -> Result<(), String> {
    let remote = Url::parse(remote).map_err(|err| format!("invalid pinned remote: {err}"))?;
    let Some(host) = remote.host_str() else {
        return Err("the pinned remote names no host".to_string());
    };
    if token.is_empty() {
        return Err("the pinned token is empty".to_string());
    }
    let pinned = Pinned {
        scheme: remote.scheme().to_string(),
        host: host.to_string(),
        port: remote.port(),
        token,
    };
    PINNED
        .set(pinned)
        .map_err(|_| "a token is already pinned in this process".to_string())
}

/// Whether this process carries a pinned token.
pub fn is_pinned() -> bool {
    PINNED.get().is_some()
}

/// The pinned token, which takes the place of the tokens a caller supplies.
/// Connections still open only to the pinned server.
pub fn token() -> Option<&'static str> {
    PINNED.get().map(|pinned| pinned.token.as_str())
}

/// The pinned token when `remote_url` names the pinned server, `None` when no
/// token is pinned, and an error for any other server.
pub(crate) fn token_for(remote_url: &Url) -> Result<Option<&'static str>, ProtocolError> {
    let Some(pinned) = PINNED.get() else {
        return Ok(None);
    };
    if pinned.admits(remote_url) {
        return Ok(Some(pinned.token.as_str()));
    }
    Err(ProtocolError::internal(format!(
        "this installation permits Lore connections only to {}",
        pinned.server()
    )))
}
