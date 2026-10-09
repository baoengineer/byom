//! Direct ChatGPT-plan OAuth. No credential import from other tools and no API-key fallback.
use crate::store;
use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(test)]
use serde_json::json;
use sha2::{Digest, Sha256};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Provider key for the ChatGPT session in auth.json.
const PROVIDER: &str = "openai";
const ISSUER: &str = "https://auth.openai.com";
const TOKEN: &str = "https://auth.openai.com/api/accounts/oauth/token";
const RESOURCE: &str = "https://api.openai.com/v1";
const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Session {
    access_token: String,
    refresh_token: String,
    id_token: String,
    client_id: String,
    subject: String,
    email: Option<String>,
    scopes: Vec<String>,
    expires_at: u64,
    #[serde(default)]
    earliest_refresh_at: u64,
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .timeout(Duration::from_secs(30))
        .build()?)
}
fn parse(bytes: &[u8]) -> Result<Session> {
    let s: Session = serde_json::from_slice(bytes)
        .map_err(|_| anyhow::anyhow!("Invalid OAuth state; sign in again with byom login"))?;
    if s.client_id.is_empty()
        || s.client_id == "dynamic_agent_client"
        || s.subject.is_empty()
        || s.access_token.is_empty()
        || s.refresh_token.is_empty()
        || !s.scopes.iter().any(|s| s == "chatgpt.tokens.use.direct")
    {
        bail!("ChatGPT plan permission is missing; reconnect");
    }
    Ok(s)
}
/// The stored ChatGPT session, if signed in.
fn load() -> Result<Option<Session>> {
    match store::auth::get(PROVIDER)? {
        None => Ok(None),
        Some(value) => parse(&serde_json::to_vec(&value)?).map(Some),
    }
}
fn save(session: &Session) -> Result<()> {
    store::auth::set(PROVIDER, serde_json::to_value(session)?)
}
fn signed_in() -> Result<Session> {
    load()?.context("Not signed in. Run: byom login")
}
fn token_session(
    v: &Value,
    client_id: &str,
    identity: (&str, Option<String>),
    old: Option<&Session>,
) -> Result<Session> {
    let required = |key: &str| -> Result<String> {
        v.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .with_context(|| format!("OAuth response missing {key}"))
    };
    let scopes: Vec<String> = v
        .get("scope")
        .and_then(Value::as_str)
        .map(|s| s.split_whitespace().map(str::to_owned).collect())
        .or_else(|| old.map(|s| s.scopes.clone()))
        .context("OAuth response missing scopes")?;
    if !scopes.iter().any(|s| s == "chatgpt.tokens.use.direct") {
        bail!("ChatGPT plan usage was not authorized; enable it during login");
    }
    if !v
        .get("token_type")
        .and_then(Value::as_str)
        .is_some_and(|s| s.eq_ignore_ascii_case("bearer"))
    {
        bail!("Unexpected OAuth token type");
    }
    let lifetime = v
        .get("expires_in")
        .and_then(Value::as_u64)
        .filter(|v| *v > 0 && *v <= 86400)
        .context("Invalid token lifetime")?;
    Ok(Session {
        access_token: required("access_token")?,
        refresh_token: required("refresh_token")?,
        id_token: v
            .get("id_token")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| old.map(|s| s.id_token.clone()))
            .context("Missing ID token")?,
        client_id: client_id.into(),
        subject: identity.0.into(),
        email: identity.1,
        scopes,
        expires_at: now() + lifetime,
        earliest_refresh_at: v
            .get("earliest_refresh_at")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    })
}
async fn exchange(c: &reqwest::Client, fields: &[(&str, &str)]) -> Result<Value> {
    exchange_at(c, TOKEN, fields).await
}
async fn exchange_at(
    c: &reqwest::Client,
    endpoint: &str,
    fields: &[(&str, &str)],
) -> Result<Value> {
    let r = c
        .post(endpoint)
        .form(fields)
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("OAuth service unavailable"))?;
    if !r.status().is_success() {
        bail!(
            "OAuth token exchange failed (HTTP {}); reconnect if access was revoked",
            r.status().as_u16()
        );
    }
    r.json()
        .await
        .map_err(|_| anyhow::anyhow!("Invalid OAuth service response"))
}
async fn validate_identity(
    c: &reqwest::Client,
    token: &str,
    client_id: &str,
    nonce: &str,
) -> Result<(String, Option<String>)> {
    let discovery: Value = c
        .get(format!("{ISSUER}/.well-known/openid-configuration"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    if discovery["issuer"].as_str() != Some(ISSUER) {
        bail!("Unexpected OpenAI discovery issuer");
    }
    let uri = discovery["jwks_uri"]
        .as_str()
        .context("Missing OpenAI JWKS URI")?;
    let url = reqwest::Url::parse(uri)?;
    if url.scheme() != "https" || url.host_str() != Some("auth.openai.com") {
        bail!("Unexpected OpenAI JWKS destination");
    }
    let keys: JwkSet = c.get(url).send().await?.error_for_status()?.json().await?;
    verify_identity(token, client_id, nonce, &keys)
}
fn verify_identity(
    token: &str,
    client_id: &str,
    nonce: &str,
    keys: &JwkSet,
) -> Result<(String, Option<String>)> {
    let h = decode_header(token).map_err(|_| anyhow::anyhow!("Invalid ID token"))?;
    if h.alg != Algorithm::RS256 {
        bail!("Unexpected ID token signing algorithm");
    }
    let jwk = keys
        .find(h.kid.as_deref().context("Missing ID token key ID")?)
        .context("Unknown ID token signing key")?;
    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_issuer(&[ISSUER]);
    validation.set_audience(&[client_id]);
    validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
    validation.validate_nbf = true;
    let claims = decode::<Value>(token, &DecodingKey::from_jwk(jwk)?, &validation)
        .map_err(|_| anyhow::anyhow!("ID token signature or identity validation failed"))?
        .claims;
    if claims["nonce"].as_str() != Some(nonce) {
        bail!("ID token nonce mismatch");
    }
    Ok((
        claims["sub"]
            .as_str()
            .filter(|s| !s.is_empty())
            .context("Missing validated subject")?
            .into(),
        claims["email"].as_str().map(str::to_owned),
    ))
}
fn callback(query: &str, state: &str, previous: Option<&str>) -> Result<(String, String)> {
    let pairs: std::collections::HashMap<String, String> =
        reqwest::Url::parse(&format!("http://127.0.0.1/auth/callback?{query}"))?
            .query_pairs()
            .into_owned()
            .collect();
    if pairs.get("state").map(String::as_str) != Some(state) {
        bail!("OAuth state mismatch");
    }
    if pairs.contains_key("error") {
        bail!("ChatGPT authorization declined; no credentials changed");
    }
    let id = pairs
        .get("client_id")
        .map(String::as_str)
        .or(previous)
        .context("Registration callback missing issued client ID")?;
    if id == "dynamic_agent_client" || id.is_empty() || previous.is_some_and(|p| p != id) {
        bail!("Registration client ID mismatch");
    }
    Ok((
        pairs
            .get("code")
            .filter(|s| !s.is_empty())
            .context("Missing authorization code")?
            .clone(),
        id.into(),
    ))
}
pub async fn login() -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let _lock = store::lock().await?;
    let previous = load()?;
    let host_path = store::home()?.join("host-id");
    let host = match store::read_private(&host_path)? {
        Some(bytes) => String::from_utf8(bytes)?,
        None => {
            let h = format!("urn:uuid:{}", uuid::Uuid::new_v4());
            store::write_private(&host_path, h.as_bytes())?;
            h
        }
    };
    uuid::Uuid::parse_str(
        host.strip_prefix("urn:uuid:")
            .context("Invalid saved host ID")?,
    )?;
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    let redirect = format!(
        "http://127.0.0.1:{}/auth/callback",
        listener.local_addr()?.port()
    );
    let random = || URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>());
    let verifier = random();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random();
    let nonce = random();
    let mut url = reqwest::Url::parse("https://auth.openai.com/api/accounts/authorize")?;
    {
        let mut q = url.query_pairs_mut();
        q.extend_pairs([
            (
                "client_id",
                previous
                    .as_ref()
                    .map(|s| s.client_id.as_str())
                    .unwrap_or("dynamic_agent_client"),
            ),
            ("ext_agent_host_id", host.as_str()),
            ("response_type", "code"),
            ("redirect_uri", redirect.as_str()),
            ("resource", RESOURCE),
            ("scope", SCOPES),
            ("state", state.as_str()),
            ("nonce", nonce.as_str()),
            ("code_challenge_method", "S256"),
            ("code_challenge", challenge.as_str()),
        ]);
        if previous.is_none() {
            q.append_pair("agent_name_hint", "byom");
        }
    }
    println!("Continue with ChatGPT:\n{url}\nWaiting for browser login (5 minutes)…");
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(opener)
        .arg(url.as_str())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    let result=tokio::time::timeout(Duration::from_secs(300),async {
        loop {
            let (mut socket,_)=listener.accept().await?;
            let mut bytes=Vec::new(); let mut chunk=[0u8;1024];
            loop { let n=tokio::time::timeout(Duration::from_secs(3),socket.read(&mut chunk)).await??;if n==0 {break;} bytes.extend_from_slice(&chunk[..n]);if bytes.windows(4).any(|w|w==b"\r\n\r\n") || bytes.len()>16384 {break;} }
            let line=std::str::from_utf8(&bytes).unwrap_or("").lines().next().unwrap_or("");
            let target=line.strip_prefix("GET ").and_then(|s|s.split_whitespace().next()).unwrap_or("");
            let Some(query)=target.strip_prefix("/auth/callback?") else { let _=socket.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;continue; };
            match callback(query,&state,previous.as_ref().map(|s|s.client_id.as_str())) {
                Ok(value)=>{let text="Login received. Return to your terminal to confirm completion.";let reply=format!("HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",text.len());socket.write_all(reply.as_bytes()).await?;return Ok::<_,anyhow::Error>(value);},
                Err(e)=>{let _=socket.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;if format!("{e}").contains("declined") { return Err(e); }}
            }
        }
    }).await.context("Browser login timed out; run byom login again")??;
    let c = client()?;
    let token = exchange(
        &c,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", &result.1),
            ("code", &result.0),
            ("code_verifier", &verifier),
            ("redirect_uri", &redirect),
            ("resource", RESOURCE),
        ],
    )
    .await?;
    let identity = validate_identity(
        &c,
        token["id_token"].as_str().context("Missing ID token")?,
        &result.1,
        &nonce,
    )
    .await?;
    if previous.as_ref().is_some_and(|s| s.subject != identity.0) {
        bail!("Returning login identity mismatch; existing credentials unchanged");
    }
    let s = token_session(&token, &result.1, (&identity.0, identity.1), None)?;
    save(&s)?;
    println!(
        "Signed in. Using ChatGPT plan—not an API key. Manage usage: https://chatgpt.com/settings/usage\nRun `byom models` to see your available model IDs."
    );
    Ok(())
}
/// A valid access token for the signed-in account, refreshing it when near expiry.
pub async fn access_token() -> Result<String> {
    let _lock = store::lock().await?;
    let mut s = signed_in()?;
    if now() + 180 >= s.expires_at {
        if now() < s.earliest_refresh_at {
            bail!("Token renewal is not yet permitted; retry later");
        }
        let token = exchange(
            &client()?,
            &[
                ("grant_type", "refresh_token"),
                ("client_id", &s.client_id),
                ("refresh_token", &s.refresh_token),
                ("resource", RESOURCE),
            ],
        )
        .await?;
        let next = token_session(
            &token,
            &s.client_id,
            (&s.subject, s.email.clone()),
            Some(&s),
        )?;
        save(&next)?;
        s = next;
    }
    Ok(s.access_token)
}
pub fn status() -> Result<()> {
    let s = signed_in()?;
    println!(
        "ChatGPT subscription OAuth: signed in; token {}",
        if now() < s.expires_at {
            "valid"
        } else {
            "needs refresh"
        }
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_real_signature_identity_and_nonce() {
        // Public test-only signing fixture; never usable as an OpenAI credential.
        let key = jsonwebtoken::EncodingKey::from_rsa_pem(include_bytes!(
            "../tests/fixtures/oauth-test-key.pem"
        ))
        .unwrap();
        let keys: JwkSet =
            serde_json::from_str(include_str!("../tests/fixtures/oauth-test-jwks.json")).unwrap();
        let mut header = jsonwebtoken::Header::new(Algorithm::RS256);
        header.kid = Some("test-key".into());
        let claims = json!({"iss":ISSUER,"aud":"issued","sub":"test-subject","exp":now()+3600,"nonce":"expected","email":"test@example.invalid"});
        let token = jsonwebtoken::encode(&header, &claims, &key).unwrap();
        assert_eq!(
            verify_identity(&token, "issued", "expected", &keys)
                .unwrap()
                .0,
            "test-subject"
        );
        assert!(verify_identity(&token, "wrong-client", "expected", &keys).is_err());
        assert!(verify_identity(&token, "issued", "wrong-nonce", &keys).is_err());
        let mut expired = claims.clone();
        expired["exp"] = json!(now() - 3600);
        assert!(
            verify_identity(
                &jsonwebtoken::encode(&header, &expired, &key).unwrap(),
                "issued",
                "expected",
                &keys
            )
            .is_err()
        );
        let mut wrong_issuer = claims;
        wrong_issuer["iss"] = json!("https://example.invalid");
        assert!(
            verify_identity(
                &jsonwebtoken::encode(&header, &wrong_issuer, &key).unwrap(),
                "issued",
                "expected",
                &keys
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn mock_oauth_code_exchange_refresh_and_persistence() {
        use axum::{Router, extract::Form, routing::post};
        use std::collections::HashMap;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let app = Router::new().route("/token", post(move |Form(fields): Form<HashMap<String,String>>| {
            let tx = tx.clone(); async move {
                tx.send(fields.clone()).unwrap();
                let refresh = fields["grant_type"] == "refresh_token";
                axum::Json(json!({"access_token":if refresh {"fresh-access"} else {"first-access"},"refresh_token":if refresh {"rotated-refresh"} else {"initial-refresh"},"id_token":"test-id-token","token_type":"Bearer","expires_in":3600,"scope":SCOPES}))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let c = client().unwrap();
        let token = exchange_at(
            &c,
            &endpoint,
            &[
                ("grant_type", "authorization_code"),
                ("client_id", "issued"),
                ("code", "dummy-code"),
                ("code_verifier", "dummy-verifier"),
                ("resource", RESOURCE),
            ],
        )
        .await
        .unwrap();
        let fields = rx.recv().await.unwrap();
        assert_eq!(fields["client_id"], "issued");
        assert_eq!(fields["code_verifier"], "dummy-verifier");
        let s = token_session(&token, "issued", ("subject", None), None).unwrap();
        let loaded = parse(&serde_json::to_vec(&s).unwrap()).unwrap();
        let refreshed = exchange_at(
            &c,
            &endpoint,
            &[
                ("grant_type", "refresh_token"),
                ("client_id", &loaded.client_id),
                ("refresh_token", &loaded.refresh_token),
                ("resource", RESOURCE),
            ],
        )
        .await
        .unwrap();
        let fields = rx.recv().await.unwrap();
        assert_eq!(fields["refresh_token"], "initial-refresh");
        assert_eq!(fields["resource"], RESOURCE);
        assert!(!fields.contains_key("scope"));
        let next = token_session(
            &refreshed,
            &loaded.client_id,
            (&loaded.subject, None),
            Some(&loaded),
        )
        .unwrap();
        let saved = parse(&serde_json::to_vec(&next).unwrap()).unwrap();
        assert_eq!(saved.access_token, "fresh-access");
        assert_eq!(saved.refresh_token, "rotated-refresh");
        assert_eq!(saved.subject, "subject");
        server.abort();
    }

    #[test]
    fn callback_checks_state_and_registration() {
        assert!(callback("code=c&state=wrong&client_id=oaiapp_a", "s", None).is_err());
        assert!(callback("code=c&state=s", "s", None).is_err());
        assert!(callback("code=c&state=s&client_id=other", "s", Some("saved")).is_err());
        assert_eq!(
            callback("code=c&state=s", "s", Some("saved")).unwrap(),
            ("c".into(), "saved".into())
        );
        assert!(callback("error=access_denied&state=s", "s", None).is_err());
    }
    #[test]
    fn validates_scope_and_rotation() {
        let v = json!({"access_token":"dummy","refresh_token":"rotated","id_token":"id","token_type":"Bearer","expires_in":3600,"scope":SCOPES});
        let s = token_session(&v, "issued", ("subject", None), None).unwrap();
        assert_eq!(s.refresh_token, "rotated");
        assert!(s.expires_at > now());
        let mut denied = v.clone();
        denied["scope"] = json!("openid");
        assert!(token_session(&denied, "issued", ("subject", None), None).is_err());
    }
}
