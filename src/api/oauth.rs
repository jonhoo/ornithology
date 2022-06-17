use anyhow::Context;
use axum::{extract::Query, routing::get, Router};
use oauth2::{HttpRequest, HttpResponse, RedirectUrl};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};

/// The data passed along with an OAuth authorization redirect.
#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub(super) enum Redirect {
    Authorized { code: String, state: String },
    Error(Error),
}

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct Error {
    #[serde(rename = "error")]
    kind: AuthErrorKind,

    /// The authorization server can optionally include a human-readable description of the
    /// error. This parameter is intended for the developer to understand the error, and is not
    /// meant to be displayed to the end user. The valid characters for this parameter are the
    /// ASCII character set except for the double quote and backslash, specifically, hex codes
    /// 20-21, 23-5B and 5D-7E.
    #[serde(rename = "error_description")]
    description: Option<String>,

    /// The server can also return a URL to a human-readable web page with information about
    /// the error. This is intended for the developer to get more information about the error,
    /// and is not meant to be displayed to the end user.
    #[serde(rename = "error_uri")]
    uri: Option<String>,

    /// If the request contained a state parameter, the error response must also include the
    /// exact value from the request. The client may use this to associate this response with
    /// the initial request.
    state: String,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "OAuth authentication flow failed")
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.kind)
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub(super) enum AuthErrorKind {
    /// The request is missing a parameter, contains an invalid parameter, includes a parameter
    /// more than once, or is otherwise invalid.
    #[serde(rename = "invalid_request")]
    InvalidRequest,
    /// The user or authorization server denied the request.
    #[serde(rename = "access_denied")]
    AccessDenied,
    /// The client is not allowed to request an authorization code using this method, for example
    /// if a confidential client attempts to use the implicit grant type.
    #[serde(rename = "unauthorized_client")]
    UnauthorizedClient,
    /// The server does not support obtaining an authorization code using this method, for example
    /// if the authorization server never implemented the implicit grant type.
    #[serde(rename = "unsupported_response_type")]
    UnsupportedResponseType,
    /// The requested scope is invalid or unknown.
    #[serde(rename = "invalid_scope")]
    InvalidScope,
    /// Tnstead of displaying a 500 Internal Server Error page to the user, the server can redirect with this error code.
    #[serde(rename = "server_error")]
    ServerError,
    /// Tf the server is undergoing maintenance, or is otherwise unavailable, this error code can be returned instead of responding with a 503 Service Unavailable status code.
    #[serde(rename = "temporarily_unavailable")]
    TemporarilyUnavailable,
}

impl fmt::Display for AuthErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthErrorKind::AccessDenied => write!(f, "you didn't allow access"),
            AuthErrorKind::InvalidRequest
            | AuthErrorKind::UnauthorizedClient
            | AuthErrorKind::UnsupportedResponseType
            | AuthErrorKind::InvalidScope => write!(f, "the code is wrong"),
            AuthErrorKind::ServerError => write!(f, "the twitter api broke"),
            AuthErrorKind::TemporarilyUnavailable => write!(f, "the twitter api is down"),
        }
    }
}

impl std::error::Error for AuthErrorKind {}

/// Starts a single-request server to receive an OAuth redirect.
///
/// `.0` is the URL to use for the redirect, and `.1` is a oneshot channel that the OAuth redirect
/// data will be sent to when the user is redirected there.
pub(super) async fn redirect_server(
) -> anyhow::Result<(RedirectUrl, tokio::sync::oneshot::Receiver<Redirect>)> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let (shut, down) = tokio::sync::oneshot::channel();

    let tx = Arc::new(Mutex::new(Some((tx, shut))));

    // build our application with a route
    let app = Router::new().route(
        "/callback",
        get({
            let tx = Arc::clone(&tx);
            |Query(token): Query<Redirect>| async move {
                let (tx, shut) = tx
                    .lock()
                    .expect("no lock poisoning")
                    .take()
                    .expect("only called once");
                let _ = tx.send(token);
                let _ = shut.send(());
                (StatusCode::CREATED, "Please return to the CLI")
            }
        }),
    );

    // XXX: would be nice to not hard-code the port here, but Twitter's redirect allow-listing
    // doesn't allow wildcard ports on localhost.
    let addr = SocketAddr::from(([127, 0, 0, 1], 8180));
    let server = axum::Server::bind(&addr).serve(app.into_make_service());
    let addr = server.local_addr();
    let redirect_addr = RedirectUrl::new(format!("http://127.0.0.1:{}/callback", addr.port()))
        .context("construct local redirect address")?;
    tokio::spawn(async move {
        if let Err(e) = server
            .with_graceful_shutdown(async move {
                down.await.ok();
            })
            .await
        {
            eprintln!("{}", e);
        }
    });

    Ok((redirect_addr, rx))
}

// This is `oauth2::reqwest::async_http_client`, except it re-uses an existing `reqwest::Client`.
pub(super) async fn async_client_request(
    client: &reqwest::Client,
    request: HttpRequest,
) -> Result<HttpResponse, oauth2::reqwest::Error<reqwest::Error>> {
    use oauth2::reqwest::Error;

    let mut request_builder = client
        .request(request.method, request.url.as_str())
        .body(request.body);
    for (name, value) in &request.headers {
        request_builder = request_builder.header(name.as_str(), value.as_bytes());
    }
    let request = request_builder.build().map_err(Error::Reqwest)?;

    let response = client.execute(request).await.map_err(Error::Reqwest)?;

    let status_code = response.status();
    let headers = response.headers().to_owned();
    let chunks = response.bytes().await.map_err(Error::Reqwest)?;
    Ok(HttpResponse {
        status_code,
        headers,
        body: chunks.to_vec(),
    })
}
