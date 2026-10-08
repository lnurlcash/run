//! run.lnurlcash.com: hosted, self-managing LNURLcash mints.
//!
//! It serves the landing page and its beta list, which people join by
//! signing in with nostr (NIP-07 in the browser, NIP-98 on the wire). An
//! approved member provisions a mint: Run commits its Kubernetes manifests to
//! a git repository, and Argo CD rolls them out (`gitops.rs`).

use std::{net::SocketAddr, sync::Arc, time::SystemTime};

use anyhow::Context;
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use clap::{Parser, Subcommand};
use serde::Deserialize;
use serde_json::{Value, json};

mod db;
mod gitops;
mod nostr;

/// The most a note on a sign-up may say.
const MAX_NOTE_CHARS: usize = 500;

#[derive(Parser)]
#[command(version, about)]
struct Config {
    /// Where the HTTP server listens.
    #[arg(long, env = "LISTEN", default_value = "127.0.0.1:8200")]
    listen: SocketAddr,

    /// The page's public origin: NIP-98 sign-ups must be signed for
    /// `<PUBLIC_URL>/api/beta`.
    #[arg(long, env = "PUBLIC_URL", default_value = "https://run.lnurlcash.com")]
    public_url: String,

    /// Directory for the beta list (`beta.sqlite3`).
    #[arg(long, env = "DATA_DIR", default_value = "data")]
    data_dir: std::path::PathBuf,

    #[command(flatten)]
    gitops: GitOpsArgs,

    #[command(subcommand)]
    command: Option<Command>,
}

/// Provisioning: off unless GITOPS_REPO is set.
#[derive(clap::Args)]
struct GitOpsArgs {
    /// The Kubernetes configuration repository Argo CD watches. A token for
    /// pushing goes in the URL (https://user:token@host/…) or in git's own
    /// credential setup; it never appears in logs.
    #[arg(long, env = "GITOPS_REPO", hide_env_values = true)]
    gitops_repo: Option<String>,
    /// The URL Argo CD reads the repository at (default: GITOPS_REPO
    /// without credentials).
    #[arg(long, env = "GITOPS_ARGO_URL")]
    gitops_argo_url: Option<String>,
    #[arg(long, env = "GITOPS_BRANCH", default_value = "main")]
    gitops_branch: String,
    /// Where the clone lives (default: <DATA_DIR>/gitops).
    #[arg(long, env = "GITOPS_DIR")]
    gitops_dir: Option<std::path::PathBuf>,
    /// The lnurl-mint-rust image mints run, pinned.
    #[arg(
        long,
        env = "MINT_IMAGE",
        default_value = "lnurlcash/lnurl-mint-rust:latest"
    )]
    mint_image: String,
    /// Mints live at <name>.<MINT_DOMAIN>.
    #[arg(long, env = "MINT_DOMAIN", default_value = "run.lnurlcash.com")]
    mint_domain: String,
    #[arg(long, env = "MINT_NAMESPACE", default_value = "lnurlcash-mints")]
    mint_namespace: String,
    #[arg(long, env = "MINT_NETWORK", default_value = "bitcoin")]
    mint_network: String,
    /// The shared Electrum server (electrs) mints sync from.
    #[arg(
        long,
        env = "MINT_ELECTRUM_URL",
        default_value = "tcp://electrs.bitcoin.svc.cluster.local:50001"
    )]
    mint_electrum_url: String,
    /// The LSPS2 provider mints get inbound liquidity from, pubkey@host:port.
    #[arg(long, env = "MINT_LSP_NODE")]
    mint_lsp_node: Option<String>,
    #[arg(long, env = "MINT_CERT_ISSUER", default_value = "letsencrypt")]
    mint_cert_issuer: String,
    /// A wildcard certificate's secret for *.<MINT_DOMAIN>, instead of one
    /// certificate per mint.
    #[arg(long, env = "MINT_TLS_SECRET")]
    mint_tls_secret: Option<String>,
    #[arg(long, env = "MINT_INGRESS_CLASS", default_value = "nginx")]
    mint_ingress_class: String,
    #[arg(long, env = "MINT_STORAGE", default_value = "2Gi")]
    mint_storage: String,
    #[arg(long, env = "MINT_STORAGE_CLASS")]
    mint_storage_class: Option<String>,
    /// Mints one approved member may have.
    #[arg(long, env = "MINTS_PER_MEMBER", default_value_t = 1)]
    mints_per_member: usize,
}

impl GitOpsArgs {
    fn build(&self, data_dir: &std::path::Path) -> Option<gitops::GitOps> {
        let url = self.gitops_repo.clone()?;
        let argo_url = self
            .gitops_argo_url
            .clone()
            .unwrap_or_else(|| strip_credentials(&url));
        Some(gitops::GitOps {
            platform: gitops::Platform {
                image: self.mint_image.clone(),
                domain: self.mint_domain.clone(),
                namespace: self.mint_namespace.clone(),
                network: self.mint_network.clone(),
                electrum_url: self.mint_electrum_url.clone(),
                lsp_node: self.mint_lsp_node.clone().filter(|l| !l.is_empty()),
                cert_issuer: self.mint_cert_issuer.clone(),
                tls_secret: self.mint_tls_secret.clone().filter(|t| !t.is_empty()),
                ingress_class: self.mint_ingress_class.clone(),
                storage: self.mint_storage.clone(),
                storage_class: self.mint_storage_class.clone().filter(|c| !c.is_empty()),
                repo_url: argo_url,
                branch: self.gitops_branch.clone(),
            },
            repo: gitops::Repo::new(
                self.gitops_dir
                    .clone()
                    .unwrap_or_else(|| data_dir.join("gitops")),
                url,
                self.gitops_branch.clone(),
            ),
        })
    }
}

/// `https://user:token@host/x` → `https://host/x`.
fn strip_credentials(url: &str) -> String {
    match (url.find("://"), url.find('@')) {
        (Some(scheme), Some(at)) if at > scheme => {
            format!("{}{}", &url[..scheme + 3], &url[at + 1..])
        }
        _ => url.to_string(),
    }
}

#[derive(Subcommand)]
enum Command {
    /// Print the beta list as JSON lines, with npubs.
    Export,
    /// Let a beta member provision mints (their key must have signed in).
    Approve {
        npub: String,
        /// Take the approval back.
        #[arg(long)]
        revoke: bool,
    },
    /// List the mints, as JSON lines.
    Mints,
    /// Provision a mint for an owner, as the operator.
    Provision {
        name: String,
        #[arg(long)]
        owner: String,
    },
    /// Scale a mint to zero. Its volume, seed and channels stay.
    Disable { name: String },
    /// Scale a mint back up.
    Enable { name: String },
    /// Re-render every mint with the current settings: an image upgrade is
    /// a new MINT_IMAGE, then this.
    Rerender,
}

struct AppState {
    store: db::Store,
    public_url: String,
    gitops: Option<gitops::GitOps>,
    mints_per_member: usize,
}

impl AppState {
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.public_url)
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let config = Config::parse();
    std::fs::create_dir_all(&config.data_dir)
        .with_context(|| format!("could not create {}", config.data_dir.display()))?;
    let path = config.data_dir.join("beta.sqlite3");
    let store = db::Store::open(&path.to_string_lossy())
        .with_context(|| format!("could not open {}", path.display()))?;

    let gitops = config.gitops.build(&config.data_dir);
    if let Some(command) = config.command {
        return operator(command, &store, gitops.as_ref());
    }

    match &gitops {
        Some(g) => log::info!(
            "provisioning into {} ({}), mints at *.{}",
            g.platform.repo_url,
            g.platform.namespace,
            g.platform.domain
        ),
        None => log::info!("provisioning off: set GITOPS_REPO"),
    }
    let state = Arc::new(AppState {
        store,
        public_url: config.public_url.trim_end_matches('/').to_string(),
        gitops,
        mints_per_member: config.gitops.mints_per_member,
    });
    let listener = tokio::net::TcpListener::bind(config.listen)
        .await
        .with_context(|| format!("could not listen on {}", config.listen))?;
    log::info!("serving on {} for {}", config.listen, config.public_url);
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route(
            "/",
            get(|| {
                page(
                    "text/html; charset=utf-8",
                    include_str!("../web/index.html"),
                )
            }),
        )
        .route(
            "/styles.css",
            get(|| page("text/css", include_str!("../web/styles.css"))),
        )
        .route(
            "/app.js",
            get(|| page("text/javascript", include_str!("../web/app.js"))),
        )
        .route(
            "/favicon.svg",
            get(|| page("image/svg+xml", include_str!("../web/favicon.svg"))),
        )
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/beta", get(count).post(sign_up))
        .route("/api/mints", get(my_mints).post(create_mint))
        .layer(DefaultBodyLimit::max(4096))
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

async fn page(content_type: &'static str, body: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "public, max-age=300"),
        ],
        body,
    )
        .into_response()
}

async fn security_headers(req: axum::extract::Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; style-src 'self' https://fonts.googleapis.com; font-src https://fonts.gstatic.com; \
             img-src 'self' data:; connect-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'",
        ),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    res
}

type ApiResult = Result<Json<Value>, (StatusCode, Json<Value>)>;

fn refuse(status: StatusCode, reason: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    (
        status,
        Json(json!({"status": "ERROR", "reason": reason.to_string()})),
    )
}

fn internal(e: anyhow::Error) -> (StatusCode, Json<Value>) {
    log::error!("{e:#}");
    refuse(
        StatusCode::INTERNAL_SERVER_ERROR,
        "something went wrong on our side",
    )
}

async fn count(State(state): State<Arc<AppState>>) -> ApiResult {
    Ok(Json(
        json!({"count": state.store.count().map_err(internal)?}),
    ))
}

#[derive(Deserialize)]
struct SignUp {
    #[serde(default)]
    note: String,
    /// Without a NIP-07 extension: an npub, typed in. Listed as unverified.
    npub: Option<String>,
}

/// `POST /api/beta`: join the beta list. Signed (an `Authorization: Nostr`
/// NIP-98 header over this URL and body) the key is verified; with just an
/// `npub` in the body it is listed unverified, and can't overwrite a
/// verified entry.
async fn sign_up(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> ApiResult {
    let req: SignUp = serde_json::from_slice(&body)
        .map_err(|_| refuse(StatusCode::BAD_REQUEST, "expected a JSON body"))?;
    let note = req.note.trim();
    if note.chars().count() > MAX_NOTE_CHARS {
        return Err(refuse(
            StatusCode::BAD_REQUEST,
            format!("keep the note under {MAX_NOTE_CHARS} characters"),
        ));
    }
    let (pubkey, signed) = match headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        Some(auth) => {
            let pubkey =
                nostr::verify_http_auth(auth, &state.url("/api/beta"), "POST", &body, now())
                    .map_err(|e| refuse(StatusCode::UNAUTHORIZED, format!("{e:#}")))?;
            (pubkey, true)
        }
        None => {
            let npub = req.npub.as_deref().ok_or_else(|| {
                refuse(
                    StatusCode::BAD_REQUEST,
                    "sign in with nostr, or give an npub",
                )
            })?;
            let pubkey = nostr::parse_pubkey(npub).map_err(|e| {
                log::debug!("refused npub {npub:?}: {e:#}");
                refuse(
                    StatusCode::BAD_REQUEST,
                    "that isn't a valid npub: check it and try again",
                )
            })?;
            (pubkey, false)
        }
    };
    let (place, verified) = state
        .store
        .sign_up(&pubkey, signed, note, now())
        .map_err(internal)?;
    log::info!(
        "beta sign-up #{place}: {} ({})",
        nostr::npub(&pubkey),
        if signed { "signed" } else { "typed" }
    );
    let mut res =
        json!({"status": "OK", "npub": nostr::npub(&pubkey), "place": place, "verified": verified});
    // only to the key itself: whether it may provision, and what it runs
    if signed {
        res["approved"] = json!(state.store.may_provision(&pubkey).map_err(internal)?);
        res["provisioning"] = json!(state.gitops.is_some());
        res["mints"] = mints_json(&state, &pubkey)?;
    }
    Ok(Json(res))
}

fn mints_json(state: &AppState, owner: &str) -> Result<Value, (StatusCode, Json<Value>)> {
    let Some(g) = &state.gitops else {
        return Ok(json!([]));
    };
    let rows = state.store.mints(Some(owner)).map_err(internal)?;
    Ok(Value::Array(
        rows.into_iter().map(|m| mint_json(g, &m)).collect(),
    ))
}

fn mint_json(g: &gitops::GitOps, m: &db::MintRow) -> Value {
    let host = format!("{}.{}", m.name, g.platform.domain);
    json!({
        "name": m.name,
        "url": format!("https://{host}"),
        "lightning_address": format!("mint@{host}"),
        "enabled": m.enabled,
        "commit": m.commit_id,
    })
}

/// The signer of a NIP-98 request to `path`.
fn signer(
    state: &AppState,
    headers: &HeaderMap,
    method: &str,
    path: &str,
    body: &[u8],
) -> Result<String, (StatusCode, Json<Value>)> {
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| refuse(StatusCode::UNAUTHORIZED, "sign in with nostr"))?;
    nostr::verify_http_auth(auth, &state.url(path), method, body, now())
        .map_err(|e| refuse(StatusCode::UNAUTHORIZED, format!("{e:#}")))
}

/// `GET /api/mints`, signed: the signer's mints.
async fn my_mints(State(state): State<Arc<AppState>>, headers: HeaderMap) -> ApiResult {
    let owner = signer(&state, &headers, "GET", "/api/mints", b"")?;
    Ok(Json(json!({"mints": mints_json(&state, &owner)?})))
}

#[derive(Deserialize)]
struct NewMint {
    name: String,
}

/// `POST /api/mints {"name"}`, signed by an approved member: commit a new
/// mint to the GitOps repository. Argo CD takes it from there.
async fn create_mint(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult {
    let owner = signer(&state, &headers, "POST", "/api/mints", &body)?;
    let req: NewMint = serde_json::from_slice(&body)
        .map_err(|_| refuse(StatusCode::BAD_REQUEST, "expected {\"name\": \"…\"}"))?;
    let name = req.name.trim().to_ascii_lowercase();
    if state.gitops.is_none() {
        return Err(refuse(
            StatusCode::SERVICE_UNAVAILABLE,
            "provisioning isn't open yet",
        ));
    }
    if !state.store.may_provision(&owner).map_err(internal)? {
        return Err(refuse(
            StatusCode::FORBIDDEN,
            "your beta access isn't approved yet: we'll reach you on nostr",
        ));
    }
    gitops::check_name(&name).map_err(|e| refuse(StatusCode::BAD_REQUEST, e))?;
    if state.store.mints(Some(&owner)).map_err(internal)?.len() >= state.mints_per_member {
        return Err(refuse(
            StatusCode::FORBIDDEN,
            "you have as many mints as the beta allows",
        ));
    }
    if !state
        .store
        .claim_mint(&name, &owner, now())
        .map_err(internal)?
    {
        return Err(refuse(StatusCode::CONFLICT, "that name is taken"));
    }
    let worker = Arc::clone(&state);
    let mint = gitops::Mint {
        name: name.clone(),
        owner: owner.clone(),
        enabled: true,
    };
    let committed =
        tokio::task::spawn_blocking(move || worker.gitops.as_ref().map(|g| g.provision(&mint)))
            .await
            .map_err(|e| internal(e.into()))?;
    match committed {
        Some(Ok(commit)) => {
            let commit = commit.unwrap_or_default();
            state
                .store
                .mint_committed(&name, &commit, true)
                .map_err(internal)?;
            log::info!("provisioned {name} for {} in {commit}", nostr::npub(&owner));
            let row = state
                .store
                .mint(&name)
                .map_err(internal)?
                .ok_or_else(|| internal(anyhow::anyhow!("lost {name}")))?;
            let mut res = mint_json(state.gitops.as_ref().expect("checked above"), &row);
            res["status"] = json!("OK");
            Ok(Json(res))
        }
        Some(Err(e)) => {
            let _ = state.store.release_mint(&name);
            Err(internal(e.context(format!("provisioning {name}"))))
        }
        None => Err(refuse(
            StatusCode::SERVICE_UNAVAILABLE,
            "provisioning isn't open yet",
        )),
    }
}

/// The operator's subcommands.
fn operator(
    command: Command,
    store: &db::Store,
    gitops: Option<&gitops::GitOps>,
) -> anyhow::Result<()> {
    let need = || gitops.ok_or_else(|| anyhow::anyhow!("set GITOPS_REPO to provision"));
    match command {
        Command::Export => {
            for s in store.all()? {
                println!(
                    "{}",
                    json!({"npub": nostr::npub(&s.pubkey), "verified": s.verified, "note": s.note, "created_at": s.created_at})
                );
            }
        }
        Command::Approve { npub, revoke } => {
            let pubkey = nostr::parse_pubkey(&npub)?;
            anyhow::ensure!(
                store.approve(&pubkey, !revoke)?,
                "{npub} isn't on the beta list"
            );
            let verified = store.may_provision(&pubkey)? || revoke;
            println!(
                "{} {}",
                if revoke { "revoked" } else { "approved" },
                nostr::npub(&pubkey)
            );
            if !verified {
                println!("note: the key never signed in, so it can't provision until it does");
            }
        }
        Command::Mints => {
            for m in store.mints(None)? {
                println!(
                    "{}",
                    json!({"name": m.name, "owner": nostr::npub(&m.owner), "enabled": m.enabled, "commit": m.commit_id})
                );
            }
        }
        Command::Provision { name, owner } => {
            let g = need()?;
            let owner = nostr::parse_pubkey(&owner)?;
            gitops::check_name(&name)?;
            anyhow::ensure!(store.claim_mint(&name, &owner, now())?, "{name} is taken");
            match g.provision(&gitops::Mint {
                name: name.clone(),
                owner,
                enabled: true,
            }) {
                Ok(commit) => {
                    let commit = commit.unwrap_or_default();
                    store.mint_committed(&name, &commit, true)?;
                    println!("provisioned {name} in {commit}");
                }
                Err(e) => {
                    store.release_mint(&name)?;
                    return Err(e);
                }
            }
        }
        Command::Disable { name } | Command::Enable { name } if store.mint(&name)?.is_none() => {
            anyhow::bail!("no mint named {name}")
        }
        Command::Disable { name } => set_enabled(need()?, store, &name, false)?,
        Command::Enable { name } => set_enabled(need()?, store, &name, true)?,
        Command::Rerender => {
            let g = need()?;
            for m in store.mints(None)? {
                let mint = gitops::Mint {
                    name: m.name.clone(),
                    owner: m.owner.clone(),
                    enabled: m.enabled,
                };
                match g.update(&mint, "re-render")? {
                    Some(commit) => {
                        store.mint_committed(&m.name, &commit, m.enabled)?;
                        println!("{}: {commit}", m.name);
                    }
                    None => println!("{}: unchanged", m.name),
                }
            }
        }
    }
    Ok(())
}

fn set_enabled(
    g: &gitops::GitOps,
    store: &db::Store,
    name: &str,
    enabled: bool,
) -> anyhow::Result<()> {
    let m = store
        .mint(name)?
        .ok_or_else(|| anyhow::anyhow!("no mint named {name}"))?;
    let mint = gitops::Mint {
        name: m.name,
        owner: m.owner,
        enabled,
    };
    let commit = g.update(&mint, if enabled { "enable" } else { "disable" })?;
    let commit = commit.or(m.commit_id).unwrap_or_default();
    store.mint_committed(name, &commit, enabled)?;
    println!(
        "{} {name} ({commit})",
        if enabled { "enabled" } else { "disabled" }
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    const URL: &str = "https://run.lnurlcash.com/api/beta";

    fn app() -> Router {
        router(Arc::new(AppState {
            store: db::Store::in_memory().unwrap(),
            public_url: "https://run.lnurlcash.com".into(),
            gitops: None,
            mints_per_member: 1,
        }))
    }

    async fn post(app: &Router, body: &str, auth: Option<String>) -> (StatusCode, Value) {
        let mut req = Request::post("/api/beta").header("content-type", "application/json");
        if let Some(auth) = auth {
            req = req.header("authorization", auth);
        }
        let res = app
            .clone()
            .oneshot(req.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 16)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn signed_and_typed_sign_ups() {
        let app = app();
        let body = r#"{"note":"a mint for our meetup"}"#;
        let auth = nostr::tests::auth_header([7; 32], URL, body.as_bytes(), now());
        let (status, res) = post(&app, body, Some(auth)).await;
        assert_eq!(status, StatusCode::OK, "{res}");
        assert_eq!(
            (res["place"].as_u64(), res["verified"].as_bool()),
            (Some(1), Some(true))
        );

        // the same key typed in: still one entry, still verified
        let npub = res["npub"].as_str().unwrap().to_string();
        let (_, res) = post(&app, &format!(r#"{{"npub":"{npub}"}}"#), None).await;
        assert_eq!(
            (res["place"].as_u64(), res["verified"].as_bool()),
            (Some(1), Some(true))
        );

        // a header signed for another body is refused
        let auth = nostr::tests::auth_header([8; 32], URL, b"{}", now());
        let (status, _) = post(&app, body, Some(auth)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let (status, _) = post(&app, r#"{"npub":"npub1nope"}"#, None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = post(&app, "{}", None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let long = format!(r#"{{"npub":"{npub}","note":"{}"}}"#, "x".repeat(501));
        assert_eq!(post(&app, &long, None).await.0, StatusCode::BAD_REQUEST);

        let res = app
            .oneshot(Request::get("/api/beta").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 16)
            .await
            .unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap()["count"], 1);
    }

    /// Approved members provision through the API, into a real git remote.
    #[tokio::test]
    async fn approved_members_provision_a_mint() {
        let tmp = std::env::temp_dir().join(format!("lnurlcash-run-api-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let remote = tmp.join("remote.git");
        std::fs::create_dir_all(&remote).unwrap();
        let init = std::process::Command::new("git")
            .args(["init", "-q", "--bare", "-b", "main"])
            .current_dir(&remote)
            .status()
            .unwrap();
        assert!(init.success());
        let url = remote.to_string_lossy().into_owned();
        let store = db::Store::in_memory().unwrap();
        let state = Arc::new(AppState {
            store,
            public_url: "https://run.lnurlcash.com".into(),
            gitops: Some(gitops::GitOps {
                platform: gitops::tests::platform(&url),
                repo: gitops::Repo::new(tmp.join("clone"), url.clone(), "main".into()),
            }),
            mints_per_member: 1,
        });
        let app = router(Arc::clone(&state));
        const MINTS: &str = "https://run.lnurlcash.com/api/mints";
        let create = |secret: [u8; 32], name: &str| {
            let body = format!(r#"{{"name":"{name}"}}"#);
            let auth = nostr::tests::auth_header(secret, MINTS, body.as_bytes(), now());
            (body, auth)
        };
        let send = |body: String, auth: String| {
            let app = app.clone();
            async move {
                let req = Request::post("/api/mints")
                    .header("authorization", auth)
                    .body(Body::from(body))
                    .unwrap();
                let res = app.oneshot(req).await.unwrap();
                let status = res.status();
                let bytes = axum::body::to_bytes(res.into_body(), 1 << 16)
                    .await
                    .unwrap();
                (status, serde_json::from_slice::<Value>(&bytes).unwrap())
            }
        };

        // signed up, but not approved yet
        let (body, auth) = create([7; 32], "satoshi");
        let pubkey = nostr::verify_http_auth(&auth, MINTS, "POST", body.as_bytes(), now()).unwrap();
        state.store.sign_up(&pubkey, true, "", now()).unwrap();
        let (status, res) = send(body.clone(), auth.clone()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{res}");

        state.store.approve(&pubkey, true).unwrap();
        let (status, res) = send(body, auth).await;
        assert_eq!(status, StatusCode::OK, "{res}");
        assert_eq!(res["lightning_address"], "mint@satoshi.run.lnurlcash.com");
        assert!(
            res["commit"].as_str().is_some_and(|c| c.len() >= 7),
            "{res}"
        );

        // one mint per member in the beta
        let (body, auth) = create([7; 32], "satoshi-two");
        assert_eq!(send(body, auth).await.0, StatusCode::FORBIDDEN);
        // a taken name, for another approved member
        let (body, auth) = create([8; 32], "satoshi");
        let other = nostr::verify_http_auth(&auth, MINTS, "POST", body.as_bytes(), now()).unwrap();
        state.store.sign_up(&other, true, "", now()).unwrap();
        state.store.approve(&other, true).unwrap();
        assert_eq!(send(body, auth).await.0, StatusCode::CONFLICT);
        let (body, auth) = create([8; 32], "www");
        assert_eq!(send(body, auth).await.0, StatusCode::BAD_REQUEST);
        // a signature for another endpoint doesn't carry over
        let body = r#"{"name":"stolen"}"#.to_string();
        let auth = nostr::tests::auth_header(
            [8; 32],
            "https://run.lnurlcash.com/api/beta",
            body.as_bytes(),
            now(),
        );
        assert_eq!(send(body, auth).await.0, StatusCode::UNAUTHORIZED);

        // the owner lists theirs, with a signed GET
        let get = nostr::tests::sign(
            [7; 32],
            now(),
            nostr::HTTP_AUTH_KIND,
            vec![
                vec!["u".into(), MINTS.into()],
                vec!["method".into(), "GET".into()],
            ],
            "",
        );
        use base64::Engine;
        let header = format!(
            "Nostr {}",
            base64::engine::general_purpose::STANDARD.encode(get.to_string())
        );
        let res = app
            .clone()
            .oneshot(
                Request::get("/api/mints")
                    .header("authorization", header)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 16)
            .await
            .unwrap();
        let listed: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(listed["mints"][0]["name"], "satoshi", "{listed}");
        assert_eq!(listed["mints"].as_array().unwrap().len(), 1);

        let log = std::process::Command::new("git")
            .args(["log", "--format=%s", "main"])
            .current_dir(&remote)
            .output()
            .unwrap();
        let log = String::from_utf8_lossy(&log.stdout);
        assert_eq!(log.lines().count(), 1, "{log}");
        assert!(log.starts_with("run: provision satoshi for npub1"), "{log}");
        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
