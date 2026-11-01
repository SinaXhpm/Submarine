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
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) struct TestServer {
    pub received: Arc<AtomicUsize>,
    pub jump_target_received: Arc<AtomicUsize>,
    /// Public keys offered for `publickey` auth (probes included).
    pub pk_offers: Arc<AtomicUsize>,
    /// When set, the ONLY group offered for `diffie-hellman-group-exchange-*`.
    gex_group: Option<russh::kex::dh::groups::DhGroup>,
    echo: HashSet<ChannelId>,
    /// The one public key `publickey` auth accepts; none when unset.
    accept_key: Option<russh::keys::PublicKey>,
    /// After that key, ask for a password too (`AuthenticationMethods
    /// publickey,password`).
    second_factor: bool,
    /// How an `sftp` subsystem request is answered.
    sftp: SftpAnswer,
    /// Run each `exec` command with this machine's `sh` (unix): a server
    /// that runs commands for real.
    shell: Option<ShellOpts>,
    /// stdin of the commands still running, by channel (`None` = EOF).
    stdin: std::collections::HashMap<ChannelId, tokio::sync::mpsc::UnboundedSender<Option<Vec<u8>>>>,
    /// Ends a running command when its channel is closed.
    kill: std::collections::HashMap<ChannelId, Arc<tokio::sync::Notify>>,
    /// Commands started, then still running (shared with their tasks).
    pub running: Arc<AtomicUsize>,
    /// Commands started in all.
    pub started: Arc<AtomicUsize>,
}

/// How the test server runs `exec` commands with the local `sh` (unix only).
#[derive(Clone, Default)]
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) struct ShellOpts {
    /// PATH for the commands (the test process's own when `None`).
    pub path: Option<String>,
    /// Their working folder (the "home" folder).
    pub cwd: Option<std::path::PathBuf>,
    /// The account's login shell: what runs each command, as
    /// `<login...> <command>` (`sh -c` when empty).
    pub login: Vec<String>,
    /// Like Dropbear: a command keeps running when its channel is closed;
    /// only a `signal` request ends it.
    pub keep_running_on_close: bool,
}

/// The test server's answer to an `sftp` subsystem request.
#[derive(Clone, Copy, Default)]
pub(crate) enum SftpAnswer {
    /// No reply at all (russh's default handler).
    #[default]
    Silent,
    /// Turned down: no SFTP set up (an sshd without `Subsystem sftp`, a
    /// Dropbear built without it).
    Refuse,
    /// Accepted, then the channel ends at once: the program behind it is
    /// missing (Dropbear on a router without an SFTP server).
    StartThenEnd,
}

impl TestServer {
    /// A server whose group exchange only ever offers `group` — models a
    /// server whose moduli file has a single size.
    pub(crate) fn with_gex_group(group: russh::kex::dh::groups::DhGroup) -> Self {
        Self { gex_group: Some(group), ..Self::default() }
    }

    /// A server whose `publickey` auth accepts only `key` (none when `None`);
    /// with `second_factor` that key then counts as a partial success and a
    /// password is asked for too. Passwords are always accepted.
    pub(crate) fn accepting_key(key: Option<russh::keys::PublicKey>, second_factor: bool) -> Self {
        Self { accept_key: key, second_factor, ..Self::default() }
    }

    /// A server that answers an `sftp` subsystem request with `answer`.
    pub(crate) fn with_sftp(answer: SftpAnswer) -> Self {
        Self { sftp: answer, ..Self::default() }
    }

    /// A server without SFTP that runs commands with this machine's `sh`.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn with_shell(opts: ShellOpts) -> Self {
        Self { sftp: SftpAnswer::Refuse, shell: Some(opts), ..Self::default() }
    }

    /// Run `command` with `sh -c` for the channel: stdin from the client's
    /// data, stdout and stderr back, then its exit status, EOF and close.
    #[cfg(unix)]
    fn spawn_shell(&mut self, channel: ChannelId, command: String, handle: server::Handle) {
        use std::process::Stdio;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Option<Vec<u8>>>();
        let kill = Arc::new(tokio::sync::Notify::new());
        self.stdin.insert(channel, tx);
        self.kill.insert(channel, Arc::clone(&kill));
        let login = match &self.shell {
            Some(opts) if !opts.login.is_empty() => opts.login.clone(),
            _ => vec!["sh".to_string(), "-c".to_string()],
        };
        let mut cmd = tokio::process::Command::new(&login[0]);
        cmd.args(&login[1..]).arg(&command).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        if let Some(opts) = &self.shell {
            if let Some(path) = &opts.path {
                cmd.env("PATH", path);
            }
            if let Some(cwd) = &opts.cwd {
                cmd.current_dir(cwd);
            }
        }
        let running = Arc::clone(&self.running);
        let started = Arc::clone(&self.started);
        tokio::spawn(async move {
            let Ok(mut child) = cmd.kill_on_drop(true).spawn() else {
                let _ = handle.exit_status_request(channel, 127).await;
                let _ = handle.close(channel).await;
                return;
            };
            running.fetch_add(1, Ordering::SeqCst);
            started.fetch_add(1, Ordering::SeqCst);
            let mut stdin = child.stdin.take().unwrap();
            let mut stdout = child.stdout.take().unwrap();
            let mut stderr = child.stderr.take().unwrap();
            let feed = tokio::spawn(async move {
                while let Some(Some(data)) = rx.recv().await {
                    if stdin.write_all(&data).await.is_err() {
                        break;
                    }
                }
            });
            let h = handle.clone();
            let out = tokio::spawn(async move {
                let mut buf = vec![0u8; 32 * 1024];
                while let Ok(n) = stdout.read(&mut buf).await {
                    if n == 0 || h.data(channel, buf[..n].to_vec()).await.is_err() {
                        break;
                    }
                }
            });
            let h = handle.clone();
            let err = tokio::spawn(async move {
                let mut buf = vec![0u8; 8 * 1024];
                while let Ok(n) = stderr.read(&mut buf).await {
                    if n == 0 || h.extended_data(channel, 1, buf[..n].to_vec()).await.is_err() {
                        break;
                    }
                }
            });
            let code = tokio::select! {
                s = child.wait() => s.ok().and_then(|s| s.code()).unwrap_or(255) as u32,
                _ = kill.notified() => {
                    let _ = child.kill().await;
                    255
                }
            };
            let _ = out.await;
            let _ = err.await;
            feed.abort();
            running.fetch_sub(1, Ordering::SeqCst);
            let _ = handle.exit_status_request(channel, code).await;
            let _ = handle.eof(channel).await;
            let _ = handle.close(channel).await;
        });
    }

    fn accepts(&self, key: &russh::keys::PublicKey) -> bool {
        self.accept_key.as_ref().is_some_and(|k| k.key_data() == key.key_data())
    }
}

impl server::Handler for TestServer {
    type Error = russh::Error;

    async fn auth_password(&mut self, _user: &str, _password: &str) -> Result<Auth, Self::Error> {
        Ok(Auth::Accept)
    }

    async fn auth_publickey_offered(
        &mut self,
        _user: &str,
        offer: russh::server::PublicKeyOffer<'_>,
    ) -> Result<Auth, Self::Error> {
        self.pk_offers.fetch_add(1, Ordering::SeqCst);
        Ok(if self.accepts(offer.public_key) { Auth::Accept } else { Auth::reject() })
    }

    async fn auth_publickey(&mut self, _user: &str, key: russh::server::VerifiedPublicKey<'_>) -> Result<Auth, Self::Error> {
        if !self.accepts(key.public_key) {
            return Ok(Auth::reject());
        }
        if self.second_factor {
            return Ok(Auth::Reject {
                proceed_with_methods: Some(russh::MethodSet::from(&[russh::MethodKind::Password][..])),
                partial_success: true,
            });
        }
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

    async fn subsystem_request(
        &mut self,
        channel: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if name != "sftp" {
            return Ok(());
        }
        match self.sftp {
            SftpAnswer::Silent => {}
            SftpAnswer::Refuse => session.channel_failure(channel)?,
            SftpAnswer::StartThenEnd => {
                session.channel_success(channel)?;
                let handle = session.handle();
                tokio::spawn(async move {
                    let _ = handle.eof(channel).await;
                    let _ = handle.close(channel).await;
                });
            }
        }
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let command = String::from_utf8_lossy(data).into_owned();
        #[cfg(unix)]
        if self.shell.is_some() {
            self.spawn_shell(channel, command, session.handle());
            return Ok(());
        }
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
        if let Some(stdin) = self.stdin.get(&channel) {
            let _ = stdin.send(Some(data.to_vec()));
        }
        Ok(())
    }

    async fn channel_eof(&mut self, channel: ChannelId, _session: &mut Session) -> Result<(), Self::Error> {
        if let Some(stdin) = self.stdin.remove(&channel) {
            let _ = stdin.send(None);
        }
        Ok(())
    }

    async fn channel_close(&mut self, channel: ChannelId, _session: &mut Session) -> Result<(), Self::Error> {
        self.stdin.remove(&channel);
        if self.shell.as_ref().is_some_and(|s| s.keep_running_on_close) {
            return Ok(());
        }
        if let Some(kill) = self.kill.remove(&channel) {
            kill.notify_one();
        }
        Ok(())
    }

    async fn signal(&mut self, channel: ChannelId, _signal: russh::Sig, _session: &mut Session) -> Result<(), Self::Error> {
        if let Some(kill) = self.kill.remove(&channel) {
            kill.notify_one();
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

/// Each way of the in-memory pipe between a test client and server. Both are
/// russh, whose session loop reads nothing while it writes what is pending:
/// with a big upload and a big download at once, both ends could wait on a
/// full pipe for good (a real server, OpenSSH or Dropbear, keeps reading).
/// Room for all the SSH windows let be on the way (8 MiB a channel from the
/// server to this app's client, 2 MiB back) keeps them from waiting on each
/// other; only what is waiting in it takes memory.
const PIPE: usize = 64 * 1024 * 1024;

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
    let (client_io, server_io) = tokio::io::duplex(PIPE);
    serve(server, tweak, server_io);
    client_over(client_io, client_config, client).await
}

/// Run `server` and complete only the handshake: the returned client isn't
/// authenticated yet, for tests that drive authentication themselves. Turned
/// down attempts are answered at once (russh otherwise pads each to 1 s).
pub(crate) async fn handshake(
    server: TestServer,
    client_config: client::Config,
) -> Result<client::Handle<TestClient>, russh::Error> {
    let (client_io, server_io) = tokio::io::duplex(PIPE);
    serve(
        server,
        |c| {
            c.auth_rejection_time = std::time::Duration::ZERO;
            c.auth_rejection_time_initial = Some(std::time::Duration::ZERO);
        },
        server_io,
    );
    client::connect_stream(Arc::new(client_config), client_io, TestClient::default()).await
}
