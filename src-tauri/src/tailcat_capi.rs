//! In-process Tailcat transport backed by upstream `libtailcat` ABI v1.
//!
//! This module contains no network listener or control protocol. The Go
//! runtime stays inside the process and Rust exchanges bytes through the C
//! ABI's opaque connection handles. It is feature-gated until Tailcat ships a
//! release containing `cmd/libtailcat`; see `SUBMARINE_LIBTAILCAT_DIR` in
//! `build.rs` for the deliberately explicit native-library build contract.

use std::{
    ffi::{c_char, CStr, CString},
    io,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock, Weak,
    },
    task::{Context, Poll},
    time::Duration,
};

use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf};

mod ffi {
    use std::ffi::{c_char, c_void};

    pub type Handle = u64;
    pub const NO_CANCEL: Handle = 0;
    pub const OK: i32 = 0;
    pub const EOF: i32 = 6;
    pub const TCP: i32 = 1;
    pub const ABI_VERSION: u32 = 1;

    unsafe extern "C" {
        pub fn tc_abi_version() -> u32;
        pub fn tc_client_new(config: *mut c_char, out: *mut Handle, error: *mut *mut c_char)
            -> i32;
        pub fn tc_client_dial(
            client: Handle,
            token: Handle,
            port: u16,
            network: i32,
            out: *mut Handle,
            error: *mut *mut c_char,
        ) -> i32;
        pub fn tc_token_new(timeout_ns: i64, out: *mut Handle, error: *mut *mut c_char) -> i32;
        pub fn tc_conn_read(
            connection: Handle,
            token: Handle,
            buffer: *mut c_void,
            capacity: usize,
            count: *mut usize,
            error: *mut *mut c_char,
        ) -> i32;
        pub fn tc_conn_write(
            connection: Handle,
            token: Handle,
            buffer: *mut c_void,
            length: usize,
            count: *mut usize,
            error: *mut *mut c_char,
        ) -> i32;
        pub fn tc_conn_close_write(
            connection: Handle,
            token: Handle,
            error: *mut *mut c_char,
        ) -> i32;
        pub fn tc_close(resource: Handle, error: *mut *mut c_char) -> i32;
        pub fn tc_free(allocation: *mut c_void);
    }
}

const DIAL_TIMEOUT: Duration = Duration::from_secs(15);
const BUFFER_SIZE: usize = 32 * 1024;

/// A non-secret, stable identity suitable for SSH known-host lookup.
pub fn verification_host(address: &str) -> String {
    let digest = Sha256::digest(address.trim().as_bytes());
    format!("tailcat:{}", hex::encode(&digest[..12]))
}

/// Redact a Tailcat capability before it can reach application logs.
#[cfg(test)]
pub fn redact(address: &str) -> &'static str {
    if address.trim_start().starts_with("tc") {
        "tc…[redacted]"
    } else {
        "[redacted]"
    }
}

/// Returns the native library ABI version without performing network I/O.
pub fn native_abi_version() -> u32 {
    // SAFETY: ABI version is a pure, process-wide query.
    unsafe { ffi::tc_abi_version() }
}

fn redact_native_detail(detail: String, address: &str) -> String {
    if address.is_empty() {
        detail
    } else {
        detail.replace(address, "[redacted]")
    }
}

fn native_error(status: i32, error: *mut c_char, address: Option<&str>) -> String {
    // libtailcat documents that configuration errors never echo secrets. Keep
    // that boundary nevertheless: only error text allocated by libtailcat is
    // read and immediately returned/freed; the caller's address is never
    // included in this module's error strings.
    let detail = if error.is_null() {
        None
    } else {
        // SAFETY: successful ABI calls return either NULL or a NUL-terminated
        // allocation owned by libtailcat, released exactly once below.
        let value = unsafe { CStr::from_ptr(error) }
            .to_string_lossy()
            .into_owned();
        // SAFETY: `error` came from libtailcat and is freed using its allocator.
        unsafe { ffi::tc_free(error.cast()) };
        Some(value)
    };
    match detail {
        // Native errors are dependency-controlled text. Do not let a future
        // libtailcat version turn a capability into an application log entry.
        Some(detail) if !detail.is_empty() => format!(
            "Tailcat native transport failed (status {status}): {}",
            redact_native_detail(detail, address.unwrap_or_default())
        ),
        _ => format!("Tailcat native transport failed (status {status})"),
    }
}

fn status(result: i32, error: *mut c_char, address: Option<&str>) -> Result<(), String> {
    if result == ffi::OK {
        if !error.is_null() {
            // Defensive cleanup for an ABI violation; a success error is not
            // meaningful to callers and must not leak its allocation.
            unsafe { ffi::tc_free(error.cast()) };
        }
        Ok(())
    } else {
        Err(native_error(result, error, address))
    }
}

fn close_handle(handle: ffi::Handle) {
    let mut error = std::ptr::null_mut();
    // SAFETY: opaque handle was returned by libtailcat. Closing is idempotent
    // at this wrapper boundary; libtailcat owns all associated resources.
    let result = unsafe { ffi::tc_close(handle, &mut error) };
    if !error.is_null() {
        // SAFETY: see `native_error` ownership contract.
        unsafe { ffi::tc_free(error.cast()) };
    }
    let _ = result;
}

fn timeout_token(timeout: Duration) -> Result<ffi::Handle, String> {
    let nanos = timeout.as_nanos().min(i64::MAX as u128) as i64;
    let mut token = 0;
    let mut error = std::ptr::null_mut();
    // SAFETY: output pointers are valid for this synchronous C call.
    status(
        unsafe { ffi::tc_token_new(nanos, &mut token, &mut error) },
        error,
        None,
    )?;
    Ok(token)
}

/// A reusable Tailcat client. Clone it for terminal/SFTP/monitor connections
/// to share one WireGuard/magicsock client inside the process.
#[derive(Clone)]
pub struct Client {
    inner: Arc<ClientInner>,
}

struct ClientInner {
    handle: ffi::Handle,
    address: String,
    closed: AtomicBool,
}

impl Drop for ClientInner {
    fn drop(&mut self) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            close_handle(self.handle);
        }
    }
}

impl Client {
    /// Constructs a lazy Tailcat client. No network traffic occurs until
    /// [`Self::open_tcp`] is called.
    pub fn new(address: &str) -> Result<Self, String> {
        if !address.trim_start().starts_with("tc") {
            return Err("Tailcat address must start with tc".into());
        }
        let abi = native_abi_version();
        if abi != ffi::ABI_VERSION {
            return Err(format!("Tailcat native ABI version {abi} is unsupported"));
        }
        // `json!` ensures the address is correctly escaped before it crosses
        // the C ABI; do not concatenate a secret into JSON manually.
        let address = address.trim();
        let config = serde_json::json!({ "address": address }).to_string();
        let config = CString::new(config)
            .map_err(|_| "Tailcat address contains an unsupported NUL byte".to_string())?;
        let mut handle = 0;
        let mut error = std::ptr::null_mut();
        // SAFETY: libtailcat copies its configuration before returning and all
        // output pointers are valid for the synchronous call.
        status(
            unsafe { ffi::tc_client_new(config.as_ptr().cast_mut(), &mut handle, &mut error) },
            error,
            Some(address),
        )?;
        if handle == 0 {
            return Err("Tailcat native transport returned an invalid client handle".into());
        }
        Ok(Self {
            inner: Arc::new(ClientInner {
                handle,
                address: address.to_string(),
                closed: AtomicBool::new(false),
            }),
        })
    }

    /// Opens a TCP stream to a port served by the Tailcat peer.
    pub async fn open_tcp(&self, port: u16) -> Result<TailcatStream, String> {
        if port == 0 {
            return Err("Tailcat TCP port must be between 1 and 65535".into());
        }
        let client = self.inner.clone();
        let connection = tokio::task::spawn_blocking(move || {
            if client.closed.load(Ordering::Acquire) {
                return Err("Tailcat client is closed".to_string());
            }
            let token = timeout_token(DIAL_TIMEOUT)?;
            let mut handle = 0;
            let mut error = std::ptr::null_mut();
            // SAFETY: the client and token are live opaque handles; libtailcat
            // documents concurrent dials on one client as supported.
            let result = unsafe {
                ffi::tc_client_dial(
                    client.handle,
                    token,
                    port,
                    ffi::TCP,
                    &mut handle,
                    &mut error,
                )
            };
            close_handle(token);
            status(result, error, Some(&client.address))?;
            if handle == 0 {
                return Err(
                    "Tailcat native transport returned an invalid connection handle".into(),
                );
            }
            Ok(Arc::new(ConnectionInner {
                // A libtailcat client owns every dialled connection. Retain it
                // for the stream lifetime even if the caller drops its Client
                // clone immediately after opening a transport.
                client,
                handle,
                closed: AtomicBool::new(false),
            }))
        })
        .await
        .map_err(|_| "Tailcat TCP dial task stopped unexpectedly".to_string())??;
        TailcatStream::from_connection(connection).await
    }
}

/// Opens a stream through a process-wide client cache. Keys are SHA-256
/// digests, never Tailcat addresses, so diagnostics and map keys cannot expose
/// the address capability. A live SSH/SFTP/monitor stream retains its client;
/// when the last stream closes the weak cache entry naturally expires.
pub async fn open_tcp(address: &str, port: u16) -> Result<TailcatStream, String> {
    type ClientCache = std::collections::HashMap<[u8; 32], Weak<ClientInner>>;
    static CLIENTS: OnceLock<Mutex<ClientCache>> = OnceLock::new();

    let digest: [u8; 32] = Sha256::digest(address.trim().as_bytes()).into();
    let clients = CLIENTS.get_or_init(|| Mutex::new(ClientCache::new()));
    let client = {
        let mut clients = clients
            .lock()
            .map_err(|_| "Tailcat client cache lock failed".to_string())?;
        clients.retain(|_, client| client.strong_count() != 0);
        match clients.get(&digest).and_then(Weak::upgrade) {
            Some(inner) => Client { inner },
            None => {
                let client = Client::new(address)?;
                clients.insert(digest, Arc::downgrade(&client.inner));
                client
            }
        }
    };
    client.open_tcp(port).await
}

struct ConnectionInner {
    client: Arc<ClientInner>,
    handle: ffi::Handle,
    closed: AtomicBool,
}

impl ConnectionInner {
    fn close(&self) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            close_handle(self.handle);
        }
    }

    fn request_close(self: &Arc<Self>) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            let handle = self.handle;
            std::thread::spawn(move || close_handle(handle));
        }
    }

    fn read(&self) -> Result<(Vec<u8>, bool), String> {
        let mut buffer = vec![0_u8; BUFFER_SIZE];
        let mut count = 0;
        let mut error = std::ptr::null_mut();
        // SAFETY: buffer/count/error remain valid for this synchronous call;
        // a connection allows one concurrent read and one concurrent write.
        let result = unsafe {
            ffi::tc_conn_read(
                self.handle,
                ffi::NO_CANCEL,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut count,
                &mut error,
            )
        };
        buffer.truncate(count);
        if result == ffi::EOF {
            if !error.is_null() {
                unsafe { ffi::tc_free(error.cast()) };
            }
            return Ok((buffer, true));
        }
        status(result, error, Some(&self.client.address))?;
        Ok((buffer, false))
    }

    fn write_all(&self, bytes: &[u8]) -> Result<(), String> {
        let mut offset = 0;
        while offset < bytes.len() {
            let mut count = 0;
            let mut error = std::ptr::null_mut();
            // SAFETY: libtailcat borrows the buffer only until this call returns.
            let result = unsafe {
                ffi::tc_conn_write(
                    self.handle,
                    ffi::NO_CANCEL,
                    bytes[offset..].as_ptr().cast_mut().cast(),
                    bytes.len() - offset,
                    &mut count,
                    &mut error,
                )
            };
            status(result, error, Some(&self.client.address))?;
            if count == 0 {
                return Err("Tailcat native transport made no write progress".into());
            }
            offset += count;
        }
        Ok(())
    }

    fn close_write(&self) {
        let mut error = std::ptr::null_mut();
        // SAFETY: close-write is synchronous and uses this live opaque handle.
        let result = unsafe { ffi::tc_conn_close_write(self.handle, ffi::NO_CANCEL, &mut error) };
        if !error.is_null() {
            unsafe { ffi::tc_free(error.cast()) };
        }
        let _ = result;
    }
}

impl Drop for ConnectionInner {
    fn drop(&mut self) {
        self.close();
    }
}

/// Tokio byte stream backed by an in-process `libtailcat` connection.
///
/// `libtailcat` intentionally has blocking I/O rather than exposing an OS
/// file descriptor. Two background tasks bridge it to a bounded Tokio duplex
/// stream: one reader and one writer, the exact concurrency its ABI supports.
pub struct TailcatStream {
    stream: DuplexStream,
    connection: Arc<ConnectionInner>,
}

impl TailcatStream {
    async fn from_connection(connection: Arc<ConnectionInner>) -> Result<Self, String> {
        let (stream, bridge) = tokio::io::duplex(BUFFER_SIZE * 2);
        let (mut app_read, mut app_write) = tokio::io::split(bridge);

        let read_connection = connection.clone();
        tokio::spawn(async move {
            loop {
                let current = read_connection.clone();
                let result = tokio::task::spawn_blocking(move || current.read()).await;
                let Ok(Ok((bytes, eof))) = result else { break };
                if !bytes.is_empty() && app_write.write_all(&bytes).await.is_err() {
                    break;
                }
                if eof {
                    break;
                }
            }
            let _ = app_write.shutdown().await;
            read_connection.request_close();
        });

        let write_connection = connection.clone();
        tokio::spawn(async move {
            let mut buffer = vec![0_u8; BUFFER_SIZE];
            loop {
                let read = app_read.read(&mut buffer).await;
                let Ok(count) = read else { break };
                if count == 0 {
                    write_connection.close_write();
                    break;
                }
                let bytes = buffer[..count].to_vec();
                let current = write_connection.clone();
                let result = tokio::task::spawn_blocking(move || current.write_all(&bytes)).await;
                if !matches!(result, Ok(Ok(()))) {
                    break;
                }
            }
            write_connection.request_close();
        });

        Ok(Self { stream, connection })
    }
}

impl Drop for TailcatStream {
    fn drop(&mut self) {
        // `tc_close` waits for an outstanding blocking read/write. Never make
        // an SSH task's destructor wait for that work; the ABI promises close
        // itself interrupts those calls.
        self.connection.request_close();
    }
}

impl AsyncRead for TailcatStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for TailcatStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_redaction_never_returns_token() {
        let token = "tcABCDEF-secret";
        assert!(!redact(token).contains("ABCDEF"));
    }

    #[test]
    fn verification_name_is_stable_and_redacted() {
        let token = "tcABCDEF-secret";
        assert_eq!(verification_host(token), verification_host(token));
        assert!(!verification_host(token).contains(token));
    }

    #[test]
    fn native_error_detail_cannot_expose_a_capability() {
        assert_eq!(
            redact_native_detail("dial tcABCDEF-secret failed".into(), "tcABCDEF-secret"),
            "dial [redacted] failed"
        );
        assert_eq!(
            redact_native_detail(
                "tcp dial failed while fetch returned an error".into(),
                "tcABCDEF-secret"
            ),
            "tcp dial failed while fetch returned an error"
        );
    }

    #[test]
    fn linked_library_has_the_expected_abi() {
        assert_eq!(native_abi_version(), ffi::ABI_VERSION);
    }
}
