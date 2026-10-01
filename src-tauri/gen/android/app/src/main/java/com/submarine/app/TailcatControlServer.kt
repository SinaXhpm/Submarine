package com.submarine.app

import com.submarine.tailcatbridge.tailcatbridge.Tailcatbridge
import android.content.Context
import android.net.ConnectivityManager
import android.net.NetworkCapabilities
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
import org.json.JSONArray
import org.json.JSONObject

/**
 * Private same-process control plane for the Go Mobile Tailcat bridge. Rust
 * sends an address only over 127.0.0.1, receives an ephemeral loopback port,
 * and continues using Tokio/russh normally. This is not a VPN service and
 * does not create a TUN interface or request VPN permission.
 */
object TailcatControlServer {
  private const val PORT = 38491
  private val clients = ConcurrentHashMap<String, Long>()
  private lateinit var appContext: Context
  @Volatile private var started = false

  private fun addressIdentity(address: String): String = MessageDigest
    .getInstance("SHA-256")
    .digest(address.toByteArray(Charsets.UTF_8))
    .take(12)
    .joinToString("") { "%02x".format(it.toInt() and 0xff) }

  // Go Mobile exceptions can include transport internals. Only return fixed
  // classifications to Rust: a Tailcat address can contain a PSK.
  private fun forwardErrorCode(error: Throwable): String = when {
    error.message?.contains("fetching DERPMap") == true -> "DERP_MAP_UNAVAILABLE"
    error.message?.contains("no DERP regions") == true -> "NO_DERP_REGION"
    error.message?.contains("meow not sent") == true -> "RELAY_UNAVAILABLE"
    error.message?.contains("context deadline exceeded") == true -> "RELAY_HANDSHAKE_TIMEOUT"
    else -> "FORWARD_OPEN_FAILED"
  }

  // Android does not grant ordinary apps netlink route access. Supply the
  // LinkProperties it does expose to the Go Tailcat engine instead. This is
  // not VpnService and does not alter the device routing table.
  private fun updateNativeNetworkState() {
    val connectivity = appContext.getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager
    val interfaces = JSONArray()
    var defaultInterface = ""
    for (network in connectivity.allNetworks) {
      val capabilities = connectivity.getNetworkCapabilities(network) ?: continue
      if (!capabilities.hasCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET) ||
        capabilities.hasTransport(NetworkCapabilities.TRANSPORT_VPN)) continue
      val properties = connectivity.getLinkProperties(network) ?: continue
      val name = properties.interfaceName ?: continue
      val addresses = JSONArray()
      properties.linkAddresses.forEach { address ->
        addresses.put("${address.address.hostAddress}/${address.prefixLength}")
      }
      if (addresses.length() == 0) continue
      interfaces.put(JSONObject().apply {
        put("name", name)
        put("addresses", addresses)
        put("mtu", properties.mtu)
      })
      if (defaultInterface.isEmpty() && capabilities.hasCapability(NetworkCapabilities.NET_CAPABILITY_VALIDATED)) {
        defaultInterface = name
      }
    }
    if (defaultInterface.isEmpty() && interfaces.length() > 0) {
      defaultInterface = interfaces.getJSONObject(0).getString("name")
    }
    Tailcatbridge.updateNetworkState(JSONObject().apply {
      put("interfaces", interfaces)
      put("defaultInterface", defaultInterface)
    }.toString())
  }

  fun start(context: Context) {
    appContext = context.applicationContext
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
    val handle = runCatching {
      updateNativeNetworkState()
      clients.computeIfAbsent(address) { Tailcatbridge.start(it) }
    }
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
      .getOrElse { error ->
        out.write("ERR ${forwardErrorCode(error)}\n")
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
