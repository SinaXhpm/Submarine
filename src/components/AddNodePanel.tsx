import { useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { invoke } from "@tauri-apps/api/core";
import { Cpu, X, Link2, ArrowLeftRight, Shield, ShieldAlert, Key, User, FolderPlus, Download, CheckSquare, Square, History, Plus, Copy, Play, FlaskConical } from "lucide-react";
import PasswordField from "./PasswordField";
import { useConfirm, useTextPrompt } from "../ui/confirm";
import { useSlideOverFocus } from "../ui/slideOverFocus";
import { FormError, unsavedQuestion, useUnsavedGuard } from "../ui/slideOverForm";
import { folderTree } from "../util/folderTree";
import { DEFAULT_RDP_OPTIONS, RDP_BUILTIN_EXPERIMENTAL, RDP_WINDOW_SIZES, type RdpOptions } from "../util/rdpOptions";
import { DEFAULT_SESSION_KEEP } from "../util/sessionKeep";
import { SwitchRow } from "./FormRows";
import { RdpShareFields, RdpSpeedFields } from "./RdpOptionFields";
import { VncSpeedFields } from "./VncOptionFields";
import { DEFAULT_VNC_OPTIONS, normalizeVncOptions, type VncOptions } from "../util/vncOptions";
import { IS_MOBILE, IS_WINDOWS } from "../util/platform";
import { TAG_COLOURS, customTagColour, pickerValue } from "../util/tagColours";
import { USUAL_PORTS, isFtp, type FtpProtocol, type ServerProtocol } from "../util/serverProtocol";
import { t, tn, tx, useT } from "../i18n";

// Compact "2h ago" style relative time for the attribution line.
function relTime(d: Date): string {
  const s = Math.floor((Date.now() - d.getTime()) / 1000);
  if (s < 45) return t("just now");
  const m = Math.floor(s / 60); if (m < 60) return t("{count}m ago", { count: m });
  const h = Math.floor(m / 60); if (h < 24) return t("{count}h ago", { count: h });
  const dd = Math.floor(h / 24); if (dd < 30) return t("{count}d ago", { count: dd });
  return d.toLocaleDateString();
}

// The HLC stamp is "{phys_ms}:{counter}:{node_id}" — the leading segment is the
// wall-clock millisecond the row was last written.
function hlcTime(stamp?: string | null): Date | null {
  if (!stamp) return null;
  const ms = parseInt(String(stamp).split(":")[0], 10);
  return Number.isFinite(ms) && ms > 0 ? new Date(ms) : null;
}

// Shape returned by the `parse_ssh_config` backend command. One entry per
// non-wildcard alias resolved from the user's OpenSSH config. FileZilla's site
// list (read_filezilla_sites, or pasted) adds FTP sites (`protocol`) and the
// passwords FileZilla kept.
type ImportedHost = {
  host_alias: string;
  hostname: string;
  port: number;
  user: string;
  identity_file: string | null;
  proxy_jump: string | null;
  protocol?: FtpProtocol;
  password?: string;
};

// The kind a row says it is, for its badge.
const importKind = (h: ImportedHost) =>
  h.protocol === "ftp" ? "FTP" : h.protocol ? "FTPS" : "SSH";

// What `import_ssh_key_file` hands back after putting a key file in the vault.
type ImportedSshKey = {
  id: number;
  name: string;
  reused: boolean;
  encrypted: boolean;
};

const AddNodePanel = ({ isOpen, onClose, newNode, setNewNode, onSave, credentials, sshKeys, refreshSshKeys, folders, refreshFolders, refreshServers, servers, isEditMode, formError, isMobile }: any) => {
  const t = useT();
  const [isCreatingFolder, setIsCreatingFolder] = useState(false);
  // An RDP (issue #3), telnet (#179) or VNC server has a host, port, login
  // and maybe an SSH server to go through; the SSH-only parts of the form
  // are hidden for it.
  const isRdp = newNode.protocol === "rdp";
  // This computer's drives, for sharing a whole one with an RDP server.
  const [localDrives, setLocalDrives] = useState<string[]>([]);
  useEffect(() => {
    if (isOpen && isRdp && IS_WINDOWS) invoke<string[]>("rdp_share_drives").then(setLocalDrives, () => setLocalDrives([]));
  }, [isOpen, isRdp]);
  const isTelnet = newNode.protocol === "telnet";
  const isVnc = newNode.protocol === "vnc";
  // FTP: FTPS with TLS after connecting (the usual), from the start, or plain.
  const ftp = isFtp(newNode.protocol);
  const isSsh = !isRdp && !isTelnet && !isVnc && !ftp;
  const setProtocol = (protocol: ServerProtocol) => {
    if (protocol === newNode.protocol) return;
    // The usual port of the new kind, unless another was typed.
    const usual = USUAL_PORTS;
    const port = !newNode.port || Object.values(usual).includes(newNode.port) ? usual[protocol] : newNode.port;
    const authType = protocol !== "ssh" && newNode.authType === "custom_key" ? "custom_pass" : newNode.authType;
    setNewNode({ ...newNode, protocol, port, authType });
  };
  const [newFolderName, setNewFolderName] = useState("");
  // Set while the native key picker is open so the Browse button can't be
  // fired twice — a second dialog would sit behind the first with no way to
  // tell which one the user is answering.
  const [keyBrowseBusy, setKeyBrowseBusy] = useState(false);
  const [keyBrowseNote, setKeyBrowseNote] = useState<string | null>(null);
  // A key generated from this form: its public half is shown (with a copy
  // button) while it's the selected key, since it has to go into the
  // server's authorized_keys before it can log in.
  const [generatedKey, setGeneratedKey] = useState<{ id: number; name: string; publicKey: string } | null>(null);
  const [publicKeyCopied, setPublicKeyCopied] = useState(false);
  const textPrompt = useTextPrompt();
  const confirm = useConfirm();
  const panelRef = useSlideOverFocus(isOpen);

  // The panel stays mounted and slides off-screen rather than unmounting, so
  // the note from the last key someone loaded would still be sitting there the
  // next time they open it — against a different server.
  const [portError, setPortError] = useState("");
  useEffect(() => {
    if (!isOpen) {
      setKeyBrowseNote(null);
      setGeneratedKey(null);
      setPortError("");
    }
  }, [isOpen]);

  // One save at a time: it is several writes, and a click while they run would
  // add the server a second time.
  const savingRef = useRef(false);
  const [saving, setSaving] = useState(false);
  const save = async () => {
    if (savingRef.current) return;
    // Empty, it reaches the backend as null; out of range, the connection
    // would wrap it to some other port.
    if (!Number.isInteger(newNode.port) || newNode.port < 1 || newNode.port > 65535) {
      setPortError(t("Port must be 1 to 65535."));
      return;
    }
    setPortError("");
    savingRef.current = true;
    setSaving(true);
    try {
      await onSave();
    } finally {
      savingRef.current = false;
      setSaving(false);
    }
  };

  // A click beside the form, or X, closes it at once only if nothing would be
  // lost: "Add server" starts blank and the pencil reloads the row, so what was
  // typed doesn't come back. With changes, it asks: save them, discard them or
  // keep editing (ui/slideOverForm). The revealed password stays out of what
  // the form opened with, and all of it goes with the form: nothing here may
  // outlive what DesktopApp clears on close and Lock. A typed password still
  // counts as a change, through `password_dirty`.
  const { requestClose, backdropProps } = useUnsavedGuard({
    open: isOpen,
    value: newNode,
    close: onClose,
    save,
    busy: () => savingRef.current,
    snapshot: ({ password, ...rest }: any) => JSON.stringify(rest),
    question: (opened: any) => unsavedQuestion("server", !isEditMode, opened.name),
  });

  // SSH-config import modal state. Lives here so it's colocated with the
  // "Add server" flow — the button that triggers it sits in this panel's
  // header. `hosts` is the raw list returned by parse_ssh_config; `selected`
  // is the subset of aliases the user ticked; `busy` blocks the import
  // button while add_server calls are inflight so the user can't double-fire.
  const [importOpen, setImportOpen] = useState(false);
  const [importLoading, setImportLoading] = useState(false);
  const [importBusy, setImportBusy] = useState(false);
  const [importError, setImportError] = useState<string | null>(null);
  // Separate from importError: the import worked, there's just something the
  // user has to follow up on (a key that still needs its passphrase).
  const [importNotice, setImportNotice] = useState<string | null>(null);
  const [importHosts, setImportHosts] = useState<ImportedHost[]>([]);
  const [importSelected, setImportSelected] = useState<Set<string>>(new Set());
  // Source selector for the import modal. `ssh` = the on-disk OpenSSH
  // config (desktop only); `paste` = a text blob pasted from PuTTY /
  // MobaXterm / a JSON export — parsed by the auto-detecting Rust
  // command so the user doesn't have to say which client it came from.
  const [importSource, setImportSource] = useState<"ssh" | "filezilla" | "paste">("ssh");
  const [importPasteText, setImportPasteText] = useState("");

  // Open the picker: fetch fresh from ~/.ssh/config every time so users who
  // edit the file between imports see current contents. We keep the modal
  // open on empty results so the user gets an explicit "nothing to import"
  // message rather than a silent no-op.
  // Open the modal and immediately try the SSH-config path — that's the
  // most common source on desktop. If it errors (Android, no config file,
  // permissions), the user just switches to the "Paste" tab and drops in
  // their PuTTY/MobaXterm/JSON export instead.
  const openImportModal = async () => {
    setImportOpen(true);
    setImportSource("ssh");
    setImportPasteText("");
    setImportError(null);
    setImportNotice(null);
    setImportHosts([]);
    setImportSelected(new Set());
    setImportLoading(true);
    try {
      const hosts = await invoke<ImportedHost[]>("parse_ssh_config", { path: null });
      setImportHosts(hosts);
    } catch (e: any) {
      setImportError(typeof e === "string" ? e : (e?.message || String(e)));
    } finally {
      setImportLoading(false);
    }
  };

  // Parse whatever's in the textarea. Rust auto-detects the format so we
  // don't need three separate importers here.
  const parsePastedImport = async () => {
    setImportLoading(true);
    setImportError(null);
    setImportNotice(null);
    setImportHosts([]);
    setImportSelected(new Set());
    try {
      const hosts = await invoke<ImportedHost[]>("parse_client_import", { text: importPasteText });
      setImportHosts(hosts);
    } catch (e: any) {
      setImportError(typeof e === "string" ? e : (e?.message || String(e)));
    } finally {
      setImportLoading(false);
    }
  };

  const switchImportSource = async (next: "ssh" | "filezilla" | "paste") => {
    if (importBusy || importSource === next) return;
    setImportSource(next);
    setImportError(null);
    setImportNotice(null);
    setImportHosts([]);
    setImportSelected(new Set());
    if (next === "ssh" || next === "filezilla") {
      setImportLoading(true);
      try {
        const hosts = next === "ssh"
          ? await invoke<ImportedHost[]>("parse_ssh_config", { path: null })
          : await invoke<ImportedHost[]>("read_filezilla_sites");
        setImportHosts(hosts);
      } catch (e: any) {
        setImportError(typeof e === "string" ? e : (e?.message || String(e)));
      } finally {
        setImportLoading(false);
      }
    }
  };

  const closeImportModal = () => {
    if (importBusy) return;
    setImportOpen(false);
    setImportError(null);
    setImportNotice(null);
    setImportHosts([]);
    setImportSelected(new Set());
    setImportPasteText("");
  };

  const toggleSelectAll = () => {
    if (importSelected.size === importHosts.length) {
      setImportSelected(new Set());
    } else {
      setImportSelected(new Set(importHosts.map((h) => h.host_alias)));
    }
  };

  const toggleOne = (alias: string) => {
    setImportSelected((prev) => {
      const next = new Set(prev);
      if (next.has(alias)) next.delete(alias);
      else next.add(alias);
      return next;
    });
  };

  // Fire one `add_server` per selected host. A host that named an IdentityFile
  // comes in key-authenticated; anything else lands as auth_type "custom_pass"
  // with no password — a valid row that won't connect until the user fills it
  // in. Errors per-host are surfaced but don't abort the batch; the modal
  // shows how many landed and how many failed.
  const importSelectedHosts = async () => {
    if (importSelected.size === 0 || importBusy) return;
    setImportBusy(true);
    setImportError(null);
    setImportNotice(null);
    let ok = 0;
    let failed = 0;
    let lastErr: string | null = null;
    // Several hosts routinely point at the same `~/.ssh/id_ed25519`. The Rust
    // side already dedupes by key content, but caching here means one read and
    // one vault write for the whole batch instead of one per host. `null`
    // records a path we already tried and couldn't use, so we don't retry a
    // missing file once per host that mentions it.
    const keysByPath = new Map<string, ImportedSshKey | null>();
    let keysAttached = 0;
    let keysNeedingPassphrase = 0;
    // Key files from this computer, going into a shared profile, would sync
    // to everyone it's shared with: ask first. The safe answer (Escape, or
    // the focused button) imports the hosts without them.
    const keyFiles = [
      ...new Set(importHosts.filter((h) => importSelected.has(h.host_alias) && h.identity_file).map((h) => h.identity_file as string)),
    ];
    let withKeys = true;
    // FileZilla's site list keeps passwords: in a shared profile they would go
    // to everyone it's shared with, so they are asked about too. The safe
    // answer leaves them out (each server asks when connecting).
    const withPasswordsAsked = importHosts.some((h) => importSelected.has(h.host_alias) && h.password);
    const sharedProfile = keyFiles.length > 0 || withPasswordsAsked
      ? await invoke<{ current: string | null }>("share_key_state").then((s) => !!s.current).catch(() => false)
      : false;
    let withPasswords = true;
    if (withPasswordsAsked && sharedProfile) {
      withPasswords = await confirm({
        title: t("Add saved passwords to a shared profile?"),
        message: t("This profile is shared. The passwords saved with these sites would go to everyone you share it with. Import the sites with them, or without (each one asks when connecting)?"),
        okLabel: t("Import with passwords"),
        cancelLabel: t("Without passwords"),
        safeDefault: true,
      });
    }
    if (keyFiles.length > 0) {
      const shared = sharedProfile;
      if (shared) {
        withKeys = await confirm({
          title: t("Add your key files to a shared profile?"),
          message: tn(
            keyFiles.length,
            "This profile is shared. This private key from this computer would go to everyone you share it with: {keys}. Import the hosts with it, or without (you can pick keys for them later)?",
            "This profile is shared. These private keys from this computer would go to everyone you share it with: {keys}. Import the hosts with them, or without (you can pick keys for them later)?",
            { keys: keyFiles.join(", ") },
          ),
          okLabel: t("Import with keys"),
          cancelLabel: t("Without keys"),
          safeDefault: true,
        });
      }
    }
    for (const host of importHosts) {
      if (!importSelected.has(host.host_alias)) continue;
      let key: ImportedSshKey | null = null;
      if (host.identity_file && withKeys) {
        if (keysByPath.has(host.identity_file)) {
          key = keysByPath.get(host.identity_file) ?? null;
        } else {
          try {
            key = await invoke<ImportedSshKey>("import_ssh_key_file", {
              path: host.identity_file,
              name: null,
            });
          } catch {
            // An unreadable IdentityFile isn't a reason to skip the host —
            // the server row is still worth having. It just imports
            // password-less, exactly as it did before keys were attached.
            key = null;
          }
          keysByPath.set(host.identity_file, key);
          if (key) {
            keysAttached++;
            if (key.encrypted) keysNeedingPassphrase++;
          }
        }
      }
      try {
        const id = await invoke<number>("add_server", {
          name: host.host_alias,
          host: host.hostname || host.host_alias,
          port: host.port || (host.protocol ? USUAL_PORTS[host.protocol] : 22),
          // No user in the export (MobaXterm's <default>, an ssh_config
          // entry without User): leave it blank, so connecting asks
          // "Login as" — where empty still means root (#72).
          username: host.user ? host.user.trim() : "",
          // A password FileZilla kept (and, in a shared profile, allowed).
          password: host.password && withPasswords && !key ? host.password : null,
          credentialId: null,
          folderId: null,
          proxyType: "none",
          proxyHost: "",
          proxyPort: 1080,
          tunnels: [],
          authType: key ? "custom_key" : "custom_pass",
          keyId: key ? key.id : null,
          autostart: false,
          mirrors: [],
          color: null,
          persist: false,
        });
        // An FTP site from FileZilla's list: its kind goes with it.
        if (host.protocol) {
          await invoke("set_server_protocol", { id, value: host.protocol, persist: false });
        }
        ok++;
      } catch (e: any) {
        failed++;
        lastErr = typeof e === "string" ? e : (e?.message || String(e));
      }
    }
    // One save for all of them: each writes the whole vault, so one per host
    // grew with the square of the hosts. Until it's done they're in memory. If
    // the app dies first, the file is as it was and the import can be run again.
    let saveErr: string | null = null;
    if (ok > 0) {
      try {
        await invoke("persist_vault");
      } catch (e: any) {
        saveErr = typeof e === "string" ? e : (e?.message || String(e));
      }
    }
    setImportBusy(false);
    if (saveErr) {
      setImportError(t("{ok} imported, but they couldn't be saved: {error}", { ok, error: saveErr }));
    } else if (failed > 0) {
      setImportError(lastErr
        ? t("{ok} imported, {failed} failed: {error}", { ok, failed, error: lastErr })
        : t("{ok} imported, {failed} failed", { ok, failed }));
    }
    if (refreshServers) {
      try { await refreshServers(); } catch { /* refresh failure isn't fatal */ }
    }
    if (keysAttached > 0 && refreshSshKeys) {
      try { await refreshSshKeys(); } catch { /* same — the list refreshes on next open */ }
    }
    // A passphrase-protected key's passphrase isn't anywhere in the config
    // file. The user can add it under Settings › SSH keys, or leave it and be
    // asked at connect time (issue #30) — either way, hold the modal open and
    // point it out so the import isn't silently half-finished.
    if (keysNeedingPassphrase > 0) {
      setImportNotice(tn(
        keysAttached,
        "{ok} imported, {count} SSH key attached. {need} of them need a passphrase — add it under Settings › SSH keys, or leave it empty to be asked when connecting.",
        "{ok} imported, {count} SSH keys attached. {need} of them need a passphrase — add it under Settings › SSH keys, or leave it empty to be asked when connecting.",
        { ok, need: keysNeedingPassphrase },
      ));
      return;
    }
    if (failed === 0 && !saveErr) {
      closeImportModal();
    }
  };

  // Browse to a key on disk and select it on this server in one step. The key
  // goes into the vault (deduped by content) rather than being referenced by
  // path — the vault is what syncs and what gets exported, so a server that
  // works here has to keep working on a machine where ~/.ssh looks different.
  const browseForKey = async () => {
    if (keyBrowseBusy) return;
    setKeyBrowseBusy(true);
    setKeyBrowseNote(null);
    try {
      const path = await invoke<string | null>("pick_ssh_key_file");
      if (!path) return;
      const key = await invoke<ImportedSshKey>("import_ssh_key_file", { path, name: null });
      if (refreshSshKeys) await refreshSshKeys();
      setNewNode({ ...newNode, keyId: key.id.toString() });
      setKeyBrowseNote(
        key.encrypted
          ? t("Loaded \"{name}\" — it's passphrase-protected. Add the passphrase under Settings › SSH keys, or leave it empty to be asked when connecting.", { name: key.name })
          : key.reused
            ? t("\"{name}\" was already in your vault — selected it.", { name: key.name })
            : t("Loaded \"{name}\" into your vault.", { name: key.name }),
      );
    } catch (e: any) {
      setKeyBrowseNote(typeof e === "string" ? e : (e?.message || String(e)));
    } finally {
      setKeyBrowseBusy(false);
    }
  };

  // Make a new key without leaving the form (#23): the same Ed25519 key the
  // SSH keys page generates, selected on this server straight away.
  const generateKey = async () => {
    if (keyBrowseBusy) return;
    const suggested = `${String(newNode.name || newNode.host || "server").trim()}-key`;
    const name = await textPrompt({
      title: t("Generate SSH key"),
      message: t("Give this key a name — it's just for your reference here in Submarine."),
      initialValue: suggested,
      okLabel: t("Generate"),
      validate: (v) => v.length === 0 ? t("Name is required") : null,
    });
    if (!name) return;
    setKeyBrowseBusy(true);
    setKeyBrowseNote(null);
    setGeneratedKey(null);
    try {
      const key = await invoke<{ id: number; public_key: string }>("generate_ssh_key", { name });
      if (refreshSshKeys) await refreshSshKeys();
      setNewNode({ ...newNode, keyId: key.id.toString() });
      setGeneratedKey({ id: key.id, name, publicKey: key.public_key });
      setPublicKeyCopied(false);
    } catch (e: any) {
      setKeyBrowseNote(typeof e === "string" ? e : (e?.message || String(e)));
    } finally {
      setKeyBrowseBusy(false);
    }
  };

  const copyPublicKey = async () => {
    if (!generatedKey) return;
    try {
      await navigator.clipboard.writeText(generatedKey.publicKey);
      setPublicKeyCopied(true);
    } catch {
      setKeyBrowseNote(t("Couldn't copy — select the key text and copy it by hand."));
    }
  };

  const handleCreateFolder = async () => {
    if (!newFolderName.trim()) return;
    try {
      await invoke("add_folder", { name: newFolderName, parentId: null });
      setNewFolderName("");
      setIsCreatingFolder(false);
      refreshFolders();
    } catch (e) {
      console.error(e);
    }
  };

  const updateTunnel = (idx: number, field: string, value: string | boolean) => {
    const newT = [...newNode.tunnels];
    newT[idx][field] = value;
    setNewNode({ ...newNode, tunnels: newT });
  };

  return (
    // Mounted while closed too (to slide out): inert then, so Tab can't reach
    // its fields. Clicks from the import dialog (a portal, so its events
    // bubble here) aren't clicks beside the form.
    <div
      inert={isOpen ? undefined : ""}
      className={`fixed inset-0 z-50 transition-all duration-500 flex justify-end ${isOpen ? 'bg-black/60 backdrop-blur-sm' : 'opacity-0 pointer-events-none'}`}
      {...backdropProps}
    >
      <div ref={panelRef} tabIndex={-1} className={`w-full ${isMobile ? 'max-w-full' : 'max-w-[450px]'} bg-background border-l border-white/5 shadow-2xl transition-all duration-500 h-full flex flex-col outline-none ${isOpen ? 'translate-x-0' : 'translate-x-full'}`} onClick={(e) => e.stopPropagation()}>

        {/* Header */}
        <div className="flex justify-between items-center p-6 border-b border-white/5 shrink-0">
          <div className="flex items-center gap-3">
            <div className="w-8 h-8 bg-primary/10 rounded-lg flex items-center justify-center text-primary shadow-inner border border-primary/20">
              <Cpu size={16} />
            </div>
            <h2 className="text-[14px] font-bold text-white tracking-tight">{t("Server details")}</h2>
          </div>
          <div className="flex items-center gap-2">
            {/* Import only makes sense when creating a fresh row — editing an
                existing server is a rename/reconfigure flow, not an import.
                Available on Android now that the modal supports a "Paste
                PuTTY / MobaXterm / JSON" source that doesn't need a local
                ~/.ssh/config. The SSH-config tab still errors gracefully
                there — the user just picks the paste tab. */}
            {!isEditMode && (isSsh || ftp) && (
              <button
                onClick={openImportModal}
                title={t("Import from ~/.ssh/config")}
                className="h-8 px-2.5 flex items-center gap-1.5 text-[11px] font-bold text-primary bg-primary/10 border border-primary/30 hover:bg-primary/20 rounded-xl transition-all"
              >
                <Download size={12} />
                <span className="hidden sm:inline">{t("Import from ~/.ssh/config")}</span>
                <span className="sm:hidden">{t("Import")}</span>
              </button>
            )}
            <button aria-label={t("Close")} onClick={() => void requestClose()} className="w-8 h-8 flex items-center justify-center text-zinc-500 hover:text-white bg-black border border-white/5 hover:bg-white/10 rounded-xl transition-all shadow-inner">
              <X size={16} />
            </button>
          </div>
        </div>

        {/* Scrollable Content */}
        <div className="p-6 flex-1 overflow-y-auto custom-scrollbar space-y-6">
          <FormError message={portError || formError} active={isOpen} />

          <div className="grid grid-cols-1 gap-4">
            {/* SSH, RDP, telnet or VNC. An RDP server opens in this
                computer's RDP app, a telnet one in a terminal, a VNC one as
                its screen in the tab; all directly or through an SSH server. */}
            <div className="space-y-1.5">
              <label htmlFor="server-type" className="text-[11px] font-bold text-zinc-400 ml-1">{t("Type")}</label>
              <select
                id="server-type"
                aria-label={t("Server type")}
                value={isRdp ? "rdp" : isTelnet ? "telnet" : isVnc ? "vnc" : ftp ? "ftps" : "ssh"}
                onChange={(e) => setProtocol(e.target.value as ServerProtocol)}
                className="w-full h-9 bg-[#1a1a1e] rounded-lg px-3 text-[12px] text-white border border-white/10 outline-none focus:border-primary/50 focus:bg-[#232328] transition-all shadow-inner"
              >
                <option value="ssh" className="bg-[#1a1a1e] text-white">SSH</option>
                <option value="rdp" className="bg-[#1a1a1e] text-white">{t("RDP (Remote Desktop)")}</option>
                <option value="telnet" className="bg-[#1a1a1e] text-white">{t("Telnet (not encrypted)")}</option>
                <option value="vnc" className="bg-[#1a1a1e] text-white">VNC</option>
                <option value="ftps" className="bg-[#1a1a1e] text-white">{t("FTP or FTPS (files)")}</option>
              </select>
              {ftp && (
                <div className="space-y-1.5 pt-1">
                  <label htmlFor="ftp-security" className="text-[11px] font-bold text-zinc-400 ml-1">{t("Encryption")}</label>
                  <select
                    id="ftp-security"
                    value={newNode.protocol}
                    onChange={(e) => setProtocol(e.target.value as FtpProtocol)}
                    className="w-full h-9 bg-[#1a1a1e] rounded-lg px-3 text-[12px] text-white border border-white/10 outline-none focus:border-primary/50 focus:bg-[#232328] transition-all shadow-inner"
                  >
                    <option value="ftps" className="bg-[#1a1a1e] text-white">{t("FTPS: TLS after connecting (usual)")}</option>
                    <option value="ftps-implicit" className="bg-[#1a1a1e] text-white">{t("FTPS: TLS from the start (port 990)")}</option>
                    <option value="ftp" className="bg-[#1a1a1e] text-white">{t("None: plain FTP (not encrypted)")}</option>
                  </select>
                  {newNode.protocol !== "ftp" && (
                    <p className="text-[10px] text-zinc-500 ml-1">{t("A certificate that no public authority vouches for is shown to you to check once, like an SSH host key.")}</p>
                  )}
                </div>
              )}
              {newNode.protocol === "ftp" && (
                <p role="note" className="flex gap-2 px-3 py-2 rounded-lg border border-amber-500/25 bg-amber-500/10 text-[11px] leading-snug text-amber-200/90">
                  <ShieldAlert size={13} className="shrink-0 mt-px" aria-hidden="true" />
                  <span>{t("Plain FTP isn't encrypted: the password and the files cross the network as they are. Pick FTPS if the server offers it, or go through an SSH server (below) to keep it private up to that server.")}</span>
                </p>
              )}
              {isTelnet && (
                <p role="note" className="flex gap-2 px-3 py-2 rounded-lg border border-amber-500/25 bg-amber-500/10 text-[11px] leading-snug text-amber-200/90">
                  <ShieldAlert size={13} className="shrink-0 mt-px" aria-hidden="true" />
                  <span>{t("Telnet isn't encrypted: what is typed, passwords too, crosses the network as plain text. Use it on a network you trust, or through an SSH server: then only the part after that server isn't encrypted.")}</span>
                </p>
              )}
              {isVnc && !newNode.jumpHostId && (
                <p role="note" className="flex gap-2 px-3 py-2 rounded-lg border border-amber-500/25 bg-amber-500/10 text-[11px] leading-snug text-amber-200/90">
                  <ShieldAlert size={13} className="shrink-0 mt-px" aria-hidden="true" />
                  <span>{t("VNC isn't encrypted: the screen and what is typed cross the network as they are. Pick an SSH server to go through (below) to keep it private up to that server.")}</span>
                </p>
              )}
            </div>

            <div className="space-y-1.5">
              <label className="text-[11px] font-bold text-zinc-400 ml-1">{t("Name")}</label>
              <input type="text" className="w-full h-9 bg-[#1a1a1e] rounded-lg px-3 text-[12px] text-white border border-white/10 outline-none focus:border-primary/50 focus:bg-[#232328] transition-all shadow-inner" placeholder={t("e.g. Work server")} value={newNode.name} onChange={e => setNewNode({ ...newNode, name: e.target.value })} />
            </div>

            <div className="space-y-1.5">
              <label className="text-[11px] font-bold text-zinc-400 ml-1">{t("Folder")}</label>
              {!isCreatingFolder ? (
                <div className="flex gap-2">
                  <select className="flex-1 h-9 bg-[#1a1a1e] rounded-lg px-3 text-[12px] text-white border border-white/10 outline-none focus:border-primary/50 focus:bg-[#232328] transition-all shadow-inner" value={newNode.folderId} onChange={e => setNewNode({ ...newNode, folderId: e.target.value })}>
                    <option value="" className="bg-[#1a1a1e] text-zinc-500">{t("-- Root --")}</option>
                    {/* Nested folders by their full path, in path order. */}
                    {(() => {
                      const tree = folderTree(folders);
                      return (folders ?? [])
                        .map((f: any) => ({ id: f.id, label: tree.label(f.id) }))
                        .sort((a: any, b: any) => a.label.localeCompare(b.label))
                        .map((f: any) => <option key={f.id} value={f.id.toString()} className="bg-[#1a1a1e] text-white">{f.label}</option>);
                    })()}
                  </select>
                  <button aria-label={t("New folder")} onClick={() => setIsCreatingFolder(true)} className="w-9 h-9 bg-white/5 border border-white/10 rounded-lg text-primary hover:bg-white/10 transition-all flex items-center justify-center shrink-0">
                    <FolderPlus size={14} />
                  </button>
                </div>
              ) : (
                <div className="flex gap-2 animate-in fade-in">
                  <input type="text" className="flex-1 h-9 bg-[#1a1a1e] rounded-lg px-3 text-[12px] text-white border border-white/10 outline-none focus:border-primary/50 transition-all shadow-inner" placeholder={t("Folder name…")} value={newFolderName} onChange={e => setNewFolderName(e.target.value)} onKeyDown={e => e.key === 'Enter' && handleCreateFolder()} />
                  <button onClick={handleCreateFolder} className="h-9 px-3 bg-primary text-black font-bold text-[12px] rounded-lg hover:bg-primary transition-all">{t("Add")}</button>
                  <button aria-label={t("Cancel")} onClick={() => setIsCreatingFolder(false)} className="w-9 h-9 bg-white/5 text-zinc-400 rounded-lg hover:bg-white/10 transition-all flex justify-center items-center shrink-0"><X size={14} /></button>
                </div>
              )}
            </div>

            <div className="space-y-1.5">
              <label className="text-[11px] font-bold text-zinc-400 ml-1">{t("Tag colour")}</label>
              <div className="flex items-center gap-2 flex-wrap">
                {TAG_COLOURS.map((p) => {
                  const selected = (newNode.color ?? null) === p.value;
                  return (
                    <button
                      key={p.name}
                      type="button"
                      title={t(p.name)}
                      aria-label={t(p.name)}
                      aria-pressed={selected}
                      onClick={() => setNewNode({ ...newNode, color: p.value })}
                      className={`w-6 h-6 rounded-full ${p.value ? "" : "bg-zinc-500"} ring-2 ring-transparent hover:ring-white/30 transition-all ${selected ? "ring-white" : ""}`}
                      style={p.value ? { background: p.value } : undefined}
                    />
                  );
                })}
                {/* Any other colour: the system's colour picker, as a swatch of its own
                    (it shows the colour once one is chosen). */}
                {(() => {
                  const custom = customTagColour(newNode.color);
                  return (
                    <label
                      title={t("Another colour…")}
                      className={`relative w-6 h-6 rounded-full cursor-pointer ring-2 ring-transparent hover:ring-white/30 transition-all ${custom ? "ring-white" : ""}`}
                      style={{ background: custom ?? "conic-gradient(#ef4444, #eab308, #22c55e, #06b6d4, #3b82f6, #a855f7, #ef4444)" }}
                    >
                      <input
                        type="color"
                        aria-label={t("Another colour")}
                        value={pickerValue(newNode.color ?? null)}
                        onChange={(e) => setNewNode({ ...newNode, color: e.target.value })}
                        className="absolute inset-0 w-full h-full opacity-0 cursor-pointer"
                      />
                    </label>
                  );
                })()}
              </div>
            </div>

            <div className="grid grid-cols-4 gap-4">
              <div className="col-span-3 space-y-1.5">
                <label className="text-[11px] font-bold text-zinc-400 ml-1">{t("Host")}</label>
                <input type="text" className="w-full h-9 bg-[#1a1a1e] rounded-lg px-3 text-[12px] text-white border border-white/10 outline-none focus:border-primary/50 focus:bg-[#232328] transition-all shadow-inner" placeholder={t("192.168.1.1 or example.com")} value={newNode.host} onChange={e => setNewNode({ ...newNode, host: e.target.value })} />
              </div>
              <div className="space-y-1.5">
                <label className="text-[11px] font-bold text-zinc-400 ml-1">{t("Port")}</label>
                <input type="number" className="w-full h-9 bg-[#1a1a1e] rounded-lg px-3 text-[12px] text-white border border-white/10 outline-none focus:border-primary/50 focus:bg-[#232328] transition-all shadow-inner" value={newNode.port} onChange={e => setNewNode({ ...newNode, port: parseInt(e.target.value) })} />
              </div>
            </div>

          </div>

          <div className="pt-5 border-t border-white/5 space-y-4">
            <div className="flex justify-between items-center">
              <label className="text-[11px] font-bold text-zinc-400">{t("How to log in")}</label>
              <select
                className="bg-transparent text-[12px] font-bold text-primary outline-none cursor-pointer"
                value={newNode.authType}
                onChange={(e) => setNewNode({ ...newNode, authType: e.target.value })}
              >
                <option value="vault" className="bg-[#121215] text-primary">{t("Use a saved login")}</option>
                <option value="custom_pass" className="bg-[#121215] text-primary">{t("Type a password")}</option>
                {isSsh && <option value="custom_key" className="bg-[#121215] text-primary">{t("Use an SSH key")}</option>}
              </select>
            </div>

            {newNode.authType === 'vault' && (() => {
              const selectedCred = credentials?.find((c: any) => c.id?.toString() === newNode.credentialId?.toString());
              return (
                <div className="space-y-3 animate-in fade-in slide-in-from-top-1">
                  <div className="space-y-1.5">
                    <label className="text-[11px] font-bold text-zinc-400 ml-1">{t("Saved login")}</label>
                    <select className="w-full h-9 bg-[#1a1a1e] rounded-lg px-3 text-[12px] text-zinc-300 border border-white/10 outline-none focus:border-primary/50 transition-all shadow-inner" value={newNode.credentialId} onChange={e => setNewNode({ ...newNode, credentialId: e.target.value })}>
                      <option value="" className="bg-[#1a1a1e] text-zinc-500">{t("-- Pick one --")}</option>
                      {credentials?.map((c: any) => <option key={c.id} value={c.id.toString()} className="bg-[#1a1a1e] text-zinc-300">{c.name} ({c.username})</option>)}
                    </select>
                  </div>
                  {selectedCred && (
                    <div className="px-3 py-2 bg-primary/5 border border-primary/20 rounded-lg text-[11px] font-mono text-zinc-300 flex items-center gap-2">
                      <User size={11} className="text-primary shrink-0" />
                      <span className="text-zinc-500">{t("Sign in as")}</span>
                      <span className="text-primary font-bold">{selectedCred.username || "—"}</span>
                      <span className="text-zinc-600 ml-auto text-[10px]">{selectedCred.auth_type === 'key' ? t("using key") : t("using password")}</span>
                    </div>
                  )}
                </div>
              );
            })()}

            {newNode.authType === 'custom_pass' && isRdp && (
              <div className="space-y-3 animate-in fade-in">
                <div className="space-y-1.5">
                  <label className="text-[11px] font-bold text-zinc-400 ml-1">{t("Username")}</label>
                  <input type="text" placeholder={t("Asked by the RDP app")} value={newNode.username || ""} onChange={e => setNewNode({ ...newNode, username: e.target.value })} autoCapitalize="none" autoCorrect="off" spellCheck={false} className="w-full h-9 bg-[#1a1a1e] rounded-lg px-3 text-[12px] text-white border border-white/10 outline-none focus:border-primary/50 transition-all shadow-inner" />
                  <p className="text-[10px] text-zinc-500 ml-1">{t("DOMAIN\\user works too.")}</p>
                </div>
                <div className="space-y-1.5">
                  <label className="text-[11px] font-bold text-zinc-400 ml-1">{t("Password")}</label>
                  <PasswordField
                    value={newNode.password || ""}
                    onChange={(v) => setNewNode({ ...newNode, password: v, password_dirty: true })}
                    className="w-full h-9 bg-[#1a1a1e] rounded-lg px-3 text-[12px] text-white border border-white/10 outline-none focus:border-primary/50 transition-all shadow-inner"
                    placeholder="••••••"
                  />
                  <p className="text-[10px] text-zinc-500 ml-1">{t("Leave empty to be asked when connecting. Remote Desktop Connection on Windows gets it too.")}</p>
                </div>
              </div>
            )}

            {newNode.authType === 'custom_pass' && !isRdp && (
              <div className="space-y-3 animate-in fade-in">
                {!isVnc && <div className="space-y-1.5">
                  <label className="text-[11px] font-bold text-zinc-400 ml-1">{t("Username")}</label>
                  <input type="text" placeholder={isTelnet ? t("For your reference") : ftp ? t("Empty: anonymous") : t("Ask when connecting")} value={newNode.username || ""} onChange={e => setNewNode({ ...newNode, username: e.target.value })} autoCapitalize="none" autoCorrect="off" spellCheck={false} className="w-full h-9 bg-[#1a1a1e] rounded-lg px-3 text-[12px] text-white border border-white/10 outline-none focus:border-primary/50 transition-all shadow-inner" />
                </div>}
                <div className="space-y-1.5">
                  <label className="text-[11px] font-bold text-zinc-400 ml-1">{isVnc ? t("VNC password") : t("Password")}</label>
                  <PasswordField
                    value={newNode.password || ""}
                    onChange={(v) => setNewNode({ ...newNode, password: v, password_dirty: true })}
                    className="w-full h-9 bg-[#1a1a1e] rounded-lg px-3 text-[12px] text-white border border-white/10 outline-none focus:border-primary/50 transition-all shadow-inner"
                    placeholder="••••••"
                  />
                  <p className="text-[10px] text-zinc-500 ml-1">
                    {isTelnet
                      ? t("A telnet server asks for the login in the terminal. Type it there; at the password prompt, the terminal's key button types the saved one.")
                      : isVnc
                        ? t("Leave empty to be asked when the server wants one. VNC uses its first 8 characters.")
                        : t("Leave empty to be asked when connecting.")}
                  </p>
                </div>
              </div>
            )}

            {newNode.authType === 'custom_key' && isSsh && (
              <div className="space-y-3 animate-in fade-in">
                <div className="space-y-1.5">
                  <label className="text-[11px] font-bold text-zinc-400 ml-1">{t("Username")}</label>
                  <input type="text" placeholder={t("Ask when connecting")} value={newNode.username || ""} onChange={e => setNewNode({ ...newNode, username: e.target.value })} className="w-full h-9 bg-[#1a1a1e] rounded-lg px-3 text-[12px] text-white border border-white/10 outline-none focus:border-primary/50 transition-all shadow-inner" />
                </div>
                <div className="space-y-1.5">
                  <div className="flex items-center justify-between ml-1">
                    <label className="text-[11px] font-bold text-zinc-400">{t("SSH key")}</label>
                    {/* Loading a key from disk is the common case for someone
                        moving over from the `ssh` command line — the key is
                        already in ~/.ssh and copy-pasting it out of a terminal
                        is the worst part of setting up a first server. */}
                    <div className="flex items-center gap-1.5">
                      <button
                        type="button"
                        onClick={generateKey}
                        disabled={keyBrowseBusy}
                        title={t("Make a new SSH key and use it for this server")}
                        className="h-6 px-2 flex items-center gap-1 text-[10px] font-bold text-primary bg-primary/10 border border-primary/30 hover:bg-primary/20 disabled:opacity-40 disabled:cursor-not-allowed rounded-lg transition-all"
                      >
                        <Plus size={10} />
                        {t("Generate")}
                      </button>
                      <button
                        type="button"
                        onClick={browseForKey}
                        disabled={keyBrowseBusy}
                        className="h-6 px-2 flex items-center gap-1 text-[10px] font-bold text-primary bg-primary/10 border border-primary/30 hover:bg-primary/20 disabled:opacity-40 disabled:cursor-not-allowed rounded-lg transition-all"
                      >
                        <Key size={10} />
                        {keyBrowseBusy ? t("Loading…") : t("Load from file")}
                      </button>
                    </div>
                  </div>
                  <select className="w-full h-9 bg-[#1a1a1e] rounded-lg px-3 text-[12px] text-zinc-300 border border-white/10 outline-none focus:border-primary/50 transition-all shadow-inner" value={newNode.keyId} onChange={e => setNewNode({ ...newNode, keyId: e.target.value })}>
                    <option value="" className="bg-[#1a1a1e] text-zinc-500">{t("-- Pick a key --")}</option>
                    {sshKeys?.map((k: any) => <option key={k.id} value={k.id.toString()} className="bg-[#1a1a1e] text-zinc-300">{k.name}</option>)}
                  </select>
                  {keyBrowseNote && (
                    <div className="p-2 bg-white/5 border border-white/10 rounded-lg text-[11px] text-zinc-400 break-words">
                      {keyBrowseNote}
                    </div>
                  )}
                  {generatedKey && newNode.keyId === generatedKey.id.toString() && (
                    <div className="p-2 bg-white/5 border border-white/10 rounded-lg text-[11px] text-zinc-400 space-y-1.5">
                      <div>
                        {tx("Generated \"{name}\" and picked it for this server. Add its public key to {path} on the server before connecting:", {
                          name: generatedKey.name,
                          path: <code className="text-zinc-300">~/.ssh/authorized_keys</code>,
                        })}
                      </div>
                      <code className="block p-1.5 bg-black/40 rounded text-[10.5px] text-zinc-300 font-mono break-all select-text">
                        {generatedKey.publicKey}
                      </code>
                      <button
                        type="button"
                        onClick={copyPublicKey}
                        className="h-6 px-2 flex items-center gap-1 text-[10px] font-bold text-primary bg-primary/10 border border-primary/30 hover:bg-primary/20 rounded-lg transition-all"
                      >
                        <Copy size={10} />
                        {publicKeyCopied ? t("Copied") : t("Copy public key")}
                      </button>
                    </div>
                  )}
                </div>
              </div>
            )}
          </div>

          <div className="pt-5 border-t border-white/5 space-y-4">
            {(isSsh || ftp) && (<>
            <div className="flex justify-between items-center">
              <label className="text-[11px] font-bold text-zinc-400">{t("Proxy")}</label>
              <select className="bg-transparent text-[12px] font-bold text-zinc-300 outline-none cursor-pointer" value={newNode.proxyType} onChange={e => setNewNode({ ...newNode, proxyType: e.target.value })}>
                <option value="none" className="bg-[#121215] text-zinc-400">{t("No proxy")}</option>
                <option value="socks5" className="bg-[#121215] text-zinc-400">SOCKS5</option>
                <option value="http" className="bg-[#121215] text-zinc-400">HTTP</option>
              </select>
            </div>

            {newNode.proxyType !== 'none' && (
              <div className="grid grid-cols-4 gap-3 animate-in fade-in">
                <input
                  className="col-span-3 h-9 bg-[#1a1a1e] rounded-lg px-3 text-[12px] text-white border border-white/10 outline-none focus:border-primary/50 transition-all shadow-inner"
                  placeholder={t("Proxy Host")}
                  value={newNode.proxyHost || ""}
                  onChange={e => setNewNode({ ...newNode, proxyHost: e.target.value })}
                />
                <input
                  type="number"
                  className="h-9 bg-[#1a1a1e] rounded-lg px-3 text-[12px] text-white border border-white/10 outline-none focus:border-primary/50 transition-all shadow-inner"
                  placeholder={t("Port")}
                  value={newNode.proxyPort || ""}
                  onChange={e => setNewNode({ ...newNode, proxyPort: parseInt(e.target.value) || 0 })}
                />
              </div>
            )}
            </>)}

            {/* ProxyJump — bounce through another saved node to reach this one
                (like `ssh -J bastion target`). Distinct from the SOCKS/HTTP
                proxy above: that's a plain TCP proxy, this is a full SSH hop
                that opens a direct-tcpip channel to the target. Self is filtered
                out so a node can't jump through itself. */}
            <div className="flex justify-between items-center gap-3">
              <label className="text-[11px] font-bold text-zinc-400 shrink-0">{isSsh ? t("Jump via") : t("Through SSH server")}</label>
              <select
                className="flex-1 min-w-0 h-9 bg-[#1a1a1e] rounded-lg px-3 text-[12px] text-zinc-300 border border-white/10 outline-none focus:border-primary/50 focus:bg-[#232328] transition-all shadow-inner"
                value={newNode.jumpHostId || ""}
                onChange={e => setNewNode({ ...newNode, jumpHostId: e.target.value })}
              >
                <option value="" className="bg-[#1a1a1e] text-zinc-500">{isSsh ? t("Direct — no jump host") : t("Direct — no SSH server")}</option>
                {(servers || [])
                  .filter((s: any) => s.id !== newNode.id && (s.protocol ?? "ssh") === "ssh")
                  .map((s: any) => (
                    <option key={s.id} value={s.id.toString()} className="bg-[#1a1a1e] text-zinc-300">
                      {s.name} ({s.host})
                    </option>
                  ))}
              </select>
            </div>

            {/* RDP options: this device's own (util/rdpOptions), not synced. */}
            {isRdp && (() => {
              const o: RdpOptions = newNode.rdpOptions || DEFAULT_RDP_OPTIONS;
              const set = (patch: Partial<RdpOptions>) => setNewNode({ ...newNode, rdpOptions: { ...o, ...patch } });
              const size = `${o.width}x${o.height}`;
              const known = RDP_WINDOW_SIZES.some(([w, h]) => w === o.width && h === o.height);
              const builtin = IS_MOBILE || o.app === "builtin";
              return (
                <div className="space-y-3 pt-1">
                  {/* A phone has only the built-in client (offered in the tab). */}
                  {!IS_MOBILE && (
                    <div className="flex justify-between items-center gap-3">
                      <label className="text-[11px] font-bold text-zinc-400 shrink-0">{t("Open in")}</label>
                      <select
                        aria-label={t("Open in")}
                        className="flex-1 min-w-0 h-9 bg-[#1a1a1e] rounded-lg px-3 text-[12px] text-zinc-300 border border-white/10 outline-none focus:border-primary/50 focus:bg-[#232328] transition-all shadow-inner"
                        value={o.app}
                        onChange={(e) => set({ app: e.target.value === "builtin" ? "builtin" : "system" })}
                      >
                        <option value="system" className="bg-[#1a1a1e]">{t("This computer's RDP app")}</option>
                        <option value="builtin" className="bg-[#1a1a1e]">{t("Submarine's tab (experimental)")}</option>
                      </select>
                    </div>
                  )}
                  {builtin ? (<>
                    <div role="note" className="flex gap-2 p-2.5 rounded-lg border border-amber-500/25 bg-amber-500/5 text-[10.5px] leading-snug text-amber-200/90">
                      <FlaskConical size={13} className="shrink-0 mt-px text-amber-300" aria-hidden="true" />
                      <span>
                        {t(RDP_BUILTIN_EXPERIMENTAL)} {t("The desktop opens in the server's tab, sized to it; the saved password is used, or asked for there.")}
                      </span>
                    </div>
                    <div className="flex items-center justify-between gap-3">
                      <div className="min-w-0">
                        <div className="text-[11px] font-bold text-zinc-300">{t("Allow servers without NLA")}</div>
                        <div className="text-[10.5px] text-zinc-500 leading-snug">
                          {t("NLA makes the server prove it knows the password before it is sent. Off, a server without it is refused and nothing is sent; on, the password goes inside TLS only.")}
                        </div>
                      </div>
                      <button
                        type="button"
                        onClick={() => set({ allowNoNla: !o.allowNoNla })}
                        role="switch"
                        aria-label={t("Allow servers without NLA")}
                        aria-checked={!!o.allowNoNla}
                        className={`relative shrink-0 w-11 h-6 rounded-full transition-colors ${o.allowNoNla ? "bg-primary" : "bg-white/10"}`}
                      >
                        <span className={`absolute top-0.5 left-0.5 w-5 h-5 bg-zinc-100 rounded-full shadow transition-transform ${o.allowNoNla ? "translate-x-5" : "translate-x-0"}`} />
                      </button>
                    </div>
                    <div className="flex items-center justify-between gap-3">
                      <div className="min-w-0">
                        <div className="text-[11px] font-bold text-zinc-300">{t("Share the clipboard")}</div>
                        <div className="text-[10.5px] text-zinc-500 leading-snug">
                          {t("Text you copy on the server comes to this device, by itself while the tab is in front. This device's text goes to the server only when you paste it there (a paste key in the tab, or its Paste button).")}
                        </div>
                      </div>
                      <button
                        type="button"
                        onClick={() => set({ clipboard: !o.clipboard })}
                        role="switch"
                        aria-label={t("Share the clipboard")}
                        aria-checked={o.clipboard}
                        className={`relative shrink-0 w-11 h-6 rounded-full transition-colors ${o.clipboard ? "bg-primary" : "bg-white/10"}`}
                      >
                        <span className={`absolute top-0.5 left-0.5 w-5 h-5 bg-zinc-100 rounded-full shadow transition-transform ${o.clipboard ? "translate-x-5" : "translate-x-0"}`} />
                      </button>
                    </div>
                    {/* Files: a folder or a whole drive of this computer (rdp_share.rs). */}
                    {!IS_MOBILE && <RdpShareFields o={o} set={set} drives={localDrives} />}
                    <div className="pt-1 space-y-2.5">
                      <div className="text-[11px] font-bold text-zinc-300">{t("Speed and quality")}</div>
                      <RdpSpeedFields o={o} set={set} />
                    </div>
                  </>) : (<>
                  <div className="flex justify-between items-center gap-3">
                    <label className="text-[11px] font-bold text-zinc-400 shrink-0">{t("Window")}</label>
                    <select
                      aria-label={t("RDP window")}
                      className="flex-1 min-w-0 h-9 bg-[#1a1a1e] rounded-lg px-3 text-[12px] text-zinc-300 border border-white/10 outline-none focus:border-primary/50 focus:bg-[#232328] transition-all shadow-inner"
                      value={o.fullScreen ? "full" : size}
                      onChange={(e) => {
                        const v = e.target.value;
                        if (v === "full") return set({ fullScreen: true });
                        const [width, height] = v.split("x").map(Number);
                        set({ fullScreen: false, width, height });
                      }}
                    >
                      <option value="full" className="bg-[#1a1a1e]">{t("Full screen")}</option>
                      {RDP_WINDOW_SIZES.map(([w, h]) => (
                        <option key={`${w}x${h}`} value={`${w}x${h}`} className="bg-[#1a1a1e]">{w} × {h}</option>
                      ))}
                      {!o.fullScreen && !known && <option value={size} className="bg-[#1a1a1e]">{o.width} × {o.height}</option>}
                    </select>
                  </div>
                  <div className="flex items-center justify-between gap-3">
                    <div className="min-w-0">
                      <div className="text-[11px] font-bold text-zinc-300">{t("Share the clipboard")}</div>
                      <div className="text-[10.5px] text-zinc-500 leading-snug">{t("Copy and paste between this computer and the server.")}</div>
                    </div>
                    <button
                      type="button"
                      onClick={() => set({ clipboard: !o.clipboard })}
                      role="switch"
                      aria-label={t("Share the clipboard")}
                      aria-checked={o.clipboard}
                      className={`relative shrink-0 w-11 h-6 rounded-full transition-colors ${o.clipboard ? "bg-primary" : "bg-white/10"}`}
                    >
                      <span className={`absolute top-0.5 left-0.5 w-5 h-5 bg-zinc-100 rounded-full shadow transition-transform ${o.clipboard ? "translate-x-5" : "translate-x-0"}`} />
                    </button>
                  </div>
                  <div className="flex items-center justify-between gap-3">
                    <div className="min-w-0">
                      <div className="text-[11px] font-bold text-zinc-300">{t("Share this computer's files")}</div>
                      <div className="text-[10.5px] text-zinc-500 leading-snug">{t("The server can read and change them (with Remmina: your home folder).")}</div>
                    </div>
                    <button
                      type="button"
                      onClick={() => set({ drives: !o.drives })}
                      role="switch"
                      aria-label={t("Share this computer's files")}
                      aria-checked={o.drives}
                      className={`relative shrink-0 w-11 h-6 rounded-full transition-colors ${o.drives ? "bg-primary" : "bg-white/10"}`}
                    >
                      <span className={`absolute top-0.5 left-0.5 w-5 h-5 bg-zinc-100 rounded-full shadow transition-transform ${o.drives ? "translate-x-5" : "translate-x-0"}`} />
                    </button>
                  </div>
                  </>)}
                  <p className="text-[10px] text-zinc-500 ml-1">{t("These are kept on this device only, not synced.")}</p>
                </div>
              );
            })()}

            {/* VNC speed and quality: this device's own (util/vncOptions), not synced. */}
            {isVnc && (() => {
              const o: VncOptions = newNode.vncOptions || DEFAULT_VNC_OPTIONS;
              const set = (patch: Partial<VncOptions>) => setNewNode({ ...newNode, vncOptions: normalizeVncOptions({ ...o, ...patch }) });
              return (
                <div className="space-y-2.5 pt-1">
                  <SwitchRow
                    label={t("Share the clipboard")}
                    hint={t("Text you copy on the server comes to this device, by itself while the tab is in front; this device's goes to the server when you paste it there (a paste key in the tab, or its Paste button). Plain ASCII text only, such as links and commands: a VNC clipboard can't carry other characters.")}
                    checked={o.clipboard}
                    onChange={(clipboard) => set({ clipboard })}
                  />
                  <div className="text-[11px] font-bold text-zinc-300">{t("Speed and quality")}</div>
                  <VncSpeedFields o={o} set={set} />
                  <p className="text-[10px] text-zinc-500 ml-1">{t("These are kept on this device only, not synced.")}</p>
                </div>
              );
            })()}

            {isSsh && (<>
            <div className="space-y-2.5">
              <div className="flex justify-between items-center">
                <span className="text-[11px] font-bold text-zinc-400">{t("Port forwarding")}</span>
                <button onClick={() => setNewNode({ ...newNode, tunnels: [...newNode.tunnels, { rid: `t-${Date.now()}-${Math.random().toString(36).slice(2,7)}`, local: "1080", remote: "", type: "D" }] })} className="text-[11px] font-bold text-primary hover:text-primary transition-colors">{t("+ Add")}</button>
              </div>

              <div className="space-y-2">
                {newNode.tunnels?.map((tun: any, idx: number) => (
                  // `rid` is a stable per-row id added on Add. Falling
                  // back to idx for legacy rows that lack it; new rows
                  // always carry rid so deletions / reorders preserve
                  // each input's focus and cursor state.
                  <div key={tun.rid ?? idx} className="p-2 bg-[#1a1a1e] rounded-lg border border-white/5 group transition-all hover:border-primary/30 space-y-1.5">
                    <div className="flex items-center gap-2">
                      <select
                        className="bg-transparent text-[12px] font-semibold outline-none text-primary cursor-pointer pl-1"
                        value={tun.type}
                        onChange={e => updateTunnel(idx, 'type', e.target.value)}
                      >
                        <option value="D" className="bg-[#1a1a1e] text-primary">{t("Dynamic")}</option>
                        <option value="L" className="bg-[#1a1a1e] text-primary">{t("Local")}</option>
                        <option value="R" className="bg-[#1a1a1e] text-primary">{t("Remote")}</option>
                      </select>

                      <div className="h-4 w-px bg-white/10 mx-1" />

                      <input
                        className="flex-1 min-w-0 bg-transparent text-[12px] font-mono outline-none text-zinc-300 placeholder:text-zinc-700"
                        placeholder={
                          tun.type === 'D' ? t("Port (1080)") :
                          tun.type === 'R' ? t("Server bind (8080 or 0.0.0.0:8080)") :
                                             t("Local (8080 or 0.0.0.0:8080)")
                        }
                        value={tun.local}
                        onChange={e => updateTunnel(idx, 'local', e.target.value)}
                      />

                      {tun.type !== 'D' && (
                        <>
                          <ArrowLeftRight size={10} className="text-zinc-700 shrink-0 mx-1" />
                          <input
                            className="flex-1 min-w-0 bg-transparent text-[12px] font-mono outline-none text-primary placeholder:text-zinc-700"
                            placeholder={t("Remote (80)")}
                            value={tun.remote}
                            onChange={e => updateTunnel(idx, 'remote', e.target.value)}
                          />
                        </>
                      )}

                      <button aria-label={t("Remove this forward")} onClick={() => setNewNode({ ...newNode, tunnels: newNode.tunnels.filter((_: any, i: number) => i !== idx) })} className="opacity-100 sm:opacity-0 sm:group-hover:opacity-100 sm:focus-visible:opacity-100 w-6 h-6 flex items-center justify-center text-red-500 hover:bg-red-500/10 rounded transition-all shrink-0">
                        <X size={12} />
                      </button>
                    </div>
                    {/* Its name, and whether it starts with the connection: a
                        forward stopped in the Ports panel stays here, stopped. */}
                    <div className="flex items-center gap-2 pl-1">
                      <input
                        className="flex-1 min-w-0 bg-transparent text-[11px] outline-none text-zinc-300 placeholder:text-zinc-700"
                        placeholder={t("Name (optional)")}
                        aria-label={t("Forward name")}
                        maxLength={64}
                        value={tun.name ?? ""}
                        onChange={e => updateTunnel(idx, 'name', e.target.value)}
                      />
                      <button
                        type="button"
                        onClick={() => updateTunnel(idx, 'stopped', !tun.stopped)}
                        title={tun.stopped
                          ? t("Kept, but not started on connect. Click to start it on connect.")
                          : t("Started on connect. Click to keep it stopped.")}
                        className={`h-5 px-1.5 rounded text-[10px] font-bold uppercase tracking-wider flex items-center gap-1 shrink-0 transition-colors ${
                          tun.stopped
                            ? "text-zinc-500 bg-white/5 hover:text-zinc-300"
                            : "text-emerald-400 bg-emerald-500/10 hover:bg-emerald-500/20"
                        }`}
                      >
                        {tun.stopped ? <Square size={9} /> : <Play size={9} fill="currentColor" />}
                        {tun.stopped ? t("Stopped") : t("On connect")}
                      </button>
                    </div>
                  </div>
                ))}
              </div>
            </div>

            {/* Folder mirrors saved on this node — one-way local → remote
                sync. Stored alongside tunnels so they roam with the vault.
                Live management (start / stop / log) lives in the Mirror
                panel inside an open session. */}
            <div className="space-y-2.5">
              <div className="flex justify-between items-center">
                <span className="text-[11px] font-bold text-zinc-400">{t("Folder mirrors")}</span>
                <button
                  type="button"
                  onClick={() => setNewNode({
                    ...newNode,
                    mirrors: [...(newNode.mirrors || []), { local: "", remote: "", soft_delete: true, excludes: [], conflict_resolution: "local" }],
                  })}
                  className="text-[11px] font-bold text-primary hover:text-primary transition-colors"
                >{t("+ Add")}</button>
              </div>
              <div className="space-y-2">
                {(newNode.mirrors || []).map((m: any, idx: number) => (
                  <div key={idx} className="p-2.5 bg-[#1a1a1e] rounded-lg border border-white/5 group space-y-1.5">
                    <div className="flex items-center gap-2">
                      <input
                        className="flex-1 min-w-0 h-7 bg-black/40 rounded px-2 text-[11.5px] font-mono text-zinc-200 border border-white/10 outline-none focus:border-primary/50"
                        placeholder="/local/path"
                        value={m.local}
                        onChange={(e) => {
                          const next = [...newNode.mirrors];
                          next[idx] = { ...next[idx], local: e.target.value };
                          setNewNode({ ...newNode, mirrors: next });
                        }}
                      />
                      <ArrowLeftRight size={10} className="text-zinc-700 shrink-0 mx-1" />
                      <input
                        className="flex-1 min-w-0 h-7 bg-black/40 rounded px-2 text-[11.5px] font-mono text-zinc-200 border border-white/10 outline-none focus:border-primary/50"
                        placeholder="/remote/path"
                        value={m.remote}
                        onChange={(e) => {
                          const next = [...newNode.mirrors];
                          next[idx] = { ...next[idx], remote: e.target.value };
                          setNewNode({ ...newNode, mirrors: next });
                        }}
                      />
                      <button aria-label={t("Remove this mirror")}
                        type="button"
                        onClick={() => setNewNode({ ...newNode, mirrors: newNode.mirrors.filter((_: any, i: number) => i !== idx) })}
                        className="opacity-100 sm:opacity-0 sm:group-hover:opacity-100 sm:focus-visible:opacity-100 w-6 h-6 flex items-center justify-center text-red-500 hover:bg-red-500/10 rounded transition-all shrink-0"
                      >
                        <X size={12} />
                      </button>
                    </div>
                    <div className="flex items-center gap-3 pl-1">
                      <label className="flex items-center gap-1.5 text-[10.5px] text-zinc-400 select-none cursor-pointer">
                        <input
                          type="checkbox"
                          checked={m.soft_delete !== false}
                          onChange={(e) => {
                            const next = [...newNode.mirrors];
                            next[idx] = { ...next[idx], soft_delete: e.target.checked };
                            setNewNode({ ...newNode, mirrors: next });
                          }}
                          className="accent-primary"
                        />
                        <span>{t("Soft delete")}</span>
                      </label>
                      <select
                        value={m.conflict_resolution || "local"}
                        onChange={(e) => {
                          const next = [...newNode.mirrors];
                          next[idx] = { ...next[idx], conflict_resolution: e.target.value };
                          setNewNode({ ...newNode, mirrors: next });
                        }}
                        title={t("Initial-sync conflict resolution: which side overwrites when a file exists on both with different content. Files missing on one side always copy across.")}
                        className="h-6 px-1.5 bg-black/30 border border-white/10 rounded text-[10.5px] text-zinc-300 outline-none focus:border-primary/40"
                      >
                        <option value="local">{t("Local wins")}</option>
                        <option value="remote">{t("Remote wins")}</option>
                        <option value="newer">{t("Newer wins")}</option>
                      </select>
                      <input
                        className="flex-1 min-w-0 h-6 bg-transparent rounded px-1 text-[10.5px] font-mono text-zinc-400 placeholder:text-zinc-700 outline-none"
                        placeholder={t("Excludes: .git, node_modules, *.swp")}
                        value={(m.excludes || []).join(", ")}
                        onChange={(e) => {
                          const parts = e.target.value.split(",").map((s) => s.trim()).filter(Boolean);
                          const next = [...newNode.mirrors];
                          next[idx] = { ...next[idx], excludes: parts };
                          setNewNode({ ...newNode, mirrors: next });
                        }}
                      />
                    </div>
                  </div>
                ))}
              </div>
            </div>

            {/* Autostart toggle — when on, the node opens a session and
                connects automatically right after the user unlocks the
                profile. Useful for the one or two servers you always have
                a shell on. */}
            <div className="flex items-center justify-between gap-3 pt-1">
              <div className="min-w-0">
                <div className="text-[11px] font-bold text-zinc-300">{t("Autostart")}</div>
                <div className="text-[10.5px] text-zinc-500 leading-snug">
                  {t("Open + connect this node automatically when the app launches.")}
                </div>
              </div>
              <button
                type="button"
                onClick={() => setNewNode({ ...newNode, autostart: !newNode.autostart })}
                role="switch"
                aria-label={t("Autostart")}
                aria-checked={!!newNode.autostart}
                className={`relative shrink-0 w-11 h-6 rounded-full transition-colors ${
                  newNode.autostart ? "bg-primary" : "bg-white/10"
                }`}
              >
                <span
                  className={`absolute top-0.5 left-0.5 w-5 h-5 bg-zinc-100 rounded-full shadow transition-transform ${
                    newNode.autostart ? "translate-x-5" : "translate-x-0"
                  }`}
                />
              </button>
            </div>
            </>)}

            {/* This device's own choice for this server, see util/sessionKeep. */}
            {isSsh && (
              <div className="space-y-2 pt-1">
                {([
                  ["keepalives", t("Send keepalives")],
                  ["reconnect", t("Reconnect automatically")],
                ] as const).map(([field, label]) => (
                  <div key={field} className="flex items-center justify-between gap-3">
                    <label className="text-[11px] font-bold text-zinc-400 ml-1">{label}</label>
                    <select
                      aria-label={label}
                      className="h-8 bg-[#1a1a1e] rounded-lg px-2 text-[12px] text-zinc-300 border border-white/10 outline-none focus:border-primary/50"
                      value={(newNode.sessionKeep || DEFAULT_SESSION_KEEP)[field]}
                      onChange={(e) => setNewNode({ ...newNode, sessionKeep: { ...(newNode.sessionKeep || DEFAULT_SESSION_KEEP), [field]: e.target.value } })}
                    >
                      <option value="default" className="bg-[#1a1a1e]">{t("As in Settings")}</option>
                      <option value="on" className="bg-[#1a1a1e]">{t("On")}</option>
                      <option value="off" className="bg-[#1a1a1e]">{t("Off")}</option>
                    </select>
                  </div>
                ))}
                <p className="text-[10px] text-zinc-500 ml-1">{t("Kept on this device only. A keepalive change applies the next time a session connects.")}</p>
              </div>
            )}

            {/* Per-node notes — free-form, plain text. Lives on the server
                row (servers.notes column) and never crosses the network. The
                primary use case is small runbooks: which user owns root,
                where the deploy script lives, contact for the box, etc. */}
            <div className="space-y-1.5 pt-1">
              <label className="text-[11px] font-bold text-zinc-400 ml-1 flex items-center gap-2">
                <span>{t("Notes")}</span>
                <span className="text-[10px] text-zinc-600 font-normal normal-case tracking-normal">
                  {t("Description, runbook, contacts — only for this server.")}
                </span>
              </label>
              <textarea
                value={newNode.notes || ""}
                onChange={(e) => setNewNode({ ...newNode, notes: e.target.value })}
                placeholder={t("e.g. Prod web. Reboot windows Sun 04:00 UTC. Deploy via `make ship`. Pager: @ops")}
                rows={6}
                className="w-full bg-[#1a1a1e] rounded-lg p-3 text-[12px] text-white border border-white/10 outline-none focus:border-primary/50 focus:bg-[#232328] transition-all shadow-inner font-mono leading-relaxed resize-y"
              />
              {/* Attribution — who last changed this node and when. Populated by
                  the sync layer; most useful on shared / multi-device profiles. */}
              {isEditMode && (newNode.updatedAt || newNode.editedBy) && (() => {
                const when = hlcTime(newNode.updatedAt);
                return (
                  <div className="flex items-center gap-1.5 text-[11px] text-zinc-500 ml-1 pt-0.5">
                    <History size={11} className="text-zinc-600 shrink-0" />
                    <span>
                      {(() => {
                        const who = newNode.editedBy ? <span className="text-zinc-300 font-medium">{newNode.editedBy}</span> : null;
                        if (when && who) return tx("Last edited {when} by {who}", { when: relTime(when), who });
                        if (when) return t("Last edited {when}", { when: relTime(when) });
                        if (who) return tx("Last edited by {who}", { who });
                        return t("Last edited");
                      })()}
                      {when && <span className="text-zinc-600" title={when.toLocaleString()}> · {when.toLocaleDateString()}</span>}
                    </span>
                  </div>
                );
              })()}
            </div>

            {/* On-connect commands — auto-typed into the FIRST terminal the
                first time you connect to this server (not on reconnects).
                With the box ticked, also once into each terminal opened
                later (#256). One command per line. */}
            {isSsh && (
            <div className="space-y-1.5 pt-1">
              <label className="text-[11px] font-bold text-zinc-400 ml-1 flex items-center gap-2">
                <span>{t("Run on connect")}</span>
                <span className="text-[10px] text-zinc-600 font-normal normal-case tracking-normal">
                  {t("Commands auto-run once on the first terminal. One per line.")}
                </span>
              </label>
              <textarea
                value={newNode.runOnConnect || ""}
                onChange={(e) => setNewNode({ ...newNode, runOnConnect: e.target.value })}
                placeholder={t("e.g.\ncd /var/www\nsource .venv/bin/activate\ntail -f storage/logs/app.log")}
                rows={4}
                spellCheck={false}
                className="w-full bg-[#1a1a1e] rounded-lg p-3 text-[12px] text-white border border-white/10 outline-none focus:border-primary/50 focus:bg-[#232328] transition-all shadow-inner font-mono leading-relaxed resize-y"
              />
              <label className="flex items-center gap-1.5 ml-1 text-[10.5px] text-zinc-400 select-none cursor-pointer">
                <input
                  type="checkbox"
                  checked={!!newNode.runOnConnectEvery}
                  onChange={(e) => setNewNode({ ...newNode, runOnConnectEvery: e.target.checked })}
                  className="accent-primary"
                />
                <span>{t("Also run them in each new terminal you open")}</span>
              </label>
            </div>
            )}
          </div>
        </div>

        {/* Footer */}
        <div className="p-6 border-t border-white/5 shrink-0">
          <button onClick={save} disabled={saving} className="w-full h-10 bg-primary text-black font-bold rounded-lg text-[13px] tracking-tight hover:bg-primary hover:shadow-[0_0_20px_rgba(var(--primary),0.3)] transition-all active:scale-[0.98] disabled:opacity-60 flex items-center justify-center gap-2">
            <Cpu size={14} /> {isEditMode ? t("Save changes") : t("Save server")}
          </button>
        </div>
      </div>

      {/* SSH-config import modal. Portaled to <body> so it overlays the
          Add Server panel cleanly and isn't clipped by the panel's flex
          layout. Click-outside on the backdrop dismisses; the panel itself
          stops propagation so clicks inside don't close it. */}
      {importOpen && createPortal(
        <div
          className="fixed inset-0 z-[10000] bg-black/70 backdrop-blur-sm flex items-center justify-center p-4"
          onClick={closeImportModal}
        >
          <div
            role="dialog"
            aria-modal="true"
            aria-labelledby="ssh-import-title"
            onClick={(e) => e.stopPropagation()}
            className="w-full max-w-[520px] max-h-[80vh] flex flex-col bg-[#121214] border border-white/10 rounded-xl shadow-2xl animate-in zoom-in-95 fade-in duration-150"
          >
            {/* Modal header */}
            <div className="flex items-center justify-between p-4 border-b border-white/5 shrink-0">
              <div className="flex items-center gap-2">
                <Download size={14} className="text-primary" />
                <h3 id="ssh-import-title" className="text-[12px] font-semibold text-zinc-100">
                  {t("Import SSH hosts")}
                </h3>
              </div>
              <button aria-label={t("Close")}
                onClick={closeImportModal}
                disabled={importBusy}
                className="w-7 h-7 flex items-center justify-center text-zinc-500 hover:text-white bg-white/[0.04] border border-white/10 hover:bg-white/10 rounded-lg transition-all disabled:opacity-40 disabled:cursor-not-allowed"
              >
                <X size={12} />
              </button>
            </div>

            {/* Source selector — two ways to feed hosts in:
                    ssh    → parse the local OpenSSH config (desktop only)
                    paste  → drop text from PuTTY, MobaXterm, or a JSON
                             export; Rust auto-detects the format so the
                             user doesn't need to say which client it came
                             from. */}
            <div className="px-4 py-2 border-b border-white/5 flex items-center gap-1 shrink-0">
              <button
                onClick={() => switchImportSource("ssh")}
                disabled={importBusy}
                className={`h-7 px-3 rounded-lg text-[11.5px] font-semibold transition-all ${
                  importSource === "ssh"
                    ? "bg-primary/10 text-primary border border-primary/30"
                    : "text-zinc-400 border border-white/5 hover:text-white hover:bg-white/[0.04]"
                }`}
              >
                {t("From ~/.ssh/config")}
              </button>
              {!IS_MOBILE && (
                <button
                  onClick={() => switchImportSource("filezilla")}
                  disabled={importBusy}
                  className={`h-7 px-3 rounded-lg text-[11.5px] font-semibold transition-all ${
                    importSource === "filezilla"
                      ? "bg-primary/10 text-primary border border-primary/30"
                      : "text-zinc-400 border border-white/5 hover:text-white hover:bg-white/[0.04]"
                  }`}
                >
                  {t("From FileZilla")}
                </button>
              )}
              <button
                onClick={() => switchImportSource("paste")}
                disabled={importBusy}
                className={`h-7 px-3 rounded-lg text-[11.5px] font-semibold transition-all ${
                  importSource === "paste"
                    ? "bg-primary/10 text-primary border border-primary/30"
                    : "text-zinc-400 border border-white/5 hover:text-white hover:bg-white/[0.04]"
                }`}
              >
                {t("Paste PuTTY / MobaXterm / JSON")}
              </button>
            </div>

            {/* Body — one of loading / error / empty / list. */}
            <div className="flex-1 overflow-y-auto custom-scrollbar p-3">
              {importSource === "paste" && importHosts.length === 0 && !importLoading && (
                <div className="space-y-2 mb-2">
                  <div className="text-[11px] text-zinc-400 leading-relaxed px-1">
                    {t("Paste an export from another SSH client. Recognised sources:")}
                    <ul className="mt-1.5 ml-4 list-disc space-y-0.5 text-zinc-500 text-[10.5px]">
                      <li>{tx("{putty} — export via {command}, open the .reg file, paste its contents.", {
                        putty: <b className="text-zinc-300">PuTTY</b>,
                        command: <code className="text-zinc-300">regedit /e ... HKCU\Software\SimonTatham\PuTTY\Sessions</code>,
                      })}</li>
                      <li>{tx("{mobaxterm} — Export sessions → open {file}, paste.", {
                        mobaxterm: <b className="text-zinc-300">MobaXterm</b>,
                        file: <code className="text-zinc-300">.mxtsessions</code>,
                      })}</li>
                      <li>{tx("{filezilla} — File → Export in FileZilla (or its {file}): open it, paste.", {
                        filezilla: <b className="text-zinc-300">FileZilla</b>,
                        file: <code className="text-zinc-300">sitemanager.xml</code>,
                      })}</li>
                      <li>{tx("{json} — an array of {shape} or Termius-style {termius}.", {
                        json: <b className="text-zinc-300">JSON</b>,
                        shape: <code className="text-zinc-300">{`{name,host,port,user}`}</code>,
                        termius: <code className="text-zinc-300">{`{label,address,port,username}`}</code>,
                      })}</li>
                    </ul>
                  </div>
                  <textarea
                    value={importPasteText}
                    onChange={(e) => setImportPasteText(e.target.value)}
                    placeholder={t(`Paste your export here…\n\nExample JSON:\n[\n  {"name":"prod-web","host":"1.2.3.4","port":22,"user":"root"},\n  {"name":"db","host":"10.0.0.5","user":"admin"}\n]`)}
                    rows={10}
                    className="w-full bg-black/40 border border-white/10 rounded-lg p-3 text-[11px] font-mono text-zinc-200 placeholder:text-zinc-600 focus:border-primary/40 focus:outline-none resize-none custom-scrollbar"
                  />
                  <div className="flex justify-end">
                    <button
                      onClick={parsePastedImport}
                      disabled={!importPasteText.trim()}
                      className="h-8 px-3 rounded-lg text-[12px] font-semibold bg-primary/10 text-primary border border-primary/30 hover:bg-primary/20 disabled:opacity-40 disabled:cursor-not-allowed transition-all"
                    >
                      {t("Parse pasted text")}
                    </button>
                  </div>
                </div>
              )}

              {importLoading && (
                <div className="p-6 text-center text-[12px] text-zinc-400">
                  {importSource === "ssh" ? t("Reading ~/.ssh/config…") : importSource === "filezilla" ? t("Reading FileZilla's site list…") : t("Parsing pasted text…")}
                </div>
              )}

              {!importLoading && importError && importHosts.length === 0 && (
                <div className="m-2 p-3 bg-red-500/10 border border-red-500/20 rounded-lg text-red-400 text-[12px] font-mono whitespace-pre-wrap break-words">
                  {importError}
                </div>
              )}

              {!importLoading && !importError && importSource === "ssh" && importHosts.length === 0 && (
                <div className="p-6 text-center text-[12px] text-zinc-500">
                  {t("No importable hosts found in ~/.ssh/config.")}
                </div>
              )}

              {!importLoading && importHosts.length > 0 && (
                <>
                  {/* Sticky select-all row so users can bulk-toggle a long
                      list without scrolling back up. */}
                  <div className="sticky top-0 z-10 -mx-3 -mt-3 mb-2 px-4 py-2 bg-[#121214] border-b border-white/5 flex items-center justify-between">
                    <button
                      onClick={toggleSelectAll}
                      className="flex items-center gap-2 text-[11px] font-bold text-zinc-300 hover:text-white transition-colors"
                    >
                      {importSelected.size === importHosts.length
                        ? <CheckSquare size={13} className="text-primary" />
                        : <Square size={13} className="text-zinc-500" />}
                      <span>{t("Select all ({count})", { count: importHosts.length })}</span>
                    </button>
                    <span className="text-[11px] text-zinc-500 font-mono">
                      {t("{count} selected", { count: importSelected.size })}
                    </span>
                  </div>

                  <div className="space-y-1.5">
                    {importHosts.map((h) => {
                      const isSel = importSelected.has(h.host_alias);
                      return (
                        <label
                          key={h.host_alias}
                          className={`flex items-start gap-3 p-2.5 rounded-lg border cursor-pointer transition-all ${
                            isSel
                              ? "bg-primary/10 border-primary/40"
                              : "bg-[#1a1a1e] border-white/5 hover:border-white/20"
                          }`}
                        >
                          <input
                            type="checkbox"
                            checked={isSel}
                            onChange={() => toggleOne(h.host_alias)}
                            className="mt-0.5 accent-primary shrink-0"
                          />
                          <div className="flex-1 min-w-0">
                            <div className="flex items-center gap-1.5 min-w-0">
                              <span className="text-[12.5px] font-bold text-white truncate">{h.host_alias}</span>
                              {h.protocol && (
                                <span className={`shrink-0 px-1 rounded text-[9px] font-bold tracking-wider leading-[14px] border ${h.protocol === "ftp" ? "text-amber-300 bg-amber-500/10 border-amber-500/20" : "text-teal-300 bg-teal-500/10 border-teal-500/20"}`}>
                                  {importKind(h)}
                                </span>
                              )}
                            </div>
                            <div className="text-[11px] font-mono text-zinc-400 truncate">
                              {h.user ? `${h.user}@` : ""}{h.hostname}:{h.port}
                              {h.password && <span className="ml-2 font-sans text-zinc-500">{t("with password")}</span>}
                            </div>
                            {(h.identity_file || h.proxy_jump) && (
                              <div className="mt-0.5 text-[10.5px] text-zinc-600 truncate">
                                {h.identity_file && <span className="mr-2">{t("key: {file}", { file: h.identity_file })}</span>}
                                {h.proxy_jump && <span>{t("jump: {host}", { host: h.proxy_jump })}</span>}
                              </div>
                            )}
                          </div>
                        </label>
                      );
                    })}
                  </div>

                  {importError && importHosts.length > 0 && (
                    <div className="mt-3 p-2 bg-red-500/10 border border-red-500/20 rounded-lg text-red-400 text-[11px] font-mono">
                      {importError}
                    </div>
                  )}

                  {importNotice && (
                    <div className="mt-3 p-2 bg-amber-500/10 border border-amber-500/20 rounded-lg text-amber-300 text-[11px]">
                      {importNotice}
                    </div>
                  )}
                </>
              )}
            </div>

            {/* Modal footer */}
            <div className="p-3 border-t border-white/5 shrink-0 flex items-center justify-end gap-2">
              <button
                onClick={closeImportModal}
                disabled={importBusy}
                className="px-3 h-8 rounded-lg text-[12px] font-semibold bg-white/[0.04] border border-white/10 text-zinc-300 hover:bg-white/[0.08] hover:text-white disabled:opacity-40 disabled:cursor-not-allowed"
              >
                {t("Cancel")}
              </button>
              <button
                onClick={importSelectedHosts}
                disabled={importBusy || importSelected.size === 0 || importLoading}
                className="px-3 h-8 rounded-lg text-[12px] font-semibold bg-primary/15 border border-primary/40 text-primary hover:bg-primary/25 disabled:opacity-40 disabled:cursor-not-allowed flex items-center gap-1.5"
              >
                <Download size={11} />
                {importBusy ? t("Importing…") : t("Import {count} selected", { count: importSelected.size })}
              </button>
            </div>
          </div>
        </div>,
        document.body
      )}
    </div>
  );
};

export default AddNodePanel;