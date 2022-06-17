use anyhow::Context;
use futures_util::StreamExt;
use indicatif::ProgressBar;
use oauth2::basic::BasicClient;
use oauth2::{AuthUrl, AuthorizationCode, CsrfToken, PkceCodeChallenge, Scope, TokenUrl};
use serde::{Deserialize, Serialize};
use tower::{Service, ServiceExt};

mod oauth;

/// A client that knows how to authenticate Twitter API requests.
#[derive(Debug, Clone)]
struct RawClient {
    http: reqwest::Client,
    oauth:
        oauth2::StandardTokenResponse<oauth2::EmptyExtraTokenFields, oauth2::basic::BasicTokenType>,
}

impl RawClient {
    async fn new(client_id: oauth2::ClientId) -> anyhow::Result<Self> {
        // Stand up a localhost server to receive the OAuth redirect.
        let (redirect, auth) = oauth::redirect_server()
            .await
            .context("start auth callback server")?;

        // OAuth time!
        let client = BasicClient::new(
            client_id.clone(),
            None,
            AuthUrl::new("https://twitter.com/i/oauth2/authorize".to_string())?,
            Some(TokenUrl::new(
                "https://api.twitter.com/2/oauth2/token".to_string(),
            )?),
        )
        .set_redirect_uri(redirect);
        let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
        let (auth_url, csrf_token) = client
            .authorize_url(|| CsrfToken::new_random())
            .add_scope(Scope::new("tweet.read".to_string()))
            .add_scope(Scope::new("users.read".to_string()))
            .add_scope(Scope::new("follows.read".to_string()))
            .set_pkce_challenge(pkce_challenge)
            .url();

        // Now the user needs to auth us, so open that in their browser. Once they click authorize
        // (or not), the localhost webserver will catch the redirect and we'll have the
        // authorization code that we can then exchange for a token.
        open::that(auth_url.to_string()).context("forward to Twitter for authorization")?;
        let authorization_code = match auth.await.context("oauth callback is called")? {
            oauth::Redirect::Authorized { state, .. } if &*state != csrf_token.secret() => {
                anyhow::bail!("bad csrf token")
            }
            oauth::Redirect::Authorized { code, .. } => code,
            oauth::Redirect::Error(e) => anyhow::bail!(e),
        };

        // Exchange the one-time auth code for a longer-lived multi-use auth token.
        // XXX: refresh token after 2h? request offline.access? spawn that?
        // https://developer.twitter.com/en/docs/authentication/oauth-2-0/user-access-token
        let http = reqwest::Client::new();
        let token_response = client
            .exchange_code(AuthorizationCode::new(authorization_code))
            .set_pkce_verifier(pkce_verifier)
            // Twitter's API requires we supply this.
            .add_extra_param("client_id", client_id.as_str())
            .request_async(|req| oauth::async_client_request(&http, req))
            .await;
        let token = match token_response {
            Ok(token) => Ok(token),
            Err(oauth2::RequestTokenError::ServerResponse(r)) => {
                let e = Err(anyhow::anyhow!(r.error().clone()));
                match (r.error_description(), r.error_uri()) {
                    (Some(desc), Some(url)) => {
                        e.context(url.to_string()).context(desc.to_string()).into()
                    }
                    (Some(desc), None) => e.context(desc.to_string()).into(),
                    (None, Some(url)) => e.context(url.to_string()).into(),
                    (None, None) => e,
                }
            }
            Err(e) => Err(anyhow::anyhow!(e)),
        }
        .context("exchange oauth code")?;

        Ok(Self { http, oauth: token })
    }
}

impl tower::Service<reqwest::RequestBuilder> for RawClient {
    type Response = reqwest::Response;
    type Error = anyhow::Error;
    type Future =
        std::pin::Pin<Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>>>>;

    fn poll_ready(
        &mut self,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: reqwest::RequestBuilder) -> Self::Future {
        use oauth2::TokenResponse;
        let fut = req.bearer_auth(self.oauth.access_token().secret()).send();
        Box::pin(async move { Ok(fut.await.context("request")?) })
    }
}

/// Retry policy that knows to look for Twitter's special HTTP reply + header.
/// <https://developer.twitter.com/en/docs/twitter-api/rate-limits#headers-and-codes>
#[derive(Copy, Clone)]
struct TwitterRateLimitPolicy;
impl tower::retry::Policy<reqwest::RequestBuilder, reqwest::Response, anyhow::Error>
    for TwitterRateLimitPolicy
{
    type Future = std::pin::Pin<Box<dyn std::future::Future<Output = Self>>>;

    fn retry(
        &self,
        _: &reqwest::RequestBuilder,
        result: Result<&reqwest::Response, &anyhow::Error>,
    ) -> Option<Self::Future> {
        let r = match result {
            Err(_) => return None,
            Ok(r) if r.status() == reqwest::StatusCode::TOO_MANY_REQUESTS => r,
            Ok(_) => return None,
        };

        let reset = r
            .headers()
            .get("x-rate-limit-reset")
            .expect("Twitter promised");
        let reset: u64 = reset
            .to_str()
            .expect("x-rate-limit-reset as str")
            .parse()
            .expect("x-rate-limit-reset is a number");
        let time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(reset);

        Some(Box::pin(async move {
            match time.duration_since(std::time::SystemTime::now()) {
                Ok(d) if d.as_secs() > 1 => {
                    tokio::time::sleep(d).await;
                }
                _ => {
                    // Not worth waiting -- can just retry immediately.
                }
            }
            Self
        }))
    }

    fn clone_request(&self, req: &reqwest::RequestBuilder) -> Option<reqwest::RequestBuilder> {
        req.try_clone()
    }
}

/// A Twitter API client that authenticates requests and respects rate limitations.
///
/// Note that this client does not try to proactively follow rate limits, since the limits depend
/// on the endpoint, and this is generic over all endpoints. It's up to the caller (see
/// page_at_a_time! for an instance of this) to wrap the inner service in an appropriate-limited
/// `tower::limit::RateLimit` as needed for repeated requests.
pub struct Client(tower::retry::Retry<TwitterRateLimitPolicy, RawClient>);

macro_rules! page_at_a_time {
    ($this:ident, $msg:expr, $ids:ident, $pagesize:literal, $rate:expr, $url:literal, $t:ty) => {{
        let mut ids = $ids.into_iter();
        let n = ids.len();
        let bar = ProgressBar::new(n as u64)
            .with_style(
                indicatif::ProgressStyle::default_bar()
                    .template("{msg:>15} {bar:40} {percent:>3}% [{elapsed}]"),
            )
            .with_message($msg);
        let mut all: Vec<$t> = Vec::with_capacity(n);
        let mut svc = tower::limit::RateLimit::new(&mut $this.0, $rate);
        let mut futs = futures_util::stream::FuturesUnordered::new();
        for page in 0.. {
            const PAGE_SIZE: usize = $pagesize;
            let i = page * PAGE_SIZE;
            if i >= n {
                break;
            }
            let mut idsstr = String::new();
            for id in (&mut ids).take(PAGE_SIZE) {
                use std::fmt::Write;
                if idsstr.is_empty() {
                    write!(&mut idsstr, "{}", id)
                } else {
                    write!(&mut idsstr, ",{}", id)
                }
                .expect("this is fine");
            }
            let url = format!($url, idsstr);
            let req = svc.get_mut().get_mut().http.get(&url);
            loop {
                // The service may not be ready until we make progress on one of the in-flight
                // requests, so make sure to drive those forward too.
                tokio::select! {
                    ready = svc.ready() => {
                        let _ = ready.context("Service::poll_ready")?;
                        break;
                    }
                    chunk = futs.next(), if !futs.is_empty() => {
                        let chunk: anyhow::Result<Vec<$t>> = chunk.expect("!futs.is_empty()");
                        all.extend(chunk.context("grab next chunk")?);
                    }
                };
            }
            let res = tower::Service::call(&mut svc, req);
            let bar = bar.clone();
            futs.push(async move {
                let data: Vec<$t> = Self::parse(
                    res.await
                        .with_context(|| format!("Service::call('{}')", url))?,
                )
                .await
                .with_context(|| format!("parse('{}')", url))?
                .0;
                bar.inc(data.len() as u64);
                Ok(data)
            });
        }

        while let Some(chunk) = futs.next().await.transpose().context("grab chunks")? {
            all.extend(chunk);
        }
        bar.finish();

        Ok(all)
    }};
}

impl Client {
    pub async fn new(client_id: oauth2::ClientId) -> anyhow::Result<Self> {
        RawClient::new(client_id)
            .await
            .map(|svc| tower::retry::Retry::new(TwitterRateLimitPolicy, svc))
            .map(Self)
    }

    pub async fn whoami(&mut self) -> anyhow::Result<WhoAmI> {
        let req = self
            .0
            .get_mut()
            .http
            .get("https://api.twitter.com/2/users/me");
        let data: WhoAmI = Self::parse(
            self.0
                .ready()
                .await
                .context("Service::poll_ready")?
                .call(req)
                .await
                .context("Service::call")?,
        )
        .await
        .context("parse whoami")?
        .0;
        Ok(data)
    }

    pub async fn tweets<I>(&mut self, ids: I) -> anyhow::Result<Vec<Tweet>>
    where
        I: IntoIterator<Item = u64>,
        I::IntoIter: ExactSizeIterator,
    {
        page_at_a_time!(
            self,
            "Fetch tweets",
            ids,
            100,
            tower::limit::rate::Rate::new(900, std::time::Duration::from_secs(15 * 60)),
            "https://api.twitter.com/2/tweets?tweet.fields=id,created_at,public_metrics&ids={}",
            Tweet
        )
    }

    pub async fn users<I>(&mut self, ids: I) -> anyhow::Result<Vec<User>>
    where
        I: IntoIterator<Item = u64>,
        I::IntoIter: ExactSizeIterator,
    {
        page_at_a_time!(
            self,
            "Fetch followers",
            ids,
            100,
            tower::limit::rate::Rate::new(900, std::time::Duration::from_secs(15 * 60)),
            "https://api.twitter.com/2/users?user.fields=username,public_metrics&ids={}",
            User
        )
    }

    async fn parse<T>(res: reqwest::Response) -> anyhow::Result<(T, Option<Meta>)>
    where
        T: serde::de::DeserializeOwned,
    {
        // This is th general structure of all Twitter API responses.
        #[derive(Debug, Deserialize)]
        struct Data<T> {
            data: T,
            meta: Option<Meta>,
        }

        // We _could_ do:
        // let data: Data<T> = res.json().await.context("parse")?;
        // but that would make for unhelpful error messages if parsing fails, so we do:
        let data = res.text().await.context("get body")?;
        let data: Data<T> = serde_json::from_str(&data)
            .with_context(|| data)
            .context("parse")?;
        Ok((data.data, data.meta))
    }
}

/*
pub async fn from_pages<T, TT, TR, F, FT, C>(
    http: &reqwest::Client,
    token: &TR,
    url: impl Into<url::Url>,
    mut map: F,
) -> anyhow::Result<C>
where
    T: serde::de::DeserializeOwned,
    TT: oauth2::TokenType,
    TR: oauth2::TokenResponse<TT>,
    F: FnMut(T, &Meta) -> FT,
    C: Default + Extend<FT>,
{
    let mut all: C = Default::default();
    let mut next = None::<String>;
    let url = url.into();
    loop {
        let url = match next.as_ref() {
            None => url.clone(),
            Some(p) => {
                let mut url = url.clone();
                url.query_pairs_mut().append_pair("pagination_token", p);
                url
            }
        };

        let (page, meta): (Vec<T>, _) = grab(http, token, url.to_string())
            .await
            .context("followers")?;
        let meta = meta.expect("always meta for this");

        assert_eq!(page.len(), meta.results);
        all.extend(page.into_iter().map(|t| (map)(t, &meta)));
        if meta.next.is_none() {
            break;
        }
        next = meta.next;
    }
    Ok(all)
}
*/

#[derive(Debug, Deserialize)]
pub struct WhoAmI {
    pub id: String,
    pub username: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PublicTweetMetrics {
    #[serde(rename = "retweet_count")]
    pub retweets: usize,
    #[serde(rename = "reply_count")]
    pub replies: usize,
    #[serde(rename = "like_count")]
    pub likes: usize,
    #[serde(rename = "quote_count")]
    pub quotations: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Tweet {
    #[serde(rename = "id", with = "u64_but_str")]
    pub id: u64,
    #[serde(rename = "created_at", with = "time::serde::rfc3339")]
    pub created: time::OffsetDateTime,
    #[serde(rename = "public_metrics")]
    pub metrics: PublicTweetMetrics,
    // not reading in text: String here
    // would be great to read non_public_metrics, but those aren't available >30 days
}

impl Tweet {
    pub fn goodness(&self) -> usize {
        self.metrics.likes
            + 2 * self.metrics.retweets
            + 3 * self.metrics.quotations
            + self.metrics.replies / 2
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PublicUserMetrics {
    #[serde(rename = "followers_count")]
    pub followers: usize,
    #[serde(rename = "following_count")]
    pub following: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct User {
    pub username: String,
    #[serde(rename = "public_metrics")]
    pub metrics: PublicUserMetrics,
}

// Keeping this around for if I ever need to add pagination.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct Meta {
    #[serde(rename = "result_count")]
    results: usize,
    #[serde(rename = "next_token")]
    next: Option<String>,
}

mod u64_but_str {
    use std::fmt::Display;

    use serde::{de, Deserialize, Deserializer, Serializer};

    pub fn serialize<T, S>(value: &T, serializer: S) -> Result<S::Ok, S::Error>
    where
        T: Display,
        S: Serializer,
    {
        serializer.collect_str(value)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<u64, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(de::Error::custom)
    }
}
