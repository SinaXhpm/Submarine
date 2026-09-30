//! In-process SSH server + client over in-memory pipes, for tests that need a
//! REAL russh connection: channel backpressure (the terminal pump, the tunnel
//! bridges, a ProxyJump transport), algorithm negotiation, DH group exchange.
//! Test-only — the module is declared `#[cfg(test)]` in lib.rs.

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use russh::keys::PublicKeyOrCertificate;
use russh::server::{self, Auth, Msg, Session};
use russh::{client, Channel, ChannelId};
use tokio::io::{AsyncRead, AsyncWrite};

/// A minimal SSH server. On a session channel the client's `exec` command
/// picks the behaviour:
///   * `flood <N>` — send N bytes of output in 16 KiB chunks, then EOF + close;
///   * `echo`      — echo every byte the client sends back to it.
///
/// Every byte the client sends on any session channel is counted in
/// `received`. A `direct-tcpip` open makes it a jump host: the channel is
/// bridged to a fresh in-process `TestServer` (the "target"), whose received
/// byte count is `jump_target_received`.
#[derive(Clone, Default)]
pub(crate) struct TestServer {
    pub received: Arc<AtomicUsize>,
    pub jump_target_received: Arc<AtomicUsize>,
    /// When set, the ONLY group offered for `diffie-hellman-group-exchange-*`.
    gex_group: Option<russh::kex::dh::groups::DhGroup>,
    echo: HashSet<ChannelId>,
}

impl TestServer {
    /// A server whose group exchange only ever offers `group` — models a
    /// server whose moduli file has a single size.
    pub(crate) fn with_gex_group(group: russh::kex::dh::groups::DhGroup) -> Self {
        Self { gex_group: Some(group), ..Self::default() }
    }
}

impl server::Handler for TestServer {
    type Error = russh::Error;

    async fn auth_password(&mut self, _user: &str, _password: &str) -> Result<Auth, Self::Error> {
        Ok(Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        // Client data is consumed through the `data` callback; the channel
        // object is dropped so its (never read) queue can't stall this server.
        drop(channel);
        reply.accept().await;
        Ok(())
    }

    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<Msg>,
        _host_to_connect: &str,
        _port_to_connect: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        let (jump_end, target_end) = tokio::io::duplex(256 * 1024);
        let target = TestServer {
            received: Arc::clone(&self.jump_target_received),
            ..TestServer::default()
        };
        serve(target, |_| {}, target_end);
        tokio::spawn(async move {
            let mut hop = channel.into_stream();
            let mut jump_end = jump_end;
            let _ = tokio::io::copy_bidirectional(&mut hop, &mut jump_end).await;
        });
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let command = String::from_utf8_lossy(data).into_owned();
        if command == "echo" {
            self.echo.insert(channel);
        } else if let Some(total) = command
            .strip_prefix("flood ")
            .and_then(|n| n.trim().parse::<usize>().ok())
        {
            let handle = session.handle();
            tokio::spawn(async move {
                const CHUNK: usize = 16 * 1024;
                let mut sent = 0;
                while sent < total {
                    let n = CHUNK.min(total - sent);
                    if handle.data(channel, vec![b'x'; n]).await.is_err() {
                        return;
                    }
                    sent += n;
                }
                let _ = handle.eof(channel).await;
                let _ = handle.close(channel).await;
            });
        }
        Ok(())
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.received.fetch_add(data.len(), Ordering::SeqCst);
        if self.echo.contains(&channel) {
            session.data(channel, data.to_vec())?;
        }
        Ok(())
    }

    async fn lookup_dh_gex_group(
        &mut self,
        _gex_params: &client::GexParams,
    ) -> Result<Option<russh::kex::dh::groups::DhGroup>, Self::Error> {
        Ok(Some(
            self.gex_group
                .clone()
                .unwrap_or(russh::kex::dh::groups::DH_GROUP14),
        ))
    }
}

/// Test client: trusts any host key and records what the last key exchange
/// negotiated.
#[derive(Clone, Default)]
pub(crate) struct TestClient {
    pub negotiated: Arc<std::sync::Mutex<Option<russh::Names>>>,
}

impl client::Handler for TestClient {
    type Error = russh::Error;

    async fn check_server_key(&mut self, _key: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        Ok(true)
    }

    async fn kex_done(
        &mut self,
        _shared_secret: Option<&[u8]>,
        names: &russh::Names,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        *self.negotiated.lock().unwrap() = Some(names.clone());
        Ok(())
    }
}

fn host_key() -> russh::keys::PrivateKey {
    use russh::keys::ssh_key::private::{Ed25519Keypair, Ed25519PrivateKey};
    russh::keys::PrivateKey::from(Ed25519Keypair::from(Ed25519PrivateKey::from_bytes(&[42u8; 32])))
}

/// Serve `server` on `io` in the background. `tweak` adjusts the server
/// config (window size, algorithm lists).
pub(crate) fn serve<S>(server: TestServer, tweak: impl FnOnce(&mut server::Config), io: S)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut server_config = server::Config {
        keys: vec![host_key()],
        inactivity_timeout: None,
        ..Default::default()
    };
    tweak(&mut server_config);
    tokio::spawn(async move {
        if let Ok(running) = server::run_stream(Arc::new(server_config), io, server).await {
            let _ = running.await;
        }
    });
}

/// Handshake + password-authenticate a `client` over `transport`.
pub(crate) async fn client_over<S>(
    transport: S,
    client_config: client::Config,
    client: TestClient,
) -> Result<client::Handle<TestClient>, russh::Error>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut session = client::connect_stream(Arc::new(client_config), transport, client).await?;
    if !session.authenticate_password("test", "test").await?.success() {
        return Err(russh::Error::NotAuthenticated);
    }
    Ok(session)
}

/// Run `server` on one end of an in-memory pipe and connect a client with
/// `client_config` to the other; returns the authenticated client handle.
pub(crate) async fn connect(
    server: TestServer,
    tweak: impl FnOnce(&mut server::Config),
    client_config: client::Config,
) -> Result<client::Handle<TestClient>, russh::Error> {
    connect_with_client(server, tweak, client_config, TestClient::default()).await
}

pub(crate) async fn connect_with_client(
    server: TestServer,
    tweak: impl FnOnce(&mut server::Config),
    client_config: client::Config,
    client: TestClient,
) -> Result<client::Handle<TestClient>, russh::Error> {
    let (client_io, server_io) = tokio::io::duplex(256 * 1024);
    serve(server, tweak, server_io);
    client_over(client_io, client_config, client).await
}
