//! ssh-agent authentication (#2, #10).
//!
//! Looks for an ssh-agent running on this device and offers the keys it holds
//! during public-key authentication, with the agent producing every signature,
//! so private keys never leave it. That covers smartcards and hardware tokens
//! (PIV, OpenPGP, FIDO `sk-*` keys) and OpenSSH certificates held by the agent.
//!
//! Device-local only: nothing about the agent is stored in a profile or synced,
//! and the agent is only ever asked to sign this client's own login. Agent
//! forwarding is never requested, and agent channels a server tries to open are
//! refused (`ClientHandler::server_channel_open_agent_forward`).
//!
//! Which agent: `IdentityAgent` in `~/.ssh/config` when it applies to the host
//! (see `find_agent_for`), otherwise the first one found (`find_agent`).
//!
//! The node's own credentials are tried exactly as before. The agent is used
//! only (a) where a passphrase or password would otherwise have to be asked
//! for, and (b) after the node's own key was turned down. See the callers in
//! lib.rs.

use std::time::Duration;

use russh::client::{self, AuthResult};
use russh::keys::agent::client::{AgentClient, AgentStream};
use russh::keys::agent::AgentIdentity;
use russh::keys::{HashAlg, PublicKey};
use russh::{AgentAuthError, MethodKind, MethodSet};

/// How long an agent may take to answer while we look for one (connect and
/// list its keys). A missing agent fails at once; this only bounds a hung one.
const AGENT_LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);

/// How long one key offer may take, signature included. Generous: the agent
/// may be waiting for a PIN or a touch on a hardware key.
pub(crate) const AGENT_SIGN_TIMEOUT: Duration = Duration::from_secs(120);

/// Agent keys offered per connection. Each key the server turns down counts
/// against its `MaxAuthTries` (6 by default), so three leaves room for the
/// password prompt that may follow (up to three tries).
pub(crate) const AGENT_MAX_ATTEMPTS: usize = 3;

type AgentConn = AgentClient<Box<dyn AgentStream + Send + Unpin + 'static>>;

/// A connected agent and the keys it holds, for one login.
pub(crate) struct AgentSession {
    client: AgentConn,
    identities: Vec<AgentIdentity>,
    /// Where the agent was found, for the connect log.
    pub(crate) source: &'static str,
    /// Wire blobs of the keys already offered on this connection, by any route
    /// (the node's own key file included), so none is offered twice.
    offered: Vec<Vec<u8>>,
    /// Agent keys offered so far on this connection.
    attempts: usize,
}

impl AgentSession {
    /// Wrap a connected agent and list its keys. `None` if it holds no key or
    /// doesn't answer in time.
    async fn new(mut client: AgentConn, source: &'static str) -> Option<Self> {
        match tokio::time::timeout(AGENT_LOOKUP_TIMEOUT, client.request_identities()).await {
            Ok(Ok(identities)) if !identities.is_empty() => Some(Self {
                client,
                identities,
                source,
                offered: Vec::new(),
                attempts: 0,
            }),
            _ => None,
        }
    }

    pub(crate) fn key_count(&self) -> usize {
        self.identities.len()
    }

    /// Record that `key` was offered already (as the node's own key file), so
    /// it isn't offered again through the agent.
    pub(crate) fn mark_offered(&mut self, key: &PublicKey) {
        if let Some(blob) = key_blob(key) {
            if !self.offered.contains(&blob) {
                self.offered.push(blob);
            }
        }
    }
}

/// Look for an ssh-agent on this device that holds at least one key: an
/// `SSH_AUTH_SOCK` naming a named pipe first (gpg4win, KeeAgent and the like —
/// Windows' own OpenSSH client honours it too), then the Windows OpenSSH agent
/// service, then Pageant. The first that answers with a key is used.
#[cfg(windows)]
async fn find_agent() -> Option<AgentSession> {
    if let Ok(sock) = std::env::var("SSH_AUTH_SOCK") {
        if is_pipe_path(&sock) {
            if let Some(agent) = named_pipe(sock.trim(), "the agent at SSH_AUTH_SOCK").await {
                return Some(agent);
            }
        }
    }
    if let Some(agent) = named_pipe(r"\\.\pipe\openssh-ssh-agent", "the Windows OpenSSH agent").await {
        return Some(agent);
    }
    let client = tokio::time::timeout(AGENT_LOOKUP_TIMEOUT, AgentClient::connect_pageant())
        .await
        .ok()?
        .ok()?;
    AgentSession::new(client.dynamic(), "Pageant").await
}

#[cfg(windows)]
async fn named_pipe(path: &str, source: &'static str) -> Option<AgentSession> {
    // `connect_named_pipe` retries for as long as the pipe is busy: bound it.
    let client = tokio::time::timeout(AGENT_LOOKUP_TIMEOUT, AgentClient::connect_named_pipe(path))
        .await
        .ok()?
        .ok()?;
    AgentSession::new(client.dynamic(), source).await
}

/// `\\.\pipe\<name>` (either slash direction). Anything else in
/// `SSH_AUTH_SOCK` on Windows — a Cygwin/MSYS socket emulation, say — isn't
/// something a native client can open.
#[cfg(windows)]
fn is_pipe_path(path: &str) -> bool {
    let p = path.trim().replace('/', "\\").to_ascii_lowercase();
    p.len() > r"\\.\pipe\".len() && p.starts_with(r"\\.\pipe\")
}

/// Look for an ssh-agent on this device that holds at least one key: the one
/// `SSH_AUTH_SOCK` points at.
#[cfg(unix)]
async fn find_agent() -> Option<AgentSession> {
    let client = tokio::time::timeout(AGENT_LOOKUP_TIMEOUT, AgentClient::connect_env())
        .await
        .ok()?
        .ok()?;
    AgentSession::new(client.dynamic(), "the agent at SSH_AUTH_SOCK").await
}

#[cfg(not(any(unix, windows)))]
async fn find_agent() -> Option<AgentSession> {
    None
}

/// The source name for an agent `IdentityAgent` chose, in the connect log.
const IDENTITY_AGENT_SOURCE: &str = "the agent set by IdentityAgent";

/// The agent for a connection. If `IdentityAgent` in `~/.ssh/config` applies
/// (`setting`), it decides, as in OpenSSH: only that agent is used, or none.
/// Otherwise the usual lookup (`find_agent`). `log` explains when the config
/// ruled an agent out.
pub(crate) async fn find_agent_for(
    setting: Option<crate::ssh_config::IdentityAgent>,
    mut log: impl FnMut(String),
) -> Option<AgentSession> {
    use crate::ssh_config::IdentityAgent;
    match setting {
        None => find_agent().await,
        Some(IdentityAgent::Disabled) => {
            log("~/.ssh/config sets IdentityAgent none for this host, so no ssh-agent is used.".into());
            None
        }
        Some(IdentityAgent::Unusable(value, why)) => {
            log(format!(
                "IdentityAgent {value} in ~/.ssh/config can't be used ({why}), so no ssh-agent is used."
            ));
            None
        }
        Some(IdentityAgent::At(path)) => match agent_at(&path).await {
            Ok(agent) => Some(agent),
            Err(why) => {
                log(format!("The agent IdentityAgent in ~/.ssh/config names ({path}) {why}, so no ssh-agent is used."));
                None
            }
        },
    }
}

/// Connect to the agent at `path` and list its keys.
async fn agent_at(path: &str) -> Result<AgentSession, String> {
    let client = connect_at(path).await?;
    AgentSession::new(client, IDENTITY_AGENT_SOURCE)
        .await
        .ok_or_else(|| "holds no keys or didn't answer".to_string())
}

#[cfg(windows)]
async fn connect_at(path: &str) -> Result<AgentConn, String> {
    if !is_pipe_path(path) {
        return Err("isn't a named pipe (\\\\.\\pipe\\…)".into());
    }
    let client = tokio::time::timeout(AGENT_LOOKUP_TIMEOUT, AgentClient::connect_named_pipe(path.trim()))
        .await
        .map_err(|_| "didn't answer".to_string())?
        .map_err(|e| format!("couldn't be opened ({e})"))?;
    Ok(client.dynamic())
}

#[cfg(unix)]
async fn connect_at(path: &str) -> Result<AgentConn, String> {
    let client = tokio::time::timeout(AGENT_LOOKUP_TIMEOUT, AgentClient::connect_uds(path))
        .await
        .map_err(|_| "didn't answer".to_string())?
        .map_err(|e| format!("couldn't be opened ({e})"))?;
    Ok(client.dynamic())
}

#[cfg(not(any(unix, windows)))]
async fn connect_at(_path: &str) -> Result<AgentConn, String> {
    Err("isn't supported on this platform".into())
}

/// The public half of a node's key file, without its passphrase: an
/// OpenSSH-format key keeps the public key in the clear (only the private part
/// is encrypted). `None` for formats that reveal it only once decrypted.
pub(crate) fn key_file_public_key(private_key: &str) -> Option<PublicKey> {
    let normalized = private_key.replace("\r\n", "\n");
    russh::keys::PrivateKey::from_openssh(normalized.trim())
        .ok()
        .map(|key| key.public_key().clone())
}

/// Which of the agent's keys to offer.
pub(crate) enum Offer<'a> {
    /// Only the agent's copy of this key: the node's own key, which the agent
    /// holds unlocked, so no passphrase has to be asked for.
    Only(&'a PublicKey),
    /// The keys not offered yet, within `AGENT_MAX_ATTEMPTS` per connection.
    Rest,
}

/// What came of offering agent keys.
pub(crate) enum AgentOutcome {
    /// A key was accepted: logged in.
    Authenticated,
    /// A key was accepted, but the server wants another factor; `remaining`
    /// are the methods it still offers.
    PartialSuccess { remaining: MethodSet },
    /// Keys were offered and none was accepted. The session is untouched, so
    /// other methods can still be tried on it.
    NotAccepted,
    /// No key was left to offer: none matching, all offered already, or the
    /// connection's attempt budget used up.
    NothingToOffer,
}

/// Offer agent keys to the server one at a time. russh sends each as a probe
/// first and asks the agent to sign only once the server has said it would
/// accept that key, so a hardware key asks for a PIN or a touch only for the
/// key that's actually used.
///
/// An `Err` is FATAL for this session. Once the server has accepted a key,
/// russh's session task waits for the signature and drops every other message
/// until it arrives. If the agent then fails to sign (a declined
/// confirmation, a removed card, a touch never given) or takes longer than
/// `AGENT_SIGN_TIMEOUT`, no other method can run on that session; the caller
/// must give up on it.
pub(crate) async fn offer_keys<H: client::Handler>(
    session: &mut client::Handle<H>,
    user: &str,
    agent: &mut AgentSession,
    which: Offer<'_>,
    mut log: impl FnMut(String) + Send,
) -> Result<AgentOutcome, russh::Error> {
    let only = matches!(which, Offer::Only(_));
    let (picked, skipped) = pick(&agent.identities, &agent.offered, &which, agent.attempts);
    if picked.is_empty() {
        return Ok(AgentOutcome::NothingToOffer);
    }
    let rsa_hash = if picked
        .iter()
        .any(|&i| agent.identities[i].public_key().algorithm().is_rsa())
    {
        session.best_supported_rsa_hash().await?.flatten()
    } else {
        None
    };
    if !only {
        log(format!(
            "Trying {} key{} from {}…",
            picked.len(),
            if picked.len() == 1 { "" } else { "s" },
            agent.source
        ));
    }
    for i in picked {
        let identity = agent.identities[i].clone();
        if let Some(blob) = identity_blob(&identity) {
            agent.offered.push(blob);
        }
        agent.attempts += 1;
        let hash_alg = if identity.public_key().algorithm().is_rsa() { rsa_hash } else { None };
        let label = describe(&identity);
        let attempt = match &identity {
            AgentIdentity::PublicKey { key, .. } => {
                tokio::time::timeout(
                    AGENT_SIGN_TIMEOUT,
                    session.authenticate_publickey_with(user, key.clone(), hash_alg, &mut agent.client),
                )
                .await
            }
            AgentIdentity::Certificate { certificate, .. } => {
                tokio::time::timeout(
                    AGENT_SIGN_TIMEOUT,
                    session.authenticate_certificate_with(user, certificate.clone(), hash_alg, &mut agent.client),
                )
                .await
            }
        };
        let result = match attempt {
            Err(_) => {
                log(format!(
                    "{} did not sign with the {} within {} s (waiting for a PIN or a touch?). This login stops here; connect again to retry.",
                    capitalized(agent.source),
                    label,
                    AGENT_SIGN_TIMEOUT.as_secs()
                ));
                return Err(russh::Error::Keys(russh::keys::Error::AgentFailure));
            }
            Ok(Err(AgentAuthError::Send(_))) => return Err(russh::Error::SendError),
            Ok(Err(AgentAuthError::Key(e))) => {
                log(format!(
                    "{} would not sign with the {} ({}). This login stops here: unlock the agent, insert the key or approve the request, then connect again.",
                    capitalized(agent.source),
                    label,
                    e
                ));
                return Err(russh::Error::Keys(e));
            }
            Ok(Ok(result)) => result,
        };
        // russh also reports a dropped connection as a failure.
        if matches!(result, AuthResult::Failure { .. }) && session.is_closed() {
            return Err(russh::Error::Disconnect);
        }
        match interpret(result) {
            Some(AgentOutcome::Authenticated) => {
                log(format!("Logged in with the {} from {}.", label, agent.source));
                return Ok(AgentOutcome::Authenticated);
            }
            Some(AgentOutcome::PartialSuccess { remaining }) => {
                log(format!(
                    "The {} from {} was accepted; the server asks for another factor.",
                    label, agent.source
                ));
                return Ok(AgentOutcome::PartialSuccess { remaining });
            }
            Some(outcome) => return Ok(outcome),
            None => {}
        }
    }
    if skipped > 0 {
        log(format!(
            "{} holds {} more key{} that {} not tried: the server limits failed attempts. Save the key for this server on the node to use it directly.",
            capitalized(agent.source),
            skipped,
            if skipped == 1 { "" } else { "s" },
            if skipped == 1 { "was" } else { "were" }
        ));
    }
    Ok(AgentOutcome::NotAccepted)
}

/// What the server's answer to one offered key means: an outcome to stop on,
/// or `None` to offer the next key.
fn interpret(result: AuthResult) -> Option<AgentOutcome> {
    match result {
        AuthResult::Success => Some(AgentOutcome::Authenticated),
        AuthResult::Failure { remaining_methods, partial_success: true } => {
            Some(AgentOutcome::PartialSuccess { remaining: remaining_methods })
        }
        // Public keys no longer offered: no point offering another.
        AuthResult::Failure { remaining_methods, .. } if !remaining_methods.contains(&MethodKind::PublicKey) => {
            Some(AgentOutcome::NotAccepted)
        }
        AuthResult::Failure { .. } => None,
    }
}

/// The identities to offer now (indices, in the agent's order) and how many
/// fresh ones the attempt budget left out.
fn pick(
    identities: &[AgentIdentity],
    offered: &[Vec<u8>],
    which: &Offer<'_>,
    attempts: usize,
) -> (Vec<usize>, usize) {
    let fresh = |i: usize| identity_blob(&identities[i]).is_some_and(|blob| !offered.contains(&blob));
    match which {
        Offer::Only(wanted) => {
            let hit = (0..identities.len()).find(|&i| {
                fresh(i)
                    && matches!(&identities[i],
                        AgentIdentity::PublicKey { key, .. } if key.key_data() == wanted.key_data())
            });
            (hit.into_iter().collect(), 0)
        }
        Offer::Rest => {
            let fresh: Vec<usize> = (0..identities.len()).filter(|&i| fresh(i)).collect();
            let room = AGENT_MAX_ATTEMPTS.saturating_sub(attempts);
            let skipped = fresh.len().saturating_sub(room);
            (fresh.into_iter().take(room).collect(), skipped)
        }
    }
}

/// `source` at the start of a sentence ("the agent at …" → "The agent at …").
fn capitalized(source: &str) -> String {
    let mut chars = source.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// A key's wire blob. It carries no comment, so the same key matches however
/// each side labels it.
fn key_blob(key: &PublicKey) -> Option<Vec<u8>> {
    PublicKey::new(key.key_data().clone(), "").to_bytes().ok()
}

fn identity_blob(identity: &AgentIdentity) -> Option<Vec<u8>> {
    match identity {
        AgentIdentity::PublicKey { key, .. } => key_blob(key),
        AgentIdentity::Certificate { certificate, .. } => certificate.to_bytes().ok(),
    }
}

/// "ssh-ed25519 key "laptop" (SHA256:…)" for the connect log. The comment comes
/// from the agent, so control characters are dropped and it's kept short.
fn describe(identity: &AgentIdentity) -> String {
    let key = identity.public_key();
    let kind = if matches!(identity, AgentIdentity::Certificate { .. }) { "certificate" } else { "key" };
    let fingerprint = key.fingerprint(HashAlg::Sha256);
    let comment: String = identity
        .comment()
        .chars()
        .filter(|c| !c.is_control())
        .take(64)
        .collect();
    let comment = comment.trim();
    if comment.is_empty() {
        format!("{} {} {}", key.algorithm().as_str(), kind, fingerprint)
    } else {
        format!("{} {} \"{}\" ({})", key.algorithm().as_str(), kind, comment, fingerprint)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ssh_test_server::{self, TestClient, TestServer};
    use russh::keys::agent::server::{Agent, MessageType};
    use russh::keys::PrivateKey;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// A throwaway Ed25519 key per seed.
    fn key(seed: u8) -> PrivateKey {
        use russh::keys::ssh_key::private::{Ed25519Keypair, Ed25519PrivateKey};
        PrivateKey::from(Ed25519Keypair::from(Ed25519PrivateKey::from_bytes(&[seed; 32])))
    }

    /// russh's own agent server; `refuse_signing` makes it turn down every
    /// signature, like a declined confirmation or a removed card.
    #[derive(Clone)]
    struct TestAgent {
        refuse_signing: bool,
    }

    impl Agent for TestAgent {
        fn confirm_request(&self, msg: MessageType) -> impl std::future::Future<Output = bool> + Send {
            let allowed = !(self.refuse_signing && matches!(msg, MessageType::Sign));
            async move { allowed }
        }
    }

    async fn agent_holding(keys: &[PrivateKey], refuse_signing: bool) -> AgentSession {
        let (client_end, server_end) = tokio::io::duplex(64 * 1024);
        let listener = futures::stream::iter(vec![Ok::<_, std::io::Error>(server_end)]);
        tokio::spawn(russh::keys::agent::server::serve(listener, TestAgent { refuse_signing }));
        let mut client = AgentClient::connect(client_end);
        for k in keys {
            client.add_identity(k, &[]).await.expect("agent takes the key");
        }
        AgentSession::new(client.dynamic(), "the test agent").await.expect("agent lists keys")
    }

    /// An unauthenticated session to a server that accepts `accept` (and,
    /// with `second_factor`, then wants a password too), plus its count of
    /// public keys offered.
    async fn session_to(
        accept: Option<&PrivateKey>,
        second_factor: bool,
    ) -> (client::Handle<TestClient>, Arc<AtomicUsize>) {
        let server = TestServer::accepting_key(accept.map(|k| k.public_key().clone()), second_factor);
        let offers = Arc::clone(&server.pk_offers);
        let session = ssh_test_server::handshake(server, client::Config::default())
            .await
            .expect("handshake");
        (session, offers)
    }

    #[tokio::test]
    async fn a_key_the_server_accepts_logs_in() {
        let (a, b) = (key(1), key(2));
        let (mut session, offers) = session_to(Some(&b), false).await;
        let mut agent = agent_holding(&[a, b.clone()], false).await;
        // The agent lists its keys in no fixed order; offers stop at the accepted one.
        let accepted_at = agent
            .identities
            .iter()
            .position(|id| id.public_key().key_data() == b.public_key().key_data())
            .expect("the agent holds the key");
        let out = offer_keys(&mut session, "u", &mut agent, Offer::Rest, |_| {}).await.unwrap();
        assert!(matches!(out, AgentOutcome::Authenticated));
        assert_eq!(offers.load(Ordering::SeqCst), accepted_at + 1, "keys turned down before it, none after");
    }

    #[tokio::test]
    async fn turned_down_keys_leave_the_session_usable() {
        let (mut session, _) = session_to(None, false).await;
        let mut agent = agent_holding(&[key(1), key(2)], false).await;
        let out = offer_keys(&mut session, "u", &mut agent, Offer::Rest, |_| {}).await.unwrap();
        assert!(matches!(out, AgentOutcome::NotAccepted));
        // Nothing is left waiting: the same session still logs in by password.
        assert!(session.authenticate_password("u", "pw").await.unwrap().success());
    }

    #[tokio::test]
    async fn a_refused_signature_ends_the_login() {
        let a = key(1);
        let (mut session, _) = session_to(Some(&a), false).await;
        let mut agent = agent_holding(&[a], true).await;
        let mut logged = Vec::new();
        let res = offer_keys(&mut session, "u", &mut agent, Offer::Rest, |m| logged.push(m)).await;
        assert!(matches!(res, Err(russh::Error::Keys(_))), "a refused signature is fatal");
        assert!(logged.iter().any(|m| m.starts_with("The test agent would not sign")), "{logged:?}");
    }

    #[tokio::test]
    async fn offers_stop_at_the_attempt_budget() {
        let keys: Vec<PrivateKey> = (1..=5).map(key).collect();
        let (mut session, offers) = session_to(None, false).await;
        let mut agent = agent_holding(&keys, false).await;
        let mut logged = Vec::new();
        let out = offer_keys(&mut session, "u", &mut agent, Offer::Rest, |m| logged.push(m)).await.unwrap();
        assert!(matches!(out, AgentOutcome::NotAccepted));
        assert_eq!(offers.load(Ordering::SeqCst), AGENT_MAX_ATTEMPTS);
        assert!(logged.iter().any(|m| m.contains("2 more keys")), "{logged:?}");
        // The budget is per connection: a later round offers nothing more.
        let again = offer_keys(&mut session, "u", &mut agent, Offer::Rest, |_| {}).await.unwrap();
        assert!(matches!(again, AgentOutcome::NothingToOffer));
        assert_eq!(offers.load(Ordering::SeqCst), AGENT_MAX_ATTEMPTS);
    }

    #[tokio::test]
    async fn only_offers_the_nodes_own_key_whatever_its_comment() {
        let (a, b) = (key(1), key(2));
        let mut labelled = b.clone();
        labelled.set_comment("laptop");
        let (mut session, offers) = session_to(Some(&b), false).await;
        let mut agent = agent_holding(&[a, labelled], false).await;
        let out = offer_keys(&mut session, "u", &mut agent, Offer::Only(b.public_key()), |_| {})
            .await
            .unwrap();
        assert!(matches!(out, AgentOutcome::Authenticated));
        assert_eq!(offers.load(Ordering::SeqCst), 1, "only the matching key is offered");
    }

    #[tokio::test]
    async fn only_offers_nothing_when_the_agent_lacks_the_key() {
        let (mut session, offers) = session_to(None, false).await;
        let mut agent = agent_holding(&[key(1)], false).await;
        let out = offer_keys(&mut session, "u", &mut agent, Offer::Only(key(2).public_key()), |_| {})
            .await
            .unwrap();
        assert!(matches!(out, AgentOutcome::NothingToOffer));
        assert_eq!(offers.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_key_already_offered_is_not_offered_again() {
        let (a, b) = (key(1), key(2));
        let (mut session, offers) = session_to(None, false).await;
        let mut agent = agent_holding(&[a.clone(), b], false).await;
        agent.mark_offered(a.public_key());
        let _ = offer_keys(&mut session, "u", &mut agent, Offer::Rest, |_| {}).await.unwrap();
        assert_eq!(offers.load(Ordering::SeqCst), 1, "the node's own key isn't offered twice");
    }

    #[tokio::test]
    async fn a_second_factor_can_follow_on_the_same_session() {
        let a = key(1);
        let (mut session, _) = session_to(Some(&a), true).await;
        let mut agent = agent_holding(&[a], false).await;
        let out = offer_keys(&mut session, "u", &mut agent, Offer::Rest, |_| {}).await.unwrap();
        // russh's own server (0.63) drops the partial-success flag for public
        // keys and only lists the methods left — here just a password — so the
        // answer reads as "public keys no longer offered". A real OpenSSH
        // server sets the flag; `interpret` handles both (see below).
        assert!(matches!(out, AgentOutcome::NotAccepted | AgentOutcome::PartialSuccess { .. }));
        // Either way nothing is left waiting: the password finishes the login.
        assert!(session.authenticate_password("u", "pw").await.unwrap().success());
    }

    fn failure(methods: &[MethodKind], partial_success: bool) -> AuthResult {
        AuthResult::Failure { remaining_methods: MethodSet::from(methods), partial_success }
    }

    #[test]
    fn a_partial_success_is_reported_with_the_methods_left() {
        match interpret(failure(&[MethodKind::Password], true)) {
            Some(AgentOutcome::PartialSuccess { remaining }) => {
                assert!(remaining.contains(&MethodKind::Password));
            }
            _ => panic!("expected a partial success"),
        }
    }

    #[test]
    fn the_next_key_is_offered_only_while_public_keys_are_still_taken() {
        assert!(interpret(failure(&[MethodKind::PublicKey, MethodKind::Password], false)).is_none());
        assert!(matches!(
            interpret(failure(&[MethodKind::Password], false)),
            Some(AgentOutcome::NotAccepted)
        ));
        assert!(matches!(interpret(AuthResult::Success), Some(AgentOutcome::Authenticated)));
    }

    #[test]
    fn an_encrypted_key_file_shows_its_public_key_without_the_passphrase() {
        let plain = key(9);
        let encrypted = plain.encrypt(&mut rand::rng(), "pw").expect("encrypt");
        let file = encrypted.to_openssh(russh::keys::ssh_key::LineEnding::LF).unwrap().to_string();
        let read = key_file_public_key(&file).expect("public key in the clear");
        assert_eq!(read.key_data(), plain.public_key().key_data());
        let crlf = file.replace('\n', "\r\n");
        assert_eq!(key_file_public_key(&crlf).unwrap().key_data(), plain.public_key().key_data());
    }

    #[test]
    fn a_non_openssh_key_file_shows_nothing() {
        assert!(key_file_public_key("-----BEGIN RSA PRIVATE KEY-----\nAAAA\n-----END RSA PRIVATE KEY-----\n").is_none());
        assert!(key_file_public_key("").is_none());
    }

    #[cfg(windows)]
    #[test]
    fn only_named_pipes_count_as_an_agent_socket_on_windows() {
        assert!(is_pipe_path(r"\\.\pipe\openssh-ssh-agent"));
        assert!(is_pipe_path("//./pipe/gpg-agent"));
        assert!(!is_pipe_path(r"\\.\pipe\"));
        assert!(!is_pipe_path("/tmp/ssh-XXXX/agent.123"));
        assert!(!is_pipe_path(""));
    }
}
