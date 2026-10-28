//! HTTP authorization owns secrets outside the session's durable tool graph.

use std::collections::{BTreeMap, BTreeSet};
use std::future::poll_fn;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine;
use ext::{Cx, Tool};
use llm::Http;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::OAUTH_CLIENT_ID;

#[derive(Clone)]
pub(crate) struct Registration {
    pub id: String,
    pub issuer: String,
}

impl Registration {
    pub fn new(id: &str, issuer: &str) -> std::io::Result<Self> {
        if id.is_empty() || endpoint(issuer)?.query().is_some() {
            return Err(failure("invalid pre-registered client id or issuer"));
        }
        Ok(Self {
            id: id.into(),
            issuer: issuer.into(),
        })
    }
}

pub(crate) struct Client {
    pub inner: Arc<dyn Http>,
    pub cx: Cx,
    pub registration: Option<Registration>,
    pub state: Arc<State>,
}

#[derive(Default)]
pub(super) struct State(Mutex<BTreeMap<String, Arc<Credential>>>);

// No Debug: the token endpoint response is a secret, including refresh tokens.
#[derive(Serialize, Deserialize)]
struct Credential {
    issuer: String,
    resource: String,
    client_id: String,
    scope: String,
    access_token: String,
    refresh_token: Option<String>,
}

#[derive(Deserialize)]
struct ProtectedResource {
    resource: String,
    authorization_servers: Vec<String>,
    scopes_supported: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct Server {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    code_challenge_methods_supported: Vec<String>,
    #[serde(default)]
    client_id_metadata_document_supported: bool,
    #[serde(default)]
    authorization_response_iss_parameter_supported: bool,
}

struct Authorization<'a> {
    client: &'a dyn Http,
    cx: &'a Cx,
    runtime: &'a dyn rt::Runtime,
    resource: String,
    registration: Option<&'a Registration>,
    store: sys::credentials::Credentials,
    state: &'a State,
}

impl Http for Client {
    fn send(
        &self,
        request: http::Request<llm::http::Payload>,
    ) -> llm::http::BoxFuture<'static, std::io::Result<llm::http::Response>> {
        let inner = Arc::clone(&self.inner);
        let cx = self.cx.clone();
        let registration = self.registration.clone();
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            let resource = request.uri().to_string();
            let mut credential = crate::lock(&state.0).get(&resource).cloned();
            let mut loaded = credential.is_some();
            let mut authorizations = 0;
            let (parts, body) = request.into_parts();
            loop {
                let mut request = http::Request::builder()
                    .method(parts.method.clone())
                    .uri(parts.uri.clone())
                    .version(parts.version);
                *request.headers_mut().expect("valid HTTP request") = parts.headers.clone();
                if let Some(credential) = &credential {
                    let mut header =
                        http::HeaderValue::from_str(&format!("Bearer {}", credential.access_token))
                            .map_err(|_| failure("invalid access token"))?;
                    header.set_sensitive(true);
                    request
                        .headers_mut()
                        .expect("valid HTTP request")
                        .insert(http::header::AUTHORIZATION, header);
                }
                let response = inner
                    .send(
                        request
                            .body(body.clone())
                            .map_err(|_| failure("invalid request"))?,
                    )
                    .await?;
                let status = response.status();
                if status != http::StatusCode::UNAUTHORIZED && status != http::StatusCode::FORBIDDEN
                {
                    return Ok(response);
                }
                let challenge = challenge(response.headers())?;
                if status == http::StatusCode::FORBIDDEN
                    && challenge.get("error").map(String::as_str) != Some("insufficient_scope")
                {
                    return Ok(response);
                }
                drop(response);
                let authorization = Authorization {
                    client: &*inner,
                    cx: &cx,
                    runtime: cx
                        .service::<dyn rt::Runtime>()
                        .ok_or_else(|| failure("no runtime"))?,
                    resource: resource.clone(),
                    registration: registration.as_ref(),
                    store: match cx.service::<sys::credentials::Credentials>() {
                        Some(store) => store.clone(),
                        None => sys::credentials::Credentials::from_env()?,
                    },
                    state: &state,
                };
                if !loaded {
                    loaded = true;
                    credential = authorization.load().await?;
                    if credential.is_some() {
                        continue;
                    }
                }
                if authorizations == 2 {
                    return Err(failure("authorization rejected after retry"));
                }
                credential = Some(authorization.renew(status, &challenge, credential).await?);
                authorizations += 1;
            }
        })
    }
}

impl Authorization<'_> {
    async fn load(&self) -> std::io::Result<Option<Arc<Credential>>> {
        let store = self.store.clone();
        let resource = self.resource.clone();
        let saved = rt::unblock(self.runtime, move || store.load(&resource)).await?;
        let credential: Option<Credential> = saved
            .map(|bytes| {
                serde_json::from_slice(&bytes).map_err(|_| failure("invalid stored credential"))
            })
            .transpose()?;
        if credential
            .as_ref()
            .is_some_and(|c| c.resource != self.resource)
        {
            return Err(failure("stored resource mismatch"));
        }
        let credential = credential.map(Arc::new);
        if let Some(credential) = &credential {
            crate::lock(&self.state.0).insert(self.resource.clone(), Arc::clone(credential));
        }
        Ok(credential)
    }

    async fn save(&self, credential: Option<&Credential>) -> std::io::Result<()> {
        let encoded = credential
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|_| failure("credential encoding failed"))?;
        let store = self.store.clone();
        let key = self.resource.clone();
        rt::unblock(self.runtime, move || store.save(&key, encoded.as_deref())).await?;
        if credential.is_none() {
            crate::lock(&self.state.0).remove(&self.resource);
        }
        Ok(())
    }

    async fn renew(
        &self,
        status: http::StatusCode,
        challenge: &BTreeMap<String, String>,
        previous: Option<Arc<Credential>>,
    ) -> std::io::Result<Arc<Credential>> {
        let (resource_metadata, server) =
            discover(self.client, &self.resource, challenge, self.registration).await?;
        let client_id = if let Some(registration) = self.registration {
            if registration.issuer != server.issuer {
                return Err(failure("pre-registered issuer mismatch"));
            }
            registration.id.as_str()
        } else {
            if !server.client_id_metadata_document_supported {
                return Err(failure("server requires a pre-registered client id"));
            }
            OAUTH_CLIENT_ID
        };
        let previous = previous.filter(|c| c.issuer == server.issuer && c.client_id == client_id);
        let scopes = challenge.get("scope").cloned().unwrap_or_else(|| {
            resource_metadata
                .scopes_supported
                .unwrap_or_default()
                .join(" ")
        });
        let scopes = scopes
            .split_whitespace()
            .chain(previous.iter().flat_map(|c| c.scope.split_whitespace()))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
            .join(" ");
        let refreshed = if status == http::StatusCode::UNAUTHORIZED {
            if let Some(refresh) = previous.as_ref().and_then(|c| c.refresh_token.as_deref()) {
                token(
                    self.client,
                    &server,
                    &self.resource,
                    client_id,
                    &[("grant_type", "refresh_token"), ("refresh_token", refresh)],
                )
                .await?
            } else {
                None
            }
        } else {
            None
        };
        let refresh_token = refreshed
            .as_ref()
            .and(previous.as_ref())
            .and_then(|c| c.refresh_token.clone());
        let fresh = if let Some(fresh) = refreshed {
            fresh
        } else {
            // A revoked credential must not survive a cancelled new authorization.
            self.save(None).await?;
            authorize(
                self.client,
                self.cx,
                &server,
                &self.resource,
                client_id,
                &scopes,
            )
            .await?
        };
        let credential = Credential {
            issuer: server.issuer,
            resource: self.resource.clone(),
            client_id: client_id.to_owned(),
            scope: fresh.scope.unwrap_or(scopes),
            access_token: fresh.access_token,
            refresh_token: fresh.refresh_token.or(refresh_token),
        };
        self.save(Some(&credential)).await?;
        let credential = Arc::new(credential);
        crate::lock(&self.state.0).insert(self.resource.clone(), Arc::clone(&credential));
        Ok(credential)
    }
}

fn challenge(headers: &http::HeaderMap) -> std::io::Result<BTreeMap<String, String>> {
    for header in headers.get_all(http::header::WWW_AUTHENTICATE) {
        let header = header
            .to_str()
            .map_err(|_| failure("invalid authentication challenge"))?;
        for challenge in http_auth::ChallengeParser::new(header) {
            let challenge = challenge.map_err(|_| failure("invalid authentication challenge"))?;
            if challenge.scheme.eq_ignore_ascii_case("Bearer") {
                let mut params = BTreeMap::new();
                for (name, value) in challenge.params {
                    if params
                        .insert(name.to_ascii_lowercase(), value.to_unescaped())
                        .is_some()
                    {
                        return Err(failure("duplicate challenge parameter"));
                    }
                }
                return Ok(params);
            }
        }
    }
    Ok(BTreeMap::new())
}

fn endpoint(url: &str) -> std::io::Result<http::Uri> {
    let uri: http::Uri = url.parse().map_err(|_| failure("invalid OAuth URL"))?;
    let host = uri.host().ok_or_else(|| failure("OAuth URL has no host"))?;
    let loopback = host == "localhost"
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    if uri.authority().is_none_or(|a| a.as_str().contains('@'))
        || !(uri.scheme_str() == Some("https") || (uri.scheme_str() == Some("http") && loopback))
    {
        return Err(failure("OAuth requires HTTPS or loopback HTTP"));
    }
    Ok(uri)
}

async fn discover(
    client: &dyn Http,
    resource: &str,
    challenge: &BTreeMap<String, String>,
    registration: Option<&Registration>,
) -> std::io::Result<(ProtectedResource, Server)> {
    let uri = endpoint(resource)?;
    let origin = format!(
        "{}://{}",
        uri.scheme_str().expect("validated scheme"),
        uri.authority().expect("validated authority")
    );
    let urls = if let Some(url) = challenge.get("resource_metadata") {
        vec![url.clone()]
    } else {
        let root = format!("{origin}/.well-known/oauth-protected-resource");
        if uri.path() == "/" {
            vec![root]
        } else {
            vec![format!("{root}{}", uri.path()), root]
        }
    };
    let value = first_metadata(client, &urls).await?;
    let metadata: ProtectedResource = serde_json::from_value(value)
        .map_err(|_| failure("invalid protected resource metadata"))?;
    if metadata.resource != resource {
        return Err(failure("protected resource mismatch"));
    }
    let issuer = if let Some(registration) = registration {
        metadata
            .authorization_servers
            .iter()
            .find(|issuer| *issuer == &registration.issuer)
            .ok_or_else(|| failure("pre-registered issuer mismatch"))?
    } else {
        metadata
            .authorization_servers
            .first()
            .ok_or_else(|| failure("no authorization server"))?
    };
    let uri = endpoint(issuer)?;
    if uri.query().is_some() {
        return Err(failure("issuer has a query"));
    }
    let origin = format!(
        "{}://{}",
        uri.scheme_str().expect("validated scheme"),
        uri.authority().expect("validated authority")
    );
    let path = if uri.path() == "/" { "" } else { uri.path() };
    let mut urls = vec![
        format!("{origin}/.well-known/oauth-authorization-server{path}"),
        format!("{origin}/.well-known/openid-configuration{path}"),
    ];
    if !path.is_empty() {
        urls.push(format!("{origin}{path}/.well-known/openid-configuration"));
    }
    let value = first_metadata(client, &urls).await?;
    let server: Server = serde_json::from_value(value)
        .map_err(|_| failure("invalid authorization server metadata"))?;
    if &server.issuer != issuer {
        return Err(failure("authorization server issuer mismatch"));
    }
    endpoint(&server.authorization_endpoint)?;
    endpoint(&server.token_endpoint)?;
    if !server
        .code_challenge_methods_supported
        .iter()
        .any(|m| m == "S256")
    {
        return Err(failure("authorization server does not support S256 PKCE"));
    }
    Ok((metadata, server))
}

async fn first_metadata(client: &dyn Http, urls: &[String]) -> std::io::Result<Value> {
    for url in urls {
        let request = http::Request::get(endpoint(url)?)
            .header("accept", "application/json")
            .body(llm::http::Payload::default())
            .map_err(|_| failure("invalid metadata URL"))?;
        let response = client.send(request).await?;
        if response.status().is_success() {
            return read_json(response).await;
        }
        if response.status() != http::StatusCode::NOT_FOUND {
            return Err(failure("metadata request rejected"));
        }
    }
    Err(failure("authorization metadata not found"))
}

async fn read_json(response: llm::http::Response) -> std::io::Result<Value> {
    let mut body = response.into_body();
    let mut bytes = Vec::new();
    while let Some(chunk) = poll_fn(|cx| body.as_mut().poll_next(cx)).await {
        bytes.extend(chunk?);
    }
    serde_json::from_slice(&bytes).map_err(|_| failure("invalid OAuth JSON response"))
}

#[derive(Deserialize)]
struct Tokens {
    access_token: String,
    token_type: String,
    refresh_token: Option<String>,
    scope: Option<String>,
}

async fn token(
    client: &dyn Http,
    server: &Server,
    resource: &str,
    client_id: &str,
    params: &[(&str, &str)],
) -> std::io::Result<Option<Tokens>> {
    let body = form_urlencoded::Serializer::new(String::new())
        .extend_pairs(
            params
                .iter()
                .copied()
                .chain([("resource", resource), ("client_id", client_id)]),
        )
        .finish();
    let response = client
        .send(
            http::Request::post(&server.token_endpoint)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(llm::http::Payload::from(body.into_bytes()))
                .map_err(|_| failure("invalid token endpoint"))?,
        )
        .await?;
    let status = response.status();
    let value = read_json(response).await?;
    if !status.is_success() {
        if status == http::StatusCode::BAD_REQUEST && value["error"] == "invalid_grant" {
            return Ok(None);
        }
        return Err(failure("token endpoint rejected authorization"));
    }
    let token: Tokens =
        serde_json::from_value(value).map_err(|_| failure("invalid token response"))?;
    if token.access_token.is_empty() || !token.token_type.eq_ignore_ascii_case("Bearer") {
        return Err(failure("invalid bearer token response"));
    }
    Ok(Some(token))
}

#[derive(Default)]
struct AuthorizationNames(AtomicUsize);

async fn authorize(
    client: &dyn Http,
    cx: &Cx,
    server: &Server,
    resource: &str,
    client_id: &str,
    scopes: &str,
) -> std::io::Result<Tokens> {
    let runtime = cx
        .service::<dyn rt::Runtime>()
        .ok_or_else(|| failure("no runtime"))?;
    let listener = crate::redirect::Redirect::bind(&runtime.reactor())?;
    let redirect = listener.uri();
    let verifier =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sys::credentials::oauth_nonce()?);
    let state =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sys::credentials::oauth_nonce()?);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    let params = form_urlencoded::Serializer::new(String::new())
        .extend_pairs(
            [
                ("response_type", "code"),
                ("client_id", client_id),
                ("redirect_uri", &redirect),
                ("resource", resource),
                ("state", &state),
                ("code_challenge", &challenge),
                ("code_challenge_method", "S256"),
            ]
            .into_iter()
            .chain((!scopes.is_empty()).then_some(("scope", scopes))),
        )
        .finish();
    let url = format!(
        "{}{}{params}",
        server.authorization_endpoint,
        if server.authorization_endpoint.contains('?') {
            "&"
        } else {
            "?"
        }
    );
    // HTTP requests, including later pages, share the parent call's namespace.
    let names = cx
        .invocation()
        .local(0, || Arc::new(AuthorizationNames::default()));
    let name = names.0.fetch_add(1, Ordering::Relaxed);
    let shown = cx
        .invoke(
            &format!("oauth-authorize-{name}"),
            Authorize::NAME,
            &protocol::Json::encode(&json!({"url":url}))
                .map_err(|_| failure("authorization presentation failed"))?,
        )
        .await;
    if !matches!(shown.outcome, protocol::Outcome::Done { .. }) {
        return Err(failure("authorization declined"));
    }
    let response = listener.receive().await?;
    let mut values = BTreeMap::new();
    for (key, value) in form_urlencoded::parse(response.as_bytes()) {
        if values
            .insert(key.into_owned(), value.into_owned())
            .is_some()
        {
            return Err(failure("duplicate callback parameter"));
        }
    }
    if values.get("state") != Some(&state) {
        return Err(failure("redirect state mismatch"));
    }
    match values.get("iss") {
        Some(issuer) if issuer != &server.issuer => {
            return Err(failure("authorization response issuer mismatch"));
        }
        None if server.authorization_response_iss_parameter_supported => {
            return Err(failure("authorization response omitted issuer"));
        }
        _ => {}
    }
    if values.contains_key("error") {
        return Err(failure("authorization server declined authorization"));
    }
    let code = values
        .get("code")
        .ok_or_else(|| failure("authorization response has no code"))?;
    token(
        client,
        server,
        resource,
        client_id,
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", &redirect),
            ("code_verifier", &verifier),
        ],
    )
    .await?
    .ok_or_else(|| failure("authorization code rejected"))
}

#[derive(Deserialize, ext_derive::Schema)]
pub(crate) struct AuthorizeInput {
    pub url: String,
}

pub(crate) struct Authorize;
impl Tool for Authorize {
    type In = AuthorizeInput;
    type Out = ();
    const NAME: &'static str = "mcp_authorize";
    const DESCRIPTION: &'static str = "authorize an MCP server in your browser";
    const AUDIENCE: protocol::Audience = protocol::Audience::Plugin;
    const CONCURRENCY: ext::Concurrency = ext::Concurrency::Shared;
    fn call(&self, _: &Self::In, _: &Cx) -> Result<(), String> {
        Ok(())
    }
    fn present(input: &Self::In) -> Option<protocol::Card> {
        Some(protocol::Card {
            title: "Authorize MCP server".into(),
            body: format!(
                "Open this URL in your browser, then approve:\n{}",
                input.url
            ),
            ..protocol::Card::default()
        })
    }
}

fn failure(message: &str) -> std::io::Error {
    std::io::Error::other(format!("MCP OAuth: {message}"))
}
