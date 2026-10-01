package com.submarine.app

import com.submarine.tailcatbridge.tailcatbridge.Tailcatbridge
import java.io.BufferedReader
import java.io.InputStreamReader
import java.io.OutputStreamWriter
import java.net.InetAddress
import java.net.ServerSocket
import java.net.Socket
import java.security.MessageDigest
import java.util.Base64
import java.util.concurrent.ConcurrentHashMap
import kotlin.concurrent.thread

/**
 * Private same-process control plane for the Go Mobile Tailcat bridge. Rust
 * sends an address only over 127.0.0.1, receives an ephemeral loopback port,
 * and continues using Tokio/russh normally. This is not a VPN service and
 * does not create a TUN interface or request VPN permission.
 */
object TailcatControlServer {
  private const val PORT = 38491
  private val clients = ConcurrentHashMap<String, Long>()
  @Volatile private var started = false

  private fun addressIdentity(address: String): String = MessageDigest
    .getInstance("SHA-256")
    .digest(address.toByteArray(Charsets.UTF_8))
    .take(12)
    .joinToString("") { "%02x".format(it.toInt() and 0xff) }

  fun start() {
    if (started) return
    synchronized(this) {
      if (started) return
      val server = ServerSocket(PORT, 16, InetAddress.getByName("127.0.0.1"))
      started = true
      thread(name = "tailcat-control", isDaemon = true) {
        while (!server.isClosed) try { handle(server.accept()) } catch (_: Exception) { }
      }
    }
  }

  private fun handle(socket: Socket) = socket.use { s ->
    val out = OutputStreamWriter(s.getOutputStream(), Charsets.UTF_8)
    val fields = runCatching {
      BufferedReader(InputStreamReader(s.getInputStream(), Charsets.UTF_8)).readLine().trim().split(" ")
    }.getOrNull()
    if (fields == null || fields.size != 3 || fields[0] != "OPEN") {
      out.write("ERR BAD_REQUEST\n")
      out.flush()
      return@use
    }
    val address = runCatching {
      String(Base64.getUrlDecoder().decode(fields[1]), Charsets.UTF_8).trim()
    }.getOrNull()
    if (address.isNullOrEmpty() || !address.startsWith("tc")) {
      out.write("ERR INVALID_ADDRESS\n")
      out.flush()
      return@use
    }
    val remotePort = fields[2].toIntOrNull()?.takeIf { it in 1..65535 }
    if (remotePort == null) {
      out.write("ERR INVALID_PORT\n")
      out.flush()
      return@use
    }
    // Do not return exception messages: a Tailcat address can contain a PSK.
    val handle = runCatching { clients.computeIfAbsent(address) { Tailcatbridge.start(it) } }
      .getOrElse { error ->
        // Start only returns these fixed, non-secret validation messages. Do
        // not pass arbitrary JNI/Go exception text back to Rust.
        val code = if (error.message == "invalid Tailcat address" || error.message == "Tailcat address must start with tc") {
          "INVALID_ADDRESS"
        } else {
          "CLIENT_START_FAILED"
        }
        // Identity is a truncated SHA-256 digest, allowing support to compare
        // a persisted profile with the server address without disclosing it.
        out.write("ERR $code ${addressIdentity(address)}\n")
        out.flush()
        return@use
      }
    val localPort = runCatching { Tailcatbridge.openForward(handle, remotePort.toLong()) }
      .getOrElse {
        out.write("ERR FORWARD_OPEN_FAILED\n")
        out.flush()
        return@use
      }
    out.write("OK $localPort\n")
    out.flush()
  }

  fun stop() {
    clients.values.forEach { runCatching { Tailcatbridge.stop(it) } }
    clients.clear()
  }
}
