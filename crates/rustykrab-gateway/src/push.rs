//! APNs push for pending credential requests (Workstream E).
//!
//! When the agent asks to change a credential it cannot change itself, the
//! user has to hear about it. Until now that meant opening the app or
//! WebChat; this sends a notification instead.
//!
//! **The payload carries the credential's name and nothing else.** A push
//! traverses Apple's infrastructure and lands on a lock screen, so a value
//! must never be in it — the threat model says the same.
//!
//! Authentication is a token, not a certificate: an ES256 JWT signed with
//! the team's `.p8` key. One key covers every app and both environments,
//! which is why the endpoint has to be configured rather than inferred.

use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ring::rand::SystemRandom;
use ring::signature::{EcdsaKeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
use rustykrab_core::Error;

/// Apple rejects a token older than an hour and refuses one refreshed more
/// often than every 20 minutes. Sit in the middle.
const TOKEN_REFRESH: Duration = Duration::from_secs(45 * 60);

/// Which APNs environment to talk to.
///
/// This is not detectable at runtime: a development-signed build talks to
/// sandbox and a TestFlight or App Store build talks to production, and
/// sending to the wrong one fails with `BadDeviceToken`. Getting it wrong
/// is silent from the app's side, which is why it is explicit here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApnsEnvironment {
    Sandbox,
    Production,
}

impl ApnsEnvironment {
    fn host(&self) -> &'static str {
        match self {
            ApnsEnvironment::Sandbox => "api.sandbox.push.apple.com",
            ApnsEnvironment::Production => "api.push.apple.com",
        }
    }

    fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "production" | "prod" => ApnsEnvironment::Production,
            // Default to sandbox: a development build is what someone has
            // in hand while setting this up, and sending a real push to
            // production by accident is the worse mistake.
            _ => ApnsEnvironment::Sandbox,
        }
    }
}

/// Everything needed to authenticate to APNs, minus the key material.
#[derive(Debug, Clone)]
pub struct ApnsConfig {
    /// 10-character key identifier, e.g. `NP9F64W9GB`.
    pub key_id: String,
    /// 10-character team identifier, e.g. `3RRX845C4X`.
    pub team_id: String,
    /// The app's bundle id, sent as `apns-topic`.
    pub topic: String,
    pub environment: ApnsEnvironment,
}

impl ApnsConfig {
    /// Read the non-secret settings from the environment. The signing key
    /// is deliberately not here — it lives in the encrypted store.
    ///
    /// Returns `None` when push isn't configured, which is the normal state
    /// for a deployment that hasn't set it up.
    pub fn from_env() -> Option<Self> {
        let key_id = non_empty("RUSTYKRAB_APNS_KEY_ID")?;
        let team_id = non_empty("RUSTYKRAB_APNS_TEAM_ID")?;
        let topic = non_empty("RUSTYKRAB_APNS_TOPIC")?;
        let environment = std::env::var("RUSTYKRAB_APNS_ENVIRONMENT")
            .map(|v| ApnsEnvironment::parse(&v))
            .unwrap_or(ApnsEnvironment::Sandbox);
        Some(Self {
            key_id,
            team_id,
            topic,
            environment,
        })
    }
}

fn non_empty(key: &str) -> Option<String> {
    let value = std::env::var(key).ok()?;
    let value = value.trim().to_string();
    (!value.is_empty()).then_some(value)
}

/// Strip the PEM armour from a `.p8` and return the PKCS#8 DER inside.
///
/// Apple hands out the key as PEM; `ring` wants DER.
pub fn pkcs8_from_pem(pem: &str) -> Result<Vec<u8>, Error> {
    let body: String = pem
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect::<Vec<_>>()
        .join("");
    if body.trim().is_empty() {
        return Err(Error::Config(
            "APNs key is empty or not PEM — expected a .p8 with a \
             BEGIN PRIVATE KEY header"
                .into(),
        ));
    }
    base64::engine::general_purpose::STANDARD
        .decode(body.trim())
        .map_err(|e| Error::Config(format!("APNs key is not valid base64: {e}")))
}

/// Mints and caches the provider token.
struct TokenCache {
    token: String,
    issued_at: SystemTime,
}

/// Client for one team's APNs connection.
pub struct ApnsClient {
    config: ApnsConfig,
    key_der: Vec<u8>,
    http: reqwest::Client,
    cached: RwLock<Option<TokenCache>>,
}

impl ApnsClient {
    /// Build a client from the config and the PEM contents of the `.p8`.
    ///
    /// The key is parsed once here so a malformed one is a startup error
    /// rather than a surprise at the moment a notification matters.
    pub fn new(config: ApnsConfig, key_pem: &str) -> Result<Self, Error> {
        let key_der = pkcs8_from_pem(key_pem)?;
        // Parse once to validate; the parsed pair is not Send+Sync so it is
        // rebuilt per signature.
        let rng = SystemRandom::new();
        EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &key_der, &rng).map_err(
            |_| {
                Error::Config(
                    "APNs key is not a P-256 private key — check that the .p8 \
                     is the APNs auth key and not another credential"
                        .into(),
                )
            },
        )?;
        Ok(Self {
            config,
            key_der,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .map_err(|e| Error::Config(format!("APNs HTTP client: {e}")))?,
            cached: RwLock::new(None),
        })
    }

    /// The current provider token, minting a fresh one when it ages out.
    fn provider_token(&self) -> Result<String, Error> {
        if let Some(cached) = self
            .cached
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            if cached.issued_at.elapsed().unwrap_or(TOKEN_REFRESH) < TOKEN_REFRESH {
                return Ok(cached.token.clone());
            }
        }
        let token = self.mint_token()?;
        *self.cached.write().unwrap_or_else(|e| e.into_inner()) = Some(TokenCache {
            token: token.clone(),
            issued_at: SystemTime::now(),
        });
        Ok(token)
    }

    /// Build and sign the ES256 JWT Apple expects.
    fn mint_token(&self) -> Result<String, Error> {
        let issued_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| Error::Internal(format!("system clock before epoch: {e}")))?
            .as_secs();

        let header = serde_json::json!({
            "alg": "ES256",
            "kid": self.config.key_id,
        });
        let claims = serde_json::json!({
            "iss": self.config.team_id,
            "iat": issued_at,
        });

        let signing_input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header)?),
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims)?)
        );

        let rng = SystemRandom::new();
        let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &self.key_der, &rng)
            .map_err(|_| Error::Config("APNs key could not be loaded".into()))?;
        let signature = key
            .sign(&rng, signing_input.as_bytes())
            .map_err(|_| Error::Internal("signing the APNs token failed".into()))?;

        Ok(format!(
            "{signing_input}.{}",
            URL_SAFE_NO_PAD.encode(signature.as_ref())
        ))
    }

    /// Tell one device that a credential change is waiting.
    ///
    /// `credential_name` is the only detail that travels: enough for the
    /// user to know what they are being asked about, and nothing that would
    /// be damaging on a lock screen or in Apple's logs.
    pub async fn notify_pending_request(
        &self,
        device_token: &str,
        credential_name: &str,
        action: &str,
    ) -> Result<(), Error> {
        let body = serde_json::json!({
            "aps": {
                "alert": {
                    "title": "Approval needed",
                    "body": format!("The agent wants to {action} “{credential_name}”."),
                },
                "sound": "default",
                // Drives the app's badge; the app recomputes it on launch.
                "badge": 1,
            },
            // Deep-links to the Approvals tab. A name, never a value.
            "credentialName": credential_name,
            "action": action,
        });

        let url = format!(
            "https://{}/3/device/{}",
            self.config.environment.host(),
            device_token
        );
        let response = self
            .http
            .post(&url)
            .bearer_auth(self.provider_token()?)
            .header("apns-topic", &self.config.topic)
            .header("apns-push-type", "alert")
            // Approval prompts are worth waking the device for.
            .header("apns-priority", "10")
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Channel(format!("APNs request failed: {e}")))?;

        if response.status().is_success() {
            return Ok(());
        }
        let status = response.status();
        // Apple explains refusals in the body; without it "410" is a riddle.
        let reason = response.text().await.unwrap_or_default();
        Err(Error::Channel(format!(
            "APNs rejected the push ({status}): {reason}"
        )))
    }

    /// True when Apple rejected our *provider* token — a rotated key or a
    /// token outside its validity window — as opposed to the device's.
    pub fn is_stale_provider_token(error: &Error) -> bool {
        let text = error.to_string();
        text.contains("InvalidProviderToken") || text.contains("ExpiredProviderToken")
    }

    /// True when Apple says the *device* token is no longer valid for this
    /// app, so the caller can stop sending to it.
    pub fn is_dead_token(error: &Error) -> bool {
        let text = error.to_string();
        text.contains("BadDeviceToken") || text.contains("Unregistered")
    }
}

/// Sends a notification to every registered device.
///
/// The signing key is resolved **lazily**, not at startup. It is a
/// credential like any other, so it can be stored from the app or the CLI
/// while the daemon is already running — reading it at boot would mean
/// storing the key appeared to do nothing until a restart. The same
/// property makes key rotation work: an authentication failure drops the
/// cached client so the next notification picks up the new key.
#[derive(Clone)]
pub struct PushNotifier {
    config: ApnsConfig,
    secrets: rustykrab_store::SecretStore,
    devices: rustykrab_store::DeviceStore,
    client: Arc<RwLock<Option<Arc<ApnsClient>>>>,
}

impl std::fmt::Debug for PushNotifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // No key material or tokens in the debug output.
        f.debug_struct("PushNotifier")
            .field("topic", &self.config.topic)
            .field("environment", &self.config.environment)
            .finish()
    }
}

/// Bridges the store's notification hook to APNs.
///
/// `request_filed` is synchronous and must not block the write that
/// triggered it, so the send is spawned. A credential change is protected
/// by the request row, not by the notification.
impl rustykrab_store::RequestNotifier for PushNotifier {
    fn request_filed(&self, credential_name: &str, action: &str) {
        let notifier = self.clone();
        let name = credential_name.to_string();
        let action = action.to_string();
        tokio::spawn(async move {
            notifier.announce(&name, &action).await;
        });
    }
}

impl PushNotifier {
    pub fn new(
        config: ApnsConfig,
        secrets: rustykrab_store::SecretStore,
        devices: rustykrab_store::DeviceStore,
    ) -> Self {
        Self {
            config,
            secrets,
            devices,
            client: Arc::new(RwLock::new(None)),
        }
    }

    /// The client, building it from the stored key on first use.
    ///
    /// Returns `None` when no key is stored yet — push is optional, and a
    /// deployment that never sets one should not produce errors.
    async fn client(&self) -> Option<Arc<ApnsClient>> {
        if let Some(existing) = self
            .client
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            return Some(Arc::clone(existing));
        }

        // Resolve through the registry so the key can come from an env
        // var, the OS keychain, or the encrypted store — the same
        // resolution order as every other credential.
        let spec = rustykrab_store::registry::lookup("apns_auth_key")?;
        let key_pem = rustykrab_store::registry::resolve(spec, &self.secrets).await?;

        match ApnsClient::new(self.config.clone(), &key_pem) {
            Ok(built) => {
                let built = Arc::new(built);
                *self.client.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(&built));
                tracing::info!(
                    topic = %self.config.topic,
                    environment = ?self.config.environment,
                    "APNs client ready"
                );
                Some(built)
            }
            Err(e) => {
                tracing::error!(error = %e, "stored APNs key is unusable — push disabled");
                None
            }
        }
    }

    /// Drop the cached client so the next send re-reads the key. Used when
    /// Apple rejects the provider token, which is what a rotated key looks
    /// like from here.
    fn invalidate_client(&self) {
        *self.client.write().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Notify all devices about a filed request.
    ///
    /// Best-effort by design: a push that fails must never stop a request
    /// being recorded, because the request is the thing that protects the
    /// credential. The app and WebChat both show pending requests without
    /// any notification at all.
    pub async fn announce(&self, credential_name: &str, action: &str) {
        let Some(client) = self.client().await else {
            tracing::debug!("no APNs key stored — skipping push");
            return;
        };
        let devices = match self.devices.with_push_tokens().await {
            Ok(devices) => devices,
            Err(e) => {
                tracing::warn!(error = %e, "could not list devices for push");
                return;
            }
        };
        for (device_id, token) in devices {
            match client
                .notify_pending_request(&token, credential_name, action)
                .await
            {
                Ok(()) => tracing::info!(device = %device_id, "approval push sent"),
                Err(e) if ApnsClient::is_stale_provider_token(&e) => {
                    // The key was rotated, or the token drifted out of
                    // Apple's window. Rebuild on the next notification.
                    tracing::info!("APNs provider token rejected — rebuilding client");
                    self.invalidate_client();
                }
                Err(e) if ApnsClient::is_dead_token(&e) => {
                    // The app was deleted or the token was reissued. Drop it
                    // rather than retrying forever.
                    tracing::info!(device = %device_id, "clearing dead push token");
                    let _ = self.devices.clear_push_token(&device_id).await;
                }
                Err(e) => tracing::warn!(device = %device_id, error = %e, "approval push failed"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A P-256 key in PKCS#8 PEM, generated for this test only.
    fn test_key_pem() -> String {
        let rng = SystemRandom::new();
        let doc = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng)
            .expect("generate key");
        let b64 = base64::engine::general_purpose::STANDARD.encode(doc.as_ref());
        format!("-----BEGIN PRIVATE KEY-----\n{b64}\n-----END PRIVATE KEY-----\n")
    }

    fn config() -> ApnsConfig {
        ApnsConfig {
            key_id: "NP9F64W9GB".into(),
            team_id: "3RRX845C4X".into(),
            topic: "com.gcbh.apollo".into(),
            environment: ApnsEnvironment::Sandbox,
        }
    }

    #[test]
    fn pem_armour_is_stripped() {
        let pem = test_key_pem();
        let der = pkcs8_from_pem(&pem).expect("decode");
        assert!(!der.is_empty());
        // The armour lines must not survive into the DER.
        assert!(!String::from_utf8_lossy(&der).contains("BEGIN"));
    }

    #[test]
    fn a_key_that_is_not_pem_is_rejected_at_construction() {
        // A bad key should fail at startup, not at the moment a
        // notification matters.
        let err = match ApnsClient::new(config(), "not a key at all") {
            Err(e) => e,
            Ok(_) => panic!("garbage was accepted as an APNs key"),
        };
        assert!(
            err.to_string().contains("base64") || err.to_string().contains("PEM"),
            "unhelpful error: {err}"
        );
    }

    #[test]
    fn token_has_three_parts_with_apples_header_and_claims() {
        let client = ApnsClient::new(config(), &test_key_pem()).expect("client");
        let token = client.provider_token().expect("token");

        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3, "a JWT has three parts: {token}");

        let header: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
        assert_eq!(header["alg"], "ES256");
        assert_eq!(header["kid"], "NP9F64W9GB");

        let claims: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["iss"], "3RRX845C4X");
        assert!(claims["iat"].as_u64().unwrap() > 1_700_000_000);
        // A provider token must not carry an expiry; Apple derives it.
        assert!(claims.get("exp").is_none());

        // Base64url, not base64: no padding or non-URL-safe characters.
        assert!(!token.contains('=') && !token.contains('+') && !token.contains('/'));
    }

    #[test]
    fn the_token_is_reused_until_it_ages_out() {
        let client = ApnsClient::new(config(), &test_key_pem()).expect("client");
        let first = client.provider_token().unwrap();
        let second = client.provider_token().unwrap();
        // Apple refuses tokens refreshed more often than every 20 minutes,
        // so minting per request would get us throttled.
        assert_eq!(first, second);
    }

    #[test]
    fn environment_defaults_to_sandbox_and_picks_the_right_host() {
        assert_eq!(
            ApnsEnvironment::parse("production"),
            ApnsEnvironment::Production
        );
        assert_eq!(ApnsEnvironment::parse("PROD"), ApnsEnvironment::Production);
        assert_eq!(ApnsEnvironment::parse("sandbox"), ApnsEnvironment::Sandbox);
        // Anything unrecognised is sandbox: sending a real push to
        // production by accident is the worse mistake.
        assert_eq!(ApnsEnvironment::parse("typo"), ApnsEnvironment::Sandbox);

        assert_eq!(ApnsEnvironment::Production.host(), "api.push.apple.com");
        assert_eq!(
            ApnsEnvironment::Sandbox.host(),
            "api.sandbox.push.apple.com"
        );
    }

    #[test]
    fn dead_tokens_are_recognised() {
        let dead =
            Error::Channel("APNs rejected the push (410): {\"reason\":\"Unregistered\"}".into());
        assert!(ApnsClient::is_dead_token(&dead));
        let transient = Error::Channel("APNs request failed: connection reset".into());
        assert!(!ApnsClient::is_dead_token(&transient));
    }
}
