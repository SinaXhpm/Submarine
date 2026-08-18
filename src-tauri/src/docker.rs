// Docker management commands. Each runs through a fresh exec channel on
// the live SSH session, so the user's PTY is untouched. All actions are
// non-destructive — no remove / rm / down here per product directive.
// The cross-platform story: every command uses the standard `docker` CLI,
// which behaves identically on Linux / macOS / Windows hosts. The two
// places we diverge are (a) `docker compose` (modern plugin) vs the old
// `docker-compose` script, and (b) the in-container shell for an exec
// terminal — both handled inline below.

use crate::ssh_manager::SshState;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tauri::Emitter;
use tokio::io::AsyncReadExt;
use tokio::sync::Mutex as TokioMutex;
use tokio::task::AbortHandle;

const MAX_OUTPUT_BYTES: usize = 4 * 1024 * 1024; // 4 MB cap for logs

// State for active streaming sessions (live logs, etc.) keyed by a
// frontend-generated stream id so the user can stop them individually.
#[derive(Default)]
pub struct DockerStreams {
    pub streams: Arc<TokioMutex<HashMap<String, AbortHandle>>>,
}
impl DockerStreams {
    pub fn new() -> Self { Self::default() }
}

// ---- helpers ----------------------------------------------------------------

// POSIX-safe quoting: wrap in single quotes, escape internal single quotes.
// Works on the typical Linux/macOS docker host. Windows servers running
// docker almost always use WSL2 (POSIX), so this is the right default.
fn shq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

// Validate a docker object name (container/image/volume/network/project).
// Docker's accepted character set is roughly [a-zA-Z0-9_.-], plus colon
// and slash for image:tag and repo paths. Reject anything else so we never
// hand a shell-metachar-laced string to the remote shell.
fn is_safe_name(s: &str) -> bool {
    if s.is_empty() || s.len() > 512 {
        return false;
    }
    s.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '@' | ':' | '/'))
}

// A compose file path can include spaces and unusual characters; we
// validate only against shell-injection risk (no $`;|&<>(){}newline).
fn is_safe_path(s: &str) -> bool {
    if s.is_empty() || s.len() > 4096 {
        return false;
    }
    !s.chars().any(|c| matches!(c,
        '\0' | '\n' | '\r' | ';' | '|' | '&' | '`' | '$' |
        '<' | '>' | '(' | ')' | '{' | '}' | '*' | '?' | '!' | '\\'
    ))
}

async fn run_capture(
    state: &SshState,
    session_id: &str,
    cmd: &str,
    timeout_secs: u64,
) -> Result<(i32, String), String> {
    let session_arc = {
        let connections = state.connections.lock().await;
        connections
            .get(session_id)
            .map(Arc::clone)
            .ok_or_else(|| "Session not connected".to_string())?
    };
    let channel = {
        let session = session_arc.lock().await;
        session
            .channel_open_session()
            .await
            .map_err(|e| e.to_string())?
    };
    let wrapped = format!("{}\necho __DOCK_EXIT:$?", cmd);
    channel
        .exec(true, wrapped.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let mut stream = channel.into_stream();
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let read_fut = async {
        let mut tmp = [0u8; 8192];
        loop {
            match stream.read(&mut tmp).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if buf.len() + n > MAX_OUTPUT_BYTES {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(timeout_secs), read_fut)
        .await
        .map_err(|_| format!("docker command timed out after {}s", timeout_secs))?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    let (code, out) = if let Some(idx) = text.rfind("__DOCK_EXIT:") {
        let after = &text[idx + "__DOCK_EXIT:".len()..];
        let code: i32 = after
            .trim()
            .split_whitespace()
            .next()
            .unwrap_or("1")
            .parse()
            .unwrap_or(1);
        (code, text[..idx].trim_end_matches('\n').to_string())
    } else {
        (1, text.trim_end_matches('\n').to_string())
    };
    Ok((code, out))
}

// Run a docker command, sudo-retry if the daemon-socket permission failed.
// Returns (success, output, used_sudo). Output is stdout+stderr merged.
async fn run_docker_smart(
    state: &SshState,
    session_id: &str,
    docker_cmd: &str,
    timeout: u64,
) -> Result<(bool, String, bool), String> {
    let cmd = format!("{} 2>&1", docker_cmd);
    let (code1, out1) = run_capture(state, session_id, &cmd, timeout).await?;
    let lower = out1.to_lowercase();
    let needs_sudo = code1 != 0
        && (lower.contains("permission denied")
            || lower.contains("cannot connect to the docker daemon")
            || lower.contains("dial unix /var/run/docker.sock")
            || lower.contains("/var/run/docker.sock: connect: permission"));
    if code1 == 0 || !needs_sudo {
        return Ok((code1 == 0, out1, false));
    }
    let sudo_cmd = format!("sudo -n {}", cmd);
    let (code2, out2) = run_capture(state, session_id, &sudo_cmd, timeout).await?;
    Ok((code2 == 0, out2, true))
}

#[derive(Serialize)]
pub struct DockerActionResult {
    pub success: bool,
    pub output: String,
    pub used_sudo: bool,
}

#[derive(Serialize)]
pub struct DockerTextResult {
    pub success: bool,
    pub data: String,
    pub used_sudo: bool,
}

// ---- container actions -----------------------------------------------------

#[tauri::command]
pub async fn ssh_docker_container_action(
    state: tauri::State<'_, SshState>,
    session_id: String,
    container: String,
    action: String,
) -> Result<DockerActionResult, String> {
    if !is_safe_name(&container) {
        return Err("invalid container name".into());
    }
    // Allow-list: no remove / rm — explicit product directive.
    let valid = matches!(
        action.as_str(),
        "start" | "stop" | "restart" | "pause" | "unpause" | "kill"
    );
    if !valid {
        return Err(format!("invalid action: {}", action));
    }
    let cmd = format!("docker {} {}", action, shq(&container));
    let (success, output, used_sudo) = run_docker_smart(&state, &session_id, &cmd, 60).await?;
    Ok(DockerActionResult {
        success,
        output,
        used_sudo,
    })
}

// ---- inspect / stats / logs (one-shot) -------------------------------------

#[tauri::command]
pub async fn ssh_docker_inspect(
    state: tauri::State<'_, SshState>,
    session_id: String,
    kind: String,
    name: String,
) -> Result<DockerTextResult, String> {
    if !is_safe_name(&name) {
        return Err("invalid name".into());
    }
    let prefix = match kind.as_str() {
        "container" => "container",
        "image" => "image",
        "volume" => "volume",
        "network" => "network",
        _ => return Err("invalid inspect kind".into()),
    };
    let cmd = format!("docker {} inspect {}", prefix, shq(&name));
    let (success, data, used_sudo) = run_docker_smart(&state, &session_id, &cmd, 20).await?;
    Ok(DockerTextResult {
        success,
        data,
        used_sudo,
    })
}

#[tauri::command]
pub async fn ssh_docker_stats(
    state: tauri::State<'_, SshState>,
    session_id: String,
    // Optional container filter. When Some, we ask docker for just that
    // one container's stats — an ~80 ms probe on a quiet daemon and a
    // few hundred bytes on the wire. When None, keep the full-list
    // behaviour so callers that render a resource panel across all
    // containers still work unchanged.
    container: Option<String>,
) -> Result<DockerTextResult, String> {
    // Lean field list (only what the Stats panel renders). Kept as a plain const
    // so the doubled `{{ }}` are literal docker-template braces, not format! args.
    const STATS_FMT: &str = "docker stats --no-stream --format '{{.Name}}|{{.CPUPerc}}|{{.MemUsage}}|{{.MemPerc}}|{{.NetIO}}|{{.BlockIO}}|{{.PIDs}}'";
    let cmd = match container.as_deref() {
        Some(name) => {
            if !is_safe_name(name) {
                return Err("invalid container name".into());
            }
            format!("{} {}", STATS_FMT, shq(name))
        }
        None => STATS_FMT.to_string(),
    };
    let (success, data, used_sudo) = run_docker_smart(&state, &session_id, &cmd, 20).await?;
    Ok(DockerTextResult {
        success,
        data,
        used_sudo,
    })
}

#[tauri::command]
pub async fn ssh_docker_logs(
    state: tauri::State<'_, SshState>,
    session_id: String,
    container: String,
    tail: u32,
) -> Result<DockerTextResult, String> {
    if !is_safe_name(&container) {
        return Err("invalid container name".into());
    }
    let cap_tail = tail.min(50_000);
    let cmd = format!(
        "docker logs --tail {} --timestamps {}",
        cap_tail,
        shq(&container)
    );
    let (success, data, used_sudo) = run_docker_smart(&state, &session_id, &cmd, 30).await?;
    Ok(DockerTextResult {
        success,
        data,
        used_sudo,
    })
}

// ---- networks --------------------------------------------------------------

#[tauri::command]
pub async fn ssh_docker_networks(
    state: tauri::State<'_, SshState>,
    session_id: String,
) -> Result<DockerTextResult, String> {
    let cmd = "docker network ls --format '{{.ID}}|{{.Name}}|{{.Driver}}|{{.Scope}}'";
    let (success, data, used_sudo) = run_docker_smart(&state, &session_id, cmd, 15).await?;
    Ok(DockerTextResult {
        success,
        data,
        used_sudo,
    })
}

// ---- lightweight per-resource list commands --------------------------------
// Separate from the umbrella `ssh_info_probe_section "docker"` so the Docker
// tab can ask for just what the active sub-tab needs — no `docker stats`,
// `docker system df`, or `docker images` round-trips on first paint when the
// user only wanted the container list. Each one runs a single docker call so
// the round-trip stays in the ~100–300 ms range on a quiet daemon.

#[tauri::command]
pub async fn ssh_docker_containers(
    state: tauri::State<'_, SshState>,
    session_id: String,
) -> Result<DockerTextResult, String> {
    // Lean field list (only what the UI renders) instead of `{{json .}}` — skips
    // the unused, often-bulky .Labels/.Mounts/.Networks/.Command per container.
    let cmd = "docker ps -a --format '{{.ID}}|{{.Image}}|{{.Names}}|{{.Status}}|{{.State}}|{{.Ports}}'";
    let (success, data, used_sudo) = run_docker_smart(&state, &session_id, cmd, 15).await?;
    Ok(DockerTextResult { success, data, used_sudo })
}

#[tauri::command]
pub async fn ssh_docker_images_list(
    state: tauri::State<'_, SshState>,
    session_id: String,
) -> Result<DockerTextResult, String> {
    let cmd = "docker images --format '{{.Repository}}|{{.Tag}}|{{.ID}}|{{.Size}}|{{.CreatedSince}}'";
    let (success, data, used_sudo) = run_docker_smart(&state, &session_id, cmd, 15).await?;
    Ok(DockerTextResult { success, data, used_sudo })
}

#[tauri::command]
pub async fn ssh_docker_volumes_list(
    state: tauri::State<'_, SshState>,
    session_id: String,
) -> Result<DockerTextResult, String> {
    let cmd = "docker volume ls --format '{{.Name}}|{{.Driver}}|{{.Mountpoint}}'";
    let (success, data, used_sudo) = run_docker_smart(&state, &session_id, cmd, 15).await?;
    Ok(DockerTextResult { success, data, used_sudo })
}

// ---- compose ---------------------------------------------------------------

// Compose plugin first (`docker compose`); fall back to legacy script if
// the plugin isn't installed. Caller never has to care which is present.
fn compose_prefix() -> &'static str {
    "( docker compose version >/dev/null 2>&1 && CCMD='docker compose' ) || CCMD='docker-compose'; $CCMD"
}

#[tauri::command]
pub async fn ssh_docker_compose_list(
    state: tauri::State<'_, SshState>,
    session_id: String,
) -> Result<DockerTextResult, String> {
    // Plain tabular output (NAME / STATUS / CONFIG FILES) — `--format json`
    // turned out to be flaky across docker versions: some emit a JSON array,
    // others JSONL, others prefix deprecation warnings to the JSON which
    // breaks `JSON.parse`. The text columns are stable since the compose
    // plugin was added and are trivially parseable on the frontend.
    let cmd = format!("{} ls --all", compose_prefix());
    let (success, data, used_sudo) = run_docker_smart(&state, &session_id, &cmd, 20).await?;
    Ok(DockerTextResult {
        success,
        data,
        used_sudo,
    })
}

// Per-container compose info. Reads docker's own labels — every container
// started by compose carries `com.docker.compose.project`, `…service`, and
// `…project.config_files`. Output is pipe-separated, one container per line:
//   <name>|<project>|<config_files>|<service>|<state>
// Containers without compose labels still appear (project + service empty)
// so the frontend can filter them out without a second round-trip.
#[tauri::command]
pub async fn ssh_docker_compose_per_container(
    state: tauri::State<'_, SshState>,
    session_id: String,
) -> Result<DockerTextResult, String> {
    // Note the unusual quoting: the docker format string itself contains
    // double quotes (Go template's `{{.Label "key"}}`), so we wrap the
    // whole thing in single quotes for the shell. r#"…"# lets us embed
    // those literal double quotes without backslash escaping.
    let cmd = r#"docker ps -a --no-trunc --format '{{.Names}}|{{.Label "com.docker.compose.project"}}|{{.Label "com.docker.compose.project.config_files"}}|{{.Label "com.docker.compose.service"}}|{{.State}}'"#;
    let (success, data, used_sudo) = run_docker_smart(&state, &session_id, cmd, 15).await?;
    Ok(DockerTextResult {
        success,
        data,
        used_sudo,
    })
}

#[tauri::command]
pub async fn ssh_docker_compose_services(
    state: tauri::State<'_, SshState>,
    session_id: String,
    compose_file: String,
) -> Result<DockerTextResult, String> {
    if !is_safe_path(&compose_file) {
        return Err("invalid compose file path".into());
    }
    let cmd = format!(
        "{} -f {} ps --all --format json",
        compose_prefix(),
        shq(&compose_file)
    );
    let (success, data, used_sudo) = run_docker_smart(&state, &session_id, &cmd, 30).await?;
    Ok(DockerTextResult {
        success,
        data,
        used_sudo,
    })
}

#[tauri::command]
pub async fn ssh_docker_compose_action(
    state: tauri::State<'_, SshState>,
    session_id: String,
    compose_file: String,
    action: String,
    service: Option<String>,
) -> Result<DockerActionResult, String> {
    if !is_safe_path(&compose_file) {
        return Err("invalid compose file path".into());
    }
    // No `down` — product directive: no delete-like operations.
    let valid = matches!(action.as_str(), "up" | "restart" | "pull" | "start" | "stop");
    if !valid {
        return Err(format!("invalid compose action: {}", action));
    }
    let svc = if let Some(s) = service.as_deref() {
        if !is_safe_name(s) {
            return Err("invalid service name".into());
        }
        format!(" {}", shq(s))
    } else {
        String::new()
    };
    // `up` runs detached; without `-d` it'd hang the channel waiting on
    // the foreground process group.
    let action_args = match action.as_str() {
        "up" => "up -d --no-deps".to_string(),
        other => other.to_string(),
    };
    let cmd = format!(
        "{} -f {} {}{}",
        compose_prefix(),
        shq(&compose_file),
        action_args,
        svc
    );
    let (success, output, used_sudo) = run_docker_smart(&state, &session_id, &cmd, 300).await?;
    Ok(DockerActionResult {
        success,
        output,
        used_sudo,
    })
}

#[tauri::command]
pub async fn ssh_docker_compose_view(
    state: tauri::State<'_, SshState>,
    session_id: String,
    compose_file: String,
) -> Result<DockerTextResult, String> {
    if !is_safe_path(&compose_file) {
        return Err("invalid compose file path".into());
    }
    // Plain cat, no docker — works even if the file's project is stopped.
    // Hard-cap via head so a misconfigured path can't dump a binary at us.
    let cmd = format!("head -c 524288 {} 2>&1", shq(&compose_file));
    let (code, data) = run_capture(&state, &session_id, &cmd, 10).await?;
    Ok(DockerTextResult {
        success: code == 0,
        data,
        used_sudo: false,
    })
}

// ---- prune (safe defaults only) --------------------------------------------

// We expose ONLY the conservative variants — `prune` without `-a` or
// `--volumes`. That removes stopped containers, dangling images, unused
// networks, and the build cache. Named volumes, tagged images, and
// running containers are never touched. Anything more aggressive would
// risk silent data loss and is excluded by product directive.
#[tauri::command]
pub async fn ssh_docker_prune(
    state: tauri::State<'_, SshState>,
    session_id: String,
    scope: String,
) -> Result<DockerActionResult, String> {
    let cmd = match scope.as_str() {
        "containers" => "docker container prune -f".to_string(),
        "images" => "docker image prune -f".to_string(), // dangling only — no -a
        "networks" => "docker network prune -f".to_string(),
        "builder" => "docker builder prune -f".to_string(),
        "system" => "docker system prune -f".to_string(), // safe defaults
        _ => return Err("invalid prune scope".into()),
    };
    let (success, output, used_sudo) = run_docker_smart(&state, &session_id, &cmd, 120).await?;
    Ok(DockerActionResult {
        success,
        output,
        used_sudo,
    })
}

// ---- live logs streaming ---------------------------------------------------

#[tauri::command]
pub async fn ssh_docker_logs_start(
    app: tauri::AppHandle,
    state: tauri::State<'_, SshState>,
    streams: tauri::State<'_, DockerStreams>,
    session_id: String,
    stream_id: String,
    container: String,
    tail: u32,
) -> Result<(), String> {
    if !is_safe_name(&container) {
        return Err("invalid container name".into());
    }
    let cap_tail = tail.min(50_000);

    let session_arc = {
        let connections = state.connections.lock().await;
        connections
            .get(&session_id)
            .map(Arc::clone)
            .ok_or_else(|| "Session not connected".to_string())?
    };
    let channel = {
        let session = session_arc.lock().await;
        session
            .channel_open_session()
            .await
            .map_err(|e| e.to_string())?
    };
    // -f for follow; we own the channel for the lifetime of the stream.
    // sudo fallback: we try plain first, but follow can't easily re-try
    // sudo mid-stream, so we pre-detect by running a quick probe.
    let probe_cmd = format!("docker inspect --type=container {} >/dev/null 2>&1; echo $?", shq(&container));
    let (_, probe_out) = run_capture(&state, &session_id, &probe_cmd, 10)
        .await
        .unwrap_or((1, "1".into()));
    let use_sudo = probe_out.trim() != "0";

    let raw_cmd = format!(
        "docker logs -f --tail {} --timestamps {} 2>&1",
        cap_tail,
        shq(&container)
    );
    let cmd = if use_sudo {
        format!("sudo -n {}", raw_cmd)
    } else {
        raw_cmd
    };
    channel
        .exec(true, cmd.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    let mut stream = channel.into_stream();

    let app_clone = app.clone();
    let stream_id_clone = stream_id.clone();
    let event_name = format!("docker-logs-{}", stream_id_clone);
    let close_event = format!("docker-logs-closed-{}", stream_id_clone);
    let streams_map = streams.streams.clone();

    // tokio::spawn (not tauri::async_runtime::spawn) so we get a native
    // JoinHandle and its `.abort_handle()` — we want to be able to stop
    // the stream from another command later via DockerStreams.
    let handle = tokio::spawn(async move {
        // Coalesce log output: a chatty container's `docker logs -f` can emit a
        // firehose, and one event per 8 KB read flooded the frontend (each event
        // triggered a setLogs + full re-render of the growing <pre>). Accumulate
        // and flush at most every ~30 ms, or sooner past a size cap.
        // `stream.read` is cancel-safe, so racing it against the flush timer in
        // select! never drops data.
        const FLUSH_CAP: usize = 256 * 1024;
        let mut buf = [0u8; 8192];
        let mut out: Vec<u8> = Vec::new();
        let mut flush = tokio::time::interval(std::time::Duration::from_millis(30));
        flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Emit only the complete-UTF-8 PREFIX per streaming flush and KEEP any
        // trailing incomplete multibyte sequence in `out` so the next read can
        // finish it — decoding the whole buffer lossily each flush would turn a
        // character split across a flush boundary (the 30ms tick or the size cap)
        // into replacement chars. A genuinely invalid byte is drained lossily so
        // a malformed stream can't stall the buffer. `final_flush` emits the whole
        // remainder (the stream is ending; a trailing partial will never arrive).
        let flush_logs = |out: &mut Vec<u8>, final_flush: bool| {
            if out.is_empty() { return; }
            if final_flush {
                let s = String::from_utf8_lossy(&out[..]).into_owned();
                let _ = app_clone.emit(&event_name, s);
                out.clear();
                return;
            }
            let take = match std::str::from_utf8(&out[..]) {
                Ok(_) => out.len(),
                Err(e) => match e.error_len() {
                    // Incomplete tail: keep it buffered for the next read.
                    None => e.valid_up_to(),
                    // Invalid bytes never complete — emit through them lossily and
                    // drain so `out` can't stall/grow on a malformed stream.
                    Some(bad) => {
                        let end = e.valid_up_to() + bad;
                        let s = String::from_utf8_lossy(&out[..end]).into_owned();
                        let _ = app_clone.emit(&event_name, s);
                        out.drain(..end);
                        return;
                    }
                },
            };
            if take == 0 { return; } // only a partial char buffered so far
            let s = String::from_utf8_lossy(&out[..take]).into_owned();
            let _ = app_clone.emit(&event_name, s);
            out.drain(..take);
        };
        loop {
            tokio::select! {
                r = stream.read(&mut buf) => {
                    match r {
                        Ok(0) => { flush_logs(&mut out, true); break; }
                        Ok(n) => {
                            out.extend_from_slice(&buf[..n]);
                            if out.len() >= FLUSH_CAP { flush_logs(&mut out, false); }
                        }
                        Err(_) => { flush_logs(&mut out, true); break; }
                    }
                }
                _ = flush.tick() => { flush_logs(&mut out, false); }
            }
        }
        let _ = app_clone.emit(&close_event, serde_json::json!({}));
        // Drop our own entry on natural close so the map doesn't grow.
        let mut map = streams_map.lock().await;
        map.remove(&stream_id_clone);
    });

    streams
        .streams
        .lock()
        .await
        .insert(stream_id, handle.abort_handle());
    Ok(())
}

#[tauri::command]
pub async fn ssh_docker_logs_stop(
    streams: tauri::State<'_, DockerStreams>,
    stream_id: String,
) -> Result<(), String> {
    if let Some(h) = streams.streams.lock().await.remove(&stream_id) {
        h.abort();
    }
    Ok(())
}

// ---- terminal exec into container ------------------------------------------

// Opens a PTY-backed exec channel running `docker exec -it <name> <shell>`
// with shell auto-detect: try bash → sh. Wires into the same terminal-IO
// pipeline (`terminal-output-<id>` events + write/resize/close commands)
// the regular shell terminal uses, so xterm.js on the frontend works
// without any branch — just a different exec target.
#[tauri::command]
pub async fn open_container_terminal(
    app: tauri::AppHandle,
    state: tauri::State<'_, SshState>,
    session_id: String,
    terminal_id: String,
    container: String,
    cols: u32,
    rows: u32,
    use_sudo: bool,
) -> Result<(), String> {
    use crate::ssh_manager::TerminalCommand;
    use russh::ChannelMsg;

    if !is_safe_name(&container) {
        return Err("invalid container name".into());
    }
    let session_arc = {
        let connections = state.connections.lock().await;
        connections
            .get(&session_id)
            .map(Arc::clone)
            .ok_or_else(|| "Session not connected".to_string())?
    };
    let mut channel = {
        let session = session_arc.lock().await;
        session
            .channel_open_session()
            .await
            .map_err(|e| e.to_string())?
    };
    channel
        .request_pty(false, "xterm-256color", cols, rows, 0, 0, &[])
        .await
        .map_err(|e| e.to_string())?;

    // Shell auto-detect runs inside the container: prefer bash, fall back to
    // sh, fall back to ash (Alpine). Anything more exotic and the user can
    // jump in via a regular ssh terminal and figure it out.
    let inner = r#"if command -v bash >/dev/null 2>&1; then exec bash; elif command -v sh >/dev/null 2>&1; then exec sh; elif command -v ash >/dev/null 2>&1; then exec ash; else echo 'no shell found in container' && exit 1; fi"#;
    let docker_cmd = format!("docker exec -it {} sh -c {}", shq(&container), shq(inner));
    let cmd = if use_sudo {
        format!("sudo -n {}", docker_cmd)
    } else {
        docker_cmd
    };
    channel
        .exec(true, cmd.as_bytes())
        .await
        .map_err(|e| e.to_string())?;

    let (tx, mut rx) = tokio::sync::mpsc::channel::<TerminalCommand>(32);
    let (resize_tx, mut resize_rx) = tokio::sync::watch::channel(crate::ssh_manager::PtySize {
        cols,
        rows,
    });
    state
        .terminal_txs
        .lock()
        .await
        .insert(terminal_id.clone(), tx);
    state
        .resize_txs
        .lock()
        .await
        .insert(terminal_id.clone(), resize_tx);

    let terminal_id_clone = terminal_id.clone();
    let app_clone = app.clone();
    tauri::async_runtime::spawn(async move {
        // Same coalescing as the SSH terminal (see open_terminal): batch output
        // and flush on an ~8ms timer or a size cap so a firehose can't flood the
        // WebView main thread and freeze the tab. emit_terminal_batch base64s
        // the batch into one compact event.
        use crate::ssh_manager::emit_terminal_batch;
        const FLUSH_CAP: usize = 256 * 1024;
        const FLUSH_WINDOW: std::time::Duration = std::time::Duration::from_millis(8);
        let mut out_buf: Vec<u8> = Vec::new();
        // Flush timer armed only while bytes are buffered — an idle container
        // terminal contributes zero wakeups (see open_terminal for the rationale).
        let park = || tokio::time::Instant::now() + std::time::Duration::from_secs(24 * 3600);
        let flush_timer = tokio::time::sleep_until(park());
        tokio::pin!(flush_timer);
        loop {
            tokio::select! {
                msg_opt = channel.wait() => {
                    match msg_opt {
                        Some(ChannelMsg::Data { ref data }) => {
                            let was_empty = out_buf.is_empty();
                            out_buf.extend_from_slice(data);
                            if out_buf.len() >= FLUSH_CAP {
                                emit_terminal_batch(&app_clone, &terminal_id_clone, &mut out_buf);
                            } else if was_empty {
                                flush_timer.as_mut().reset(tokio::time::Instant::now() + FLUSH_WINDOW);
                            }
                        },
                        Some(ChannelMsg::ExtendedData { ref data, ext: _ }) => {
                            let was_empty = out_buf.is_empty();
                            out_buf.extend_from_slice(data);
                            if out_buf.len() >= FLUSH_CAP {
                                emit_terminal_batch(&app_clone, &terminal_id_clone, &mut out_buf);
                            } else if was_empty {
                                flush_timer.as_mut().reset(tokio::time::Instant::now() + FLUSH_WINDOW);
                            }
                        },
                        Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) => { emit_terminal_batch(&app_clone, &terminal_id_clone, &mut out_buf); break; },
                        Some(_) => {},
                        None => { emit_terminal_batch(&app_clone, &terminal_id_clone, &mut out_buf); break; },
                    }
                },
                _ = &mut flush_timer => {
                    emit_terminal_batch(&app_clone, &terminal_id_clone, &mut out_buf);
                    flush_timer.as_mut().reset(park());
                },
                opt_cmd = rx.recv() => {
                    match opt_cmd {
                        Some(cmd) => match cmd {
                            TerminalCommand::Data(data) => {
                                if channel.data(&data[..]).await.is_err() {
                                    // Flush the last buffered output before bailing.
                                    emit_terminal_batch(&app_clone, &terminal_id_clone, &mut out_buf);
                                    break;
                                }
                            }
                        },
                        None => { let _ = channel.close().await; break; }
                    }
                },
                changed = resize_rx.changed() => {
                    if changed.is_err() { break; }
                    let size = *resize_rx.borrow();
                    let _ = channel.window_change(size.cols, size.rows, 0, 0).await;
                }
            }
        }
        let _ = app_clone.emit(&format!("terminal-closed-{}", terminal_id_clone), serde_json::json!({}));
    });

    Ok(())
}
