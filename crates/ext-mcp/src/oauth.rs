//! HTTP authorization owns secrets outside the session's durable tool graph.

use std::collections::{BTreeMap, BTreeSet};
use std::future::poll_fn;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use base64::Engine;
use ext::Cx;
use llm::Http;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::OAUTH_CLIENT_ID;

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
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

/// One daemon's credential lifecycles, independent of sessions and connections.
/// Share this value when composing multiple MCP instances in the same daemon.
#[derive(Clone, Default)]
pub struct Authorizations(Arc<Mutex<Families>>);

#[derive(Default)]
struct Families {
    active: BTreeMap<Identity, Arc<Owner>>,
    owners: BTreeMap<Family, Arc<Owner>>,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Identity {
    server: crate::ServerName,
    resource: String,
    registration: Option<Registration>,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct Family {
    server: crate::ServerName,
    resource: String,
    issuer: String,
    client_id: String,
}

pub(crate) struct Connection {
    pub server: crate::ServerName,
    pub registration: Option<Registration>,
    pub authorizations: Authorizations,
    pub timeout: Duration,
}

pub(crate) struct Client {
    pub inner: Arc<dyn Http>,
    pub cx: Cx,
    pub connection: Connection,
}

/// A cache identity retains the owner, never an access or refresh token.
/// Anonymous identities are bound to the same registry and exact recipient.
#[derive(Clone)]
pub(crate) struct GenerationId(Arc<GenerationIdentity>);

struct GenerationIdentity {
    registry: Authorizations,
    identity: Identity,
    owner: Option<Arc<Owner>>,
    number: Option<u64>,
}

impl PartialEq for GenerationId {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0.registry.0, &other.0.registry.0)
            && self.0.identity == other.0.identity
            && self.0.number == other.0.number
            && match (&self.0.owner, &other.0.owner) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            }
    }
}
impl Eq for GenerationId {}

impl Client {
    fn identity(&self, url: &str) -> Identity {
        Identity {
            server: self.connection.server.clone(),
            resource: url.to_owned(),
            registration: self.connection.registration.clone(),
        }
    }

    async fn prepare(
        &self,
        identity: &Identity,
    ) -> std::io::Result<(Option<Arc<Owner>>, Option<Generation>)> {
        let owner = crate::lock(&self.connection.authorizations.0)
            .active
            .get(identity)
            .cloned();
        let generation = if let Some(owner) = &owner {
            let generation = owner.access(&self.cx).await?;
            if generation
                .as_ref()
                .and_then(|g| g.credential.as_ref().ok())
                .is_some_and(AccessToken::expired)
            {
                Some(
                    owner
                        .renew(
                            generation.as_ref().map(|g| g.number),
                            RenewalRequest {
                                status: http::StatusCode::UNAUTHORIZED,
                                challenge: BTreeMap::new(),
                                client: Arc::clone(&self.inner),
                                cx: self.cx.clone(),
                                deadline: Instant::now() + self.connection.timeout,
                            },
                        )
                        .await?,
                )
            } else {
                generation
            }
        } else {
            None
        };
        Ok((owner, generation))
    }

    pub(crate) async fn generation(&self, url: &str) -> std::io::Result<GenerationId> {
        let uri: http::Uri = url.parse().map_err(|_| failure("invalid resource URI"))?;
        let identity = self.identity(&uri.to_string());
        let (owner, generation) = self.prepare(&identity).await?;
        Ok(GenerationId(Arc::new(GenerationIdentity {
            registry: self.connection.authorizations.clone(),
            identity,
            owner,
            number: generation.as_ref().map(|g| g.number),
        })))
    }
}

// No Debug: persisted and in-memory credentials contain secrets.
#[derive(Serialize, Deserialize)]
struct Credential {
    family: Family,
    scope: String,
    #[serde(flatten)]
    access: AccessToken,
    refresh_token: Option<String>,
}

// Requests receive only this generation's access token, never its refresh token.
#[derive(Clone, Serialize, Deserialize)]
struct AccessToken {
    access_token: Arc<str>,
    expires_at: Option<SystemTime>,
}
impl AccessToken {
    fn expired(&self) -> bool {
        self.expires_at
            .is_some_and(|expiry| SystemTime::now() >= expiry)
    }
}

#[derive(Clone)]
struct Generation {
    number: u64,
    credential: Result<AccessToken, Arc<std::io::Error>>,
}
impl Generation {
    fn token(&self) -> std::io::Result<AccessToken> {
        self.credential
            .clone()
            .map_err(|error| std::io::Error::new(error.kind(), error.to_string()))
    }
}

struct Owner {
    family: Family,
    server: Server,
    scopes: Vec<String>,
    store: sys::credentials::Credentials,
    key: String,
    lifecycle: Mutex<Lifecycle>,
}

#[derive(Default)]
struct Lifecycle {
    current: Option<Generation>,
    cached: Cached,
    running: Option<Arc<Renewal>>,
}

#[derive(Clone, Default)]
enum Cached {
    #[default]
    Unread,
    Loaded(Option<Arc<Credential>>),
}

enum Acquisition {
    Published(Generation),
    Waiting(Arc<Renewal>),
    Start {
        renewal: Arc<Renewal>,
        previous: Cached,
    },
}
impl Lifecycle {
    fn available(&self) -> Option<Generation> {
        let mut current = self.current.clone()?;
        if current.credential.is_err()
            && let Cached::Loaded(Some(credential)) = &self.cached
        {
            current.credential = Ok(credential.access.clone());
        }
        Some(current)
    }

    fn acquire(&mut self, seen: Option<u64>) -> Acquisition {
        if let Some(running) = &self.running {
            return Acquisition::Waiting(Arc::clone(running));
        }
        if let Some(current) = &self.current
            && seen != Some(current.number)
        {
            return Acquisition::Published(current.clone());
        }
        let renewal = Arc::new(Renewal::default());
        self.running = Some(Arc::clone(&renewal));
        Acquisition::Start {
            renewal,
            previous: self.cached.clone(),
        }
    }

    fn publish(&mut self, result: std::io::Result<Arc<Credential>>) -> Generation {
        if let Ok(credential) = &result {
            self.cached = Cached::Loaded(Some(Arc::clone(credential)));
        }
        let published = Generation {
            number: self
                .current
                .as_ref()
                .map_or(1, |current| current.number + 1),
            credential: result
                .map(|credential| credential.access.clone())
                .map_err(Arc::new),
        };
        self.running = None;
        self.current = Some(published.clone());
        published
    }
}

#[derive(Default)]
struct Renewal {
    result: Mutex<Option<Generation>>,
    done: rt::Notify,
    callers: Mutex<Vec<Caller>>,
}
struct Caller {
    token: Arc<()>,
    cx: Cx,
    questions: Option<Arc<dyn ext::host::QuestionPort>>,
}
struct Participation {
    renewal: Arc<Renewal>,
    token: Arc<()>,
}
impl Drop for Participation {
    fn drop(&mut self) {
        let removed = {
            let mut callers = crate::lock(&self.renewal.callers);
            callers
                .iter()
                .position(|caller| Arc::ptr_eq(&caller.token, &self.token))
                .map(|index| callers.swap_remove(index))
        };
        drop(removed);
    }
}

impl Renewal {
    fn participate(self: &Arc<Self>, cx: &Cx) -> Participation {
        let token = Arc::new(());
        crate::lock(&self.callers).push(Caller {
            token: Arc::clone(&token),
            cx: cx.clone(),
            questions: cx.questions().ok(),
        });
        Participation {
            renewal: Arc::clone(self),
            token,
        }
    }

    fn presenter(&self) -> std::io::Result<(Cx, Arc<dyn ext::host::QuestionPort>)> {
        crate::lock(&self.callers)
            .iter()
            .find(|caller| !caller.cx.cancel.is_set() && caller.questions.is_some())
            .and_then(|caller| {
                caller
                    .questions
                    .clone()
                    .map(|port| (caller.cx.clone(), port))
            })
            .ok_or_else(|| failure("authorization has no waiting caller"))
    }
    async fn wait(&self) -> Generation {
        self.done
            .until(|| crate::lock(&self.result).is_some())
            .await;
        crate::lock(&self.result)
            .as_ref()
            .expect("completed renewal")
            .clone()
    }
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

impl Http for Client {
    fn send(
        &self,
        request: http::Request<llm::http::Payload>,
    ) -> llm::http::BoxFuture<'static, std::io::Result<llm::http::Response>> {
        let inner = Arc::clone(&self.inner);
        let cx = self.cx.clone();
        let identity = self.identity(&request.uri().to_string());
        let owners = self.connection.authorizations.clone();
        let timeout = self.connection.timeout;
        // Preparing a cache lookup and sending a request use the same owner path.
        let prepared_client = Self {
            inner: Arc::clone(&inner),
            cx: cx.clone(),
            connection: Connection {
                server: self.connection.server.clone(),
                registration: self.connection.registration.clone(),
                authorizations: owners.clone(),
                timeout,
            },
        };
        Box::pin(async move {
            let (mut owner, mut generation) = prepared_client.prepare(&identity).await?;
            let (parts, body) = request.into_parts();
            let mut authorizations = 0;
            loop {
                let credential = generation
                    .as_ref()
                    .and_then(|g| g.credential.as_ref().ok())
                    .cloned();
                let mut request = http::Request::new(body.clone());
                *request.method_mut() = parts.method.clone();
                *request.uri_mut() = parts.uri.clone();
                *request.version_mut() = parts.version;
                *request.headers_mut() = parts.headers.clone();
                if let Some(credential) = &credential {
                    let mut header =
                        http::HeaderValue::from_str(&format!("Bearer {}", credential.access_token))
                            .map_err(|_| failure("invalid access token"))?;
                    header.set_sensitive(true);
                    request
                        .headers_mut()
                        .insert(http::header::AUTHORIZATION, header);
                }
                let mut response = inner.send(request).await?;
                response
                    .extensions_mut()
                    .insert(GenerationId(Arc::new(GenerationIdentity {
                        registry: owners.clone(),
                        identity: identity.clone(),
                        owner: owner.clone(),
                        number: generation.as_ref().map(|g| g.number),
                    })));
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
                if authorizations == 2 {
                    return Err(failure("authorization rejected after retry"));
                }
                let next = owners.owner(&identity, &*inner, &cx, &challenge).await?;
                let seen = owner
                    .as_ref()
                    .filter(|old| Arc::ptr_eq(old, &next))
                    .and(generation.as_ref())
                    .map(|g| g.number);
                generation = Some(
                    next.renew(
                        seen,
                        RenewalRequest {
                            status,
                            challenge,
                            client: Arc::clone(&inner),
                            cx: cx.clone(),
                            deadline: Instant::now() + timeout,
                        },
                    )
                    .await?,
                );
                owner = Some(next);
                if credential.is_some() {
                    authorizations += 1;
                }
            }
        })
    }
}

impl Authorizations {
    async fn owner(
        &self,
        identity: &Identity,
        client: &dyn Http,
        cx: &Cx,
        challenge: &BTreeMap<String, String>,
    ) -> std::io::Result<Arc<Owner>> {
        let (metadata, server) = discover(
            client,
            &identity.resource,
            challenge,
            identity.registration.as_ref(),
        )
        .await?;
        let client_id = if let Some(registration) = &identity.registration {
            if registration.issuer != server.issuer {
                return Err(failure("pre-registered issuer mismatch"));
            }
            registration.id.clone()
        } else {
            if !server.client_id_metadata_document_supported {
                return Err(failure("server requires a pre-registered client id"));
            }
            OAUTH_CLIENT_ID.into()
        };
        let family = Family {
            server: identity.server.clone(),
            resource: identity.resource.clone(),
            issuer: server.issuer.clone(),
            client_id,
        };
        let store = match cx.service::<sys::credentials::Credentials>() {
            Some(store) => store.clone(),
            None => sys::credentials::Credentials::from_env()?,
        };
        let key = serde_json::to_string(&family)
            .map_err(|_| failure("credential identity encoding failed"))?;
        let mut families = crate::lock(&self.0);
        let owner = Arc::clone(families.owners.entry(family.clone()).or_insert_with(|| {
            Arc::new(Owner {
                family,
                server,
                scopes: metadata.scopes_supported.unwrap_or_default(),
                store,
                key,
                lifecycle: Mutex::default(),
            })
        }));
        families.active.insert(identity.clone(), Arc::clone(&owner));
        Ok(owner)
    }
}

struct RenewalRequest {
    status: http::StatusCode,
    challenge: BTreeMap<String, String>,
    client: Arc<dyn Http>,
    cx: Cx,
    deadline: Instant,
}

impl Owner {
    async fn access(&self, cx: &Cx) -> std::io::Result<Option<Generation>> {
        let (current, running) = {
            let state = crate::lock(&self.lifecycle);
            (state.available(), state.running.clone())
        };
        if let Some(running) = running {
            let _participant = running.participate(cx);
            let result = running.wait().await;
            result.token()?;
            return Ok(Some(result));
        }
        Ok(current)
    }

    async fn renew(
        self: &Arc<Self>,
        seen: Option<u64>,
        request: RenewalRequest,
    ) -> std::io::Result<Generation> {
        let runtime = request
            .cx
            .service_arc::<dyn rt::Runtime>()
            .ok_or_else(|| failure("no runtime"))?;
        let acquired = crate::lock(&self.lifecycle).acquire(seen);
        let (renewal, start) = match acquired {
            Acquisition::Published(current) => {
                current.token()?;
                return Ok(current);
            }
            Acquisition::Waiting(renewal) => (renewal, None),
            Acquisition::Start { renewal, previous } => (renewal, Some(previous)),
        };
        let _participant = renewal.participate(&request.cx);
        if let Some(previous) = start {
            let owner = Arc::clone(self);
            let done = Arc::clone(&renewal);
            let work_runtime = Arc::clone(&runtime);
            // The daemon runtime owns renewal; dropping any caller only drops its wait.
            runtime.spawn(Box::pin(async move {
                let result = owner
                    .replace(previous, &request, &done, &*work_runtime)
                    .await;
                let published = crate::lock(&owner.lifecycle).publish(result);
                *crate::lock(&done.result) = Some(published);
                done.done.notify();
            }));
        }
        let result = renewal.wait().await;
        result.token()?;
        Ok(result)
    }

    async fn replace(
        &self,
        previous: Cached,
        request: &RenewalRequest,
        renewal: &Renewal,
        runtime: &dyn rt::Runtime,
    ) -> std::io::Result<Arc<Credential>> {
        let previous = if let Cached::Loaded(previous) = previous {
            previous
        } else {
            let store = self.store.clone();
            let key = self.key.clone();
            let bytes = rt::unblock(runtime, move || store.load(&key)).await?;
            let credential: Option<Credential> = bytes
                .map(|bytes| {
                    serde_json::from_slice(&bytes).map_err(|_| failure("invalid stored credential"))
                })
                .transpose()?;
            if credential.as_ref().is_some_and(|c| c.family != self.family) {
                return Err(failure("stored credential identity mismatch"));
            }
            let credential = credential.map(Arc::new);
            crate::lock(&self.lifecycle).cached = Cached::Loaded(credential.clone());
            if let Some(credential) = credential.as_ref().filter(|c| !c.access.expired()) {
                return Ok(Arc::clone(credential));
            }
            credential
        };
        let scopes = request
            .challenge
            .get("scope")
            .cloned()
            .unwrap_or_else(|| self.scopes.join(" "));
        let scopes = scopes
            .split_whitespace()
            .chain(previous.iter().flat_map(|c| c.scope.split_whitespace()))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
            .join(" ");
        let refreshed = if request.status == http::StatusCode::UNAUTHORIZED {
            if let Some(refresh) = previous.as_ref().and_then(|c| c.refresh_token.as_deref()) {
                network(
                    runtime,
                    request.deadline,
                    token(
                        &*request.client,
                        &self.server,
                        &self.family.resource,
                        &self.family.client_id,
                        &[("grant_type", "refresh_token"), ("refresh_token", refresh)],
                    ),
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
            if request.status == http::StatusCode::UNAUTHORIZED {
                self.save(runtime, None).await?;
            }
            let (presenter, questions) = renewal.presenter()?;
            authorize(
                &*request.client,
                &presenter,
                &*questions,
                &self.server,
                &self.family,
                &scopes,
                request.deadline.saturating_duration_since(Instant::now()),
            )
            .await?
        };
        let expires_at = fresh
            .expires_in
            .map(|seconds| {
                SystemTime::now()
                    .checked_add(Duration::from_secs(seconds))
                    .ok_or_else(|| failure("invalid token expiry"))
            })
            .transpose()?;
        let credential = Arc::new(Credential {
            family: self.family.clone(),
            scope: fresh.scope.unwrap_or(scopes),
            access: AccessToken {
                access_token: fresh.access_token.into(),
                expires_at,
            },
            refresh_token: fresh.refresh_token.or(refresh_token),
        });
        // Native storage completion precedes publication to every waiter.
        self.save(runtime, Some(&credential)).await?;
        Ok(credential)
    }

    async fn save(
        &self,
        runtime: &dyn rt::Runtime,
        credential: Option<&Credential>,
    ) -> std::io::Result<()> {
        let encoded = credential
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|_| failure("credential encoding failed"))?;
        let store = self.store.clone();
        let key = self.key.clone();
        rt::unblock(runtime, move || store.save(&key, encoded.as_deref())).await?;
        if credential.is_none() {
            crate::lock(&self.lifecycle).cached = Cached::Loaded(None);
        }
        Ok(())
    }
}

async fn network<T>(
    runtime: &dyn rt::Runtime,
    deadline: Instant,
    future: impl Future<Output = std::io::Result<T>>,
) -> std::io::Result<T> {
    rt::or(future, async {
        runtime
            .sleep(deadline.saturating_duration_since(Instant::now()))
            .await;
        Err(failure("authorization timed out"))
    })
    .await
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
    expires_in: Option<u64>,
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

async fn authorize(
    client: &dyn Http,
    cx: &Cx,
    questions: &dyn ext::host::QuestionPort,
    server: &Server,
    family: &Family,
    scopes: &str,
    timeout: Duration,
) -> std::io::Result<Tokens> {
    let resource = family.resource.as_str();
    let client_id = family.client_id.as_str();
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
    let question = protocol::Question {
        source: resource.into(),
        prompts: BTreeMap::from([(
            "authorize".into(),
            protocol::Prompt::Url {
                message: "Authorize this MCP server in your browser, then continue.".into(),
                url,
            },
        )]),
    };
    let shown = questions
        .ask(question)
        .await
        .map_err(|_| failure("authorization presentation ended"))?;
    if !shown.accepted() {
        return Err(failure("authorization declined"));
    }
    let deadline = Instant::now() + timeout;
    let response = network(runtime, deadline, listener.receive()).await?;
    let code = authorization_code(&response, &state, server)?;
    network(
        runtime,
        deadline,
        token(
            client,
            server,
            resource,
            client_id,
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", &redirect),
                ("code_verifier", &verifier),
            ],
        ),
    )
    .await?
    .ok_or_else(|| failure("authorization code rejected"))
}

fn authorization_code(response: &str, state: &str, server: &Server) -> std::io::Result<String> {
    let mut values = BTreeMap::new();
    for (key, value) in form_urlencoded::parse(response.as_bytes()) {
        if values
            .insert(key.into_owned(), value.into_owned())
            .is_some()
        {
            return Err(failure("duplicate callback parameter"));
        }
    }
    if values.get("state") != Some(&state.to_owned()) {
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
    values
        .remove("code")
        .ok_or_else(|| failure("authorization response has no code"))
}

fn failure(message: &str) -> std::io::Error {
    std::io::Error::other(format!("MCP OAuth: {message}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_authorization_keeps_stdio_discovery_slots_pointer_sized() {
        assert_eq!(size_of::<Option<GenerationId>>(), size_of::<usize>());
    }

    #[test]
    fn issued_generations_share_immutable_access_token_bytes() {
        let credential = credential();
        let mut lifecycle = Lifecycle::default();
        assert!(matches!(lifecycle.acquire(None), Acquisition::Start { .. }));
        let generation = lifecycle.publish(Ok(Arc::clone(&credential)));
        let first = generation.token().unwrap();
        let second = generation.token().unwrap();
        assert!(Arc::ptr_eq(&first.access_token, &second.access_token));
        assert!(Arc::ptr_eq(
            &first.access_token,
            &credential.access.access_token
        ));
        assert_eq!(Arc::strong_count(&credential), 2);
    }

    #[test]
    fn failed_upgrade_preserves_access_without_hiding_the_shared_failure() {
        let mut lifecycle = Lifecycle::default();
        assert!(matches!(lifecycle.acquire(None), Acquisition::Start { .. }));
        let prior = lifecycle.publish(Ok(credential()));
        assert!(matches!(
            lifecycle.acquire(Some(prior.number)),
            Acquisition::Start { .. }
        ));
        let failed = lifecycle.publish(Err(failure("authorization declined")));
        assert!(failed.token().is_err());
        assert!(lifecycle.available().unwrap().token().is_ok());
        let Acquisition::Published(late) = lifecycle.acquire(Some(prior.number)) else {
            panic!("old waiters must share the failed upgrade");
        };
        assert!(late.token().is_err());
    }

    fn credential() -> Arc<Credential> {
        Arc::new(Credential {
            family: Family {
                server: "fixture".parse().unwrap(),
                resource: "https://fixture/mcp".into(),
                issuer: "https://fixture/issuer".into(),
                client_id: OAUTH_CLIENT_ID.into(),
            },
            scope: "read".into(),
            access: AccessToken {
                access_token: "access".into(),
                expires_at: None,
            },
            refresh_token: Some("refresh".into()),
        })
    }

    #[test]
    fn old_generation_rejections_share_the_published_outcome() {
        let mut state = Lifecycle::default();
        let Acquisition::Start { renewal, .. } = state.acquire(None) else {
            panic!("first caller must start renewal");
        };
        let Acquisition::Waiting(joined) = state.acquire(None) else {
            panic!("next caller must join renewal");
        };
        assert!(Arc::ptr_eq(&renewal, &joined));
        let published = state.publish(Err(failure("authorization declined")));
        *crate::lock(&renewal.result) = Some(published);
        renewal.done.notify();
        let result = rt::block_on(joined.wait());
        assert!(result.token().is_err());
        let Acquisition::Published(late) = state.acquire(None) else {
            panic!("late rejection must reuse publication");
        };
        assert_eq!(late.number, result.number);
        assert!(Arc::ptr_eq(
            late.credential.as_ref().err().unwrap(),
            result.credential.as_ref().err().unwrap()
        ));
        assert!(matches!(
            state.acquire(Some(result.number)),
            Acquisition::Start { .. }
        ));
    }
}
