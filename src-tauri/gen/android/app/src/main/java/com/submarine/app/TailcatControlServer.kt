package com.submarine.app

import com.submarine.tailcatbridge.Tailcatbridge
import java.io.BufferedReader
import java.io.InputStreamReader
import java.io.OutputStreamWriter
import java.net.InetAddress
import java.net.ServerSocket
import java.net.Socket
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
    try {
      val fields = BufferedReader(InputStreamReader(s.getInputStream(), Charsets.UTF_8)).readLine().trim().split(" ")
      if (fields.size != 3 || fields[0] != "OPEN") throw IllegalArgumentException("bad request")
      val address = String(Base64.getUrlDecoder().decode(fields[1]), Charsets.UTF_8)
      if (!address.startsWith("tc")) throw IllegalArgumentException("bad Tailcat address")
      val remotePort = fields[2].toInt().also { require(it in 1..65535) }
      val handle = clients.computeIfAbsent(address) { Tailcatbridge.start(it) }
      val localPort = Tailcatbridge.openForward(handle, remotePort)
      out.write("OK $localPort\n")
    } catch (_: Exception) {
      // Deliberately generic: a Tailcat address may carry a PSK.
      out.write("ERR Tailcat bridge request failed\n")
    }
    out.flush()
  }

  fun stop() {
    clients.values.forEach { runCatching { Tailcatbridge.stop(it) } }
    clients.clear()
  }
}
