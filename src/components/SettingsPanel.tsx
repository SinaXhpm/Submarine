import { useEffect, useMemo, useRef, useState } from "react";
import { Settings, Palette, RefreshCw, List, Cloud, Minus, Plus, ArrowRight, X, KeyRound, EyeOff } from "lucide-react";
import {
  MAX_FONT_SIZE, MIN_FONT_SIZE, clampFontSize, fontFamilyCss,
  isFontAvailable, primaryFontName, sanitizeFontFamily,
} from "../util/terminalFont";
import {
  ShellSyncPair, localPathProblem, readShellSyncPairs, readShellSyncServers, remotePathProblem,
  setShellSyncServer, writeShellSyncPairs,
} from "../util/shellSync";
import { invoke } from "@tauri-apps/api/core";
import CopyValue from "./CopyValue";
import FontPicker from "./FontPicker";
import { loadInstalledFonts } from "../util/systemFonts";
import { IS_ANDROID } from "../util/platform";

// Where this install keeps its data (see src-tauri/src/portable.rs). `portable`
// means a `submarine-data` folder next to the executable is in use; `warning`
// is set when such a folder exists but couldn't be written to.
interface StorageInfo {
  portable: boolean;
  data_dir: string;
  warning: string | null;
}

// Slider covers the everyday range; the number field takes anything 1–99.
const SLIDER_MAX = 40;

// "Use my default shell" (open_terminal in lib.rs): offered only where this
// computer has a shell of its own to start, which the backend knows
// (`local_shell`: none on Windows and Android, or without $SHELL).
function DefaultShellSetting({ settings, setSettings }: any) {
  const [shell, setShell] = useState<string | null>(null);
  useEffect(() => {
    invoke<string | null>("local_shell").then(setShell, () => setShell(null));
  }, []);
  if (!shell) return null;
  const name = shell.split("/").pop();
  return (
    <div className="bg-[#121215] border border-white/5 rounded-2xl p-6 shadow-xl">
      <div className="flex items-center justify-between gap-4">
        <div className="min-w-0">
          <div className="text-sm font-semibold text-zinc-100">Use my default shell</div>
          <p className="text-[12px] text-zinc-400 leading-relaxed mt-1">
            New terminals start {name}, the shell this computer uses, as a login shell on the
            server in place of the account's own. {name} reads the server's own startup files;
            nothing is copied from here. The server's welcome message isn't shown in these
            terminals. A server without {name}, or one that can't start it this way (a Windows
            server, some network devices), opens the account's own shell as before.
          </p>
        </div>
        <button
          role="switch"
          aria-checked={!!settings.useLocalShell}
          aria-label="Use my default shell"
          onClick={() => setSettings((s: any) => ({ ...s, useLocalShell: !s.useLocalShell }))}
          className={`relative shrink-0 w-11 h-6 rounded-full transition-colors ${settings.useLocalShell ? 'bg-primary' : 'bg-zinc-700'}`}
        >
          <span className={`absolute top-0.5 h-5 w-5 rounded-full bg-white transition-all ${settings.useLocalShell ? 'left-[22px]' : 'left-0.5'}`} />
        </button>
      </div>
    </div>
  );
}

// "Use keys from an ssh-agent" (util/sshAgent, src-tauri/src/agent_auth.rs).
function SshAgentSetting({ settings, setSettings }: any) {
  const on = settings.useSshAgent !== false;
  return (
    <div className="bg-[#121215] border border-white/5 rounded-2xl p-6 shadow-xl">
      <div className="flex items-center justify-between gap-4">
        <div className="min-w-0">
          <div className="text-sm font-semibold text-zinc-100">Use keys from an ssh-agent</div>
          <p className="text-[12px] text-zinc-400 leading-relaxed mt-1">
            Log in with keys held by an ssh-agent on this computer: Pageant, the Windows OpenSSH
            agent and agents that use its pipe (such as 1Password), the agent in SSH_AUTH_SOCK, or
            the one IdentityAgent names in ~/.ssh/config.
            Smartcards and hardware keys work through it. It's used only when a server has no saved
            password or key, its key's passphrase isn't saved, or the server turns its key down.
            Turn off to use only what's saved for each server.
          </p>
        </div>
        <button
          role="switch"
          aria-checked={on}
          aria-label="Use keys from an ssh-agent"
          onClick={() => setSettings((s: any) => ({ ...s, useSshAgent: !(s.useSshAgent !== false) }))}
          className={`relative shrink-0 w-11 h-6 rounded-full transition-colors ${on ? 'bg-primary' : 'bg-zinc-700'}`}
        >
          <span className={`absolute top-0.5 h-5 w-5 rounded-full bg-white transition-all ${on ? 'left-[22px]' : 'left-0.5'}`} />
        </button>
      </div>
    </div>
  );
}

// "Block screenshots" (Android only): FLAG_SECURE on the app's window. Kept
// on the device by the app's Android plugin rather than in these settings, so
// it applies from the first frame when the app starts (android_bridge.rs).
function SecureScreenSetting() {
  const [on, setOn] = useState<boolean | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    invoke<boolean>("get_secure_screen").then(setOn, (e) => setError(String(e)));
  }, []);
  const toggle = () => {
    if (on === null) return;
    setError(null);
    invoke<boolean>("set_secure_screen", { enabled: !on }).then(setOn, (e) => setError(String(e)));
  };
  return (
    <div className="bg-[#121215] border border-white/5 rounded-2xl p-6 shadow-xl">
      <div className="flex items-center justify-between gap-4">
        <div className="min-w-0">
          <div className="text-sm font-semibold text-zinc-100">Block screenshots</div>
          <p className="text-[12px] text-zinc-400 leading-relaxed mt-1">
            Keep Submarine out of screenshots, screen recordings and screen sharing, so passwords and
            terminal output can't be captured, not even by an app that records the screen. You
            can't take screenshots of it yourself while this is on. The preview in the recent
            apps list is always hidden (Android 13 and later).
          </p>
          {error && <p className="text-[12px] text-rose-300 mt-2 break-words">{error}</p>}
        </div>
        <button
          role="switch"
          aria-checked={!!on}
          aria-label="Block screenshots"
          disabled={on === null}
          onClick={toggle}
          className={`relative shrink-0 w-11 h-6 rounded-full transition-colors disabled:opacity-50 ${on ? 'bg-primary' : 'bg-zinc-700'}`}
        >
          <span className={`absolute top-0.5 h-5 w-5 rounded-full bg-white transition-all ${on ? 'left-[22px]' : 'left-0.5'}`} />
        </button>
      </div>
    </div>
  );
}

// Terminal font face + size. Both are drafts while you type: the size field
// can sit empty mid-edit (it used to snap back to 14 the moment it was
// cleared, which made small sizes impossible to type on Android), and the
// face is applied ~0.5 s after you stop typing instead of re-measuring every
// open terminal on each keystroke.
function TerminalFontSettings({ settings, setSettings }: any) {
  const size = clampFontSize(settings.terminalFontSize);
  const family: string = settings.terminalFontFamily || "";

  const [sizeDraft, setSizeDraft] = useState(String(size));
  useEffect(() => setSizeDraft(String(size)), [size]);
  const setSize = (n: number) =>
    setSettings((s: any) => ({ ...s, terminalFontSize: clampFontSize(n) }));

  const [familyDraft, setFamilyDraft] = useState(family);
  useEffect(() => setFamilyDraft(family), [family]);
  const commitTimer = useRef<number | null>(null);
  useEffect(() => () => { if (commitTimer.current) window.clearTimeout(commitTimer.current); }, []);
  const commitFamily = (v: string) => {
    if (commitTimer.current) { window.clearTimeout(commitTimer.current); commitTimer.current = null; }
    const clean = sanitizeFontFamily(v);
    setSettings((s: any) => (s.terminalFontFamily === clean ? s : { ...s, terminalFontFamily: clean }));
  };
  const onFamilyChange = (v: string) => {
    setFamilyDraft(v);
    if (commitTimer.current) window.clearTimeout(commitTimer.current);
    commitTimer.current = window.setTimeout(() => commitFamily(v), 500);
  };

  // Families installed on this device: the list the picker shows. The canvas
  // probe in isFontAvailable can miss a font the webview does draw (seen on
  // macOS, #59), and a font picked under "Installed on this device" must not
  // then be called missing. No warning until the list is in, so it can't
  // flash on and off; if the list can't be read, the probe decides alone.
  const [listed, setListed] = useState<Set<string> | null>(null);
  useEffect(() => {
    let alive = true;
    loadInstalledFonts().then(
      (fonts) => { if (alive) setListed(new Set(fonts.map((f) => f.family.toLowerCase()))); },
      () => { if (alive) setListed(new Set()); },
    );
    return () => { alive = false; };
  }, []);

  const installed = useMemo(
    () => listed === null || listed.has(primaryFontName(familyDraft).toLowerCase()) || isFontAvailable(familyDraft),
    [familyDraft, listed],
  );
  const previewFamily = fontFamilyCss(familyDraft);

  return (
    <div className="bg-[#121215] border border-white/5 rounded-2xl p-6 space-y-6 shadow-xl">
      {/* Font face */}
      <div className="space-y-2">
        <div className="flex justify-between items-center gap-2">
          <label htmlFor="term-font-family" className="text-[11px] font-black text-zinc-500 uppercase tracking-wider">Font</label>
          {familyDraft && (
            <button
              type="button"
              onClick={() => { setFamilyDraft(""); commitFamily(""); }}
              className="text-[11px] font-semibold text-zinc-400 hover:text-white transition-colors"
            >
              Use default
            </button>
          )}
        </div>
        <FontPicker
          value={familyDraft}
          onChange={onFamilyChange}
          onCommit={commitFamily}
          placeholder="Default monospace — pick from the list or type a font name"
        />
        {!installed && (
          <p className="text-[11.5px] text-amber-400/90">
            "{primaryFontName(familyDraft)}" doesn't seem to be installed on this device, so the default font is used instead.
          </p>
        )}
        <p className="text-[11.5px] text-zinc-500 leading-relaxed">
          Built-in fonts ship with the app and work on every device. Any font installed on this device works too, including Nerd Fonts for starship / powerline prompts.
        </p>
      </div>

      {/* Font size */}
      <div className="space-y-3 pt-5 border-t border-white/5">
        <div className="flex justify-between items-center gap-3">
          <label htmlFor="term-font-size" className="text-[11px] font-black text-zinc-500 uppercase tracking-wider">Font Size (px)</label>
          <div className="flex items-center gap-1.5">
            <button
              type="button"
              aria-label="Smaller font"
              onClick={() => setSize(size - 1)}
              disabled={size <= MIN_FONT_SIZE}
              className="w-8 h-8 flex items-center justify-center bg-black border border-white/10 rounded-lg text-zinc-300 hover:text-white hover:border-primary/40 disabled:opacity-30 transition-colors"
            >
              <Minus size={14} />
            </button>
            <input
              id="term-font-size"
              type="text"
              inputMode="numeric"
              pattern="[0-9]*"
              value={sizeDraft}
              onChange={(e) => {
                const v = e.target.value.replace(/[^0-9]/g, "").slice(0, 2);
                setSizeDraft(v);
                const n = parseInt(v, 10);
                if (n >= MIN_FONT_SIZE) setSize(n);
              }}
              onBlur={() => setSizeDraft(String(size))}
              className="w-14 h-8 bg-black border border-white/10 rounded-lg px-2 text-[12px] font-bold text-white focus:border-primary/50 outline-none text-center"
            />
            <button
              type="button"
              aria-label="Larger font"
              onClick={() => setSize(size + 1)}
              disabled={size >= MAX_FONT_SIZE}
              className="w-8 h-8 flex items-center justify-center bg-black border border-white/10 rounded-lg text-zinc-300 hover:text-white hover:border-primary/40 disabled:opacity-30 transition-colors"
            >
              <Plus size={14} />
            </button>
          </div>
        </div>
        <input
          type="range"
          aria-label="Font size"
          min={MIN_FONT_SIZE}
          max={SLIDER_MAX}
          value={Math.min(size, SLIDER_MAX)}
          onChange={(e) => setSize(parseInt(e.target.value, 10))}
          className="w-full accent-primary"
        />
        <p className="text-[11.5px] text-zinc-500">Any size from {MIN_FONT_SIZE} to {MAX_FONT_SIZE} — type it in or use the − / + buttons.</p>
      </div>

      {/* Live preview */}
      <div className="rounded-xl bg-[#09090b] border border-white/5 px-3 py-2.5 overflow-x-auto custom-scrollbar">
        <pre
          className="m-0 whitespace-pre text-[#e4e4e7] leading-snug"
          style={{ fontFamily: previewFamily, fontSize: Math.min(size, 28) }}
        >
          <span className="text-primary">user@server</span>:~$ ls -la{"\n"}0O 1lI | {"{}"} [] =&gt; ✓
        </pre>
      </div>
    </div>
  );
}

// What the Shell button in a session's toolbar pushes, and which servers show
// it. Both are this device's own (see util/shellSync.ts): the files are this
// machine's, so neither is in the vault. Shell-agnostic on purpose: a plain
// local/remote list, for whatever shell the user runs.
function ShellConfigSyncSettings({ servers }: { servers: any[] }) {
  const [pairs, setPairs] = useState<ShellSyncPair[]>(() => readShellSyncPairs());
  const [chosen, setChosen] = useState<string[]>(() => readShellSyncServers());

  const persist = (next: ShellSyncPair[]) => {
    setPairs(next);
    writeShellSyncPairs(next);
  };
  const toggle = (uuid: string, on: boolean) => {
    setShellSyncServer(uuid, on);
    setChosen(readShellSyncServers());
  };
  // Saved servers (a quick connect has no sync id), by name.
  const choices = servers
    .filter((s) => typeof s.uuid === "string" && s.uuid)
    .sort((a, b) => String(a.name).localeCompare(String(b.name)));

  return (
    <div className="bg-[#121215] border border-white/5 rounded-2xl p-6 shadow-xl space-y-4">
      <div>
        <div className="text-sm font-semibold text-zinc-100">Shell config sync</div>
        <p className="text-[12px] text-zinc-400 leading-relaxed mt-1">
          Files and folders from this computer for the Shell button in a session's toolbar to push
          to that server: your shell config, theme, plugins. The place on the server is below its
          home folder, so one list works whatever the user name. You see what would change before
          anything is sent, and a file it replaces is kept in ~/.submarine-trash on the server.
          Keys, credentials, command history and .git folders are never pushed.
        </p>
      </div>

      <div className="space-y-2">
        {pairs.map((pair, i) => {
          const problem = localPathProblem(pair.local) ?? remotePathProblem(pair.remote);
          return (
            <div key={i} className="space-y-1">
              <div className="flex items-center gap-2">
                <input
                  type="text"
                  value={pair.local}
                  onChange={(e) => persist(pairs.map((p, idx) => idx === i ? { ...p, local: e.target.value } : p))}
                  placeholder="~/.zshrc"
                  aria-label="File or folder on this computer"
                  spellCheck={false}
                  className="flex-1 min-w-0 bg-black/30 border border-white/10 rounded-lg px-3 py-2 text-sm text-zinc-200 placeholder:text-zinc-600 focus:outline-none focus:border-primary/50"
                />
                <ArrowRight size={14} className="text-zinc-600 shrink-0" />
                <input
                  type="text"
                  value={pair.remote}
                  onChange={(e) => persist(pairs.map((p, idx) => idx === i ? { ...p, remote: e.target.value } : p))}
                  placeholder=".zshrc"
                  aria-label="Place on the server, below its home folder"
                  spellCheck={false}
                  className="flex-1 min-w-0 bg-black/30 border border-white/10 rounded-lg px-3 py-2 text-sm text-zinc-200 placeholder:text-zinc-600 focus:outline-none focus:border-primary/50"
                />
                <button
                  onClick={() => persist(pairs.filter((_, idx) => idx !== i))}
                  aria-label="Remove pair"
                  title="Remove pair"
                  className="shrink-0 p-2 rounded-lg text-zinc-500 hover:bg-white/10 hover:text-rose-400 transition-colors"
                >
                  <X size={14} />
                </button>
              </div>
              {problem && <p className="text-[11px] text-amber-300 pl-1">{problem}</p>}
            </div>
          );
        })}
        {pairs.length === 0 && (
          <p className="text-[12px] text-zinc-500 italic">Nothing to push yet.</p>
        )}
      </div>

      <button
        onClick={() => persist([...pairs, { local: "", remote: "" }])}
        className="flex items-center gap-1.5 text-xs font-semibold text-primary hover:text-primary/80 transition-colors"
      >
        <Plus size={14} /> Add pair
      </button>

      <div className="pt-4 border-t border-white/5">
        <div className="text-[12px] font-semibold text-zinc-200">Push to</div>
        <p className="text-[12px] text-zinc-500 mt-0.5">
          The servers that show the Shell button. Only this device keeps the choice.
        </p>
        {choices.length === 0 ? (
          <p className="text-[12px] text-zinc-500 italic mt-2">No saved servers yet.</p>
        ) : (
          <div className="mt-2 max-h-56 overflow-y-auto custom-scrollbar space-y-0.5 pr-1">
            {choices.map((s) => (
              <label key={s.uuid} className="flex items-center gap-2.5 px-2 py-1.5 rounded-lg hover:bg-white/[0.03] cursor-pointer">
                <input
                  type="checkbox"
                  checked={chosen.includes(s.uuid)}
                  onChange={(e) => toggle(s.uuid, e.target.checked)}
                  className="accent-primary w-4 h-4 shrink-0"
                />
                <span className="text-[13px] text-zinc-200 truncate">{s.name}</span>
                <span className="text-[11px] text-zinc-500 truncate">{s.host}</span>
              </label>
            ))}
          </div>
        )}
      </div>
    </div>
  );
}

const SettingsPanel = ({ settings, setSettings, onOpenLogs, servers = [] }: any) => {
  const accentColors = [
    { name: 'Light Blue', value: '#60a5fa' },
    { name: 'Sky', value: '#38bdf8' },
    { name: 'Cyan', value: '#22d3ee' },
    { name: 'Indigo', value: '#818cf8' },
    { name: 'Emerald', value: '#10b981' },
    { name: 'Rose', value: '#f43f5e' },
  ];

  const bgColors = [
    { name: 'Onyx', value: '#050505' },
    { name: 'Charcoal', value: '#0a0a0c' },
    { name: 'Slate', value: '#0f172a' },
    { name: 'Nord', value: '#2e3440' },
    { name: 'Dark Gray', value: '#1a1a1a' },
    { name: 'Deep Space', value: '#0d0d10' },
  ];

  // Read-only storage summary from the backend: where data lives and whether
  // a `submarine-data` folder next to the exe (portable mode) is in use.
  const [storage, setStorage] = useState<StorageInfo | null>(null);
  useEffect(() => {
    invoke<StorageInfo>("get_storage_info").then(setStorage).catch(() => setStorage(null));
  }, []);

  return (
    <div className="flex-1 p-4 sm:p-10 overflow-y-auto custom-scrollbar animate-in">
      <header className="mb-6 sm:mb-10">
        <h2 className="text-xl sm:text-2xl font-bold text-white tracking-tight flex items-center gap-3">
          <Settings size={24} className="text-primary" /> Settings
        </h2>
        <p className="text-[13px] text-zinc-400 mt-2">
          Tweak how Submarine looks and feels.
          {' '}
          <span className="text-zinc-500 italic">Per-device — preferences live in this machine's local storage and don't sync with your cloud profile.</span>
        </p>
      </header>

      {/* Columns rather than a grid: the sections are very different heights, and
          a 2-col grid rows them up so a short card leaves a tall empty gap beside
          it. Column flow packs them, and break-inside-avoid keeps a card whole. */}
      <div className="columns-1 md:columns-2 gap-4 sm:gap-8">
        {/* Appearance Section */}
        <section className="break-inside-avoid space-y-3 mb-4 sm:mb-8">
          <div className="flex items-center gap-2 text-zinc-400 font-bold uppercase tracking-widest text-xs">
            <Palette size={14} /> UI Customization
          </div>

          <div className="bg-[#121215] border border-white/5 rounded-2xl p-6 space-y-8 shadow-xl">
            {/* Accent Color */}
            <div className="space-y-4">
              <div className="flex justify-between items-center">
                <label className="text-[11px] font-black text-zinc-500 uppercase tracking-wider">Primary Accent Color</label>
                <div className="flex items-center gap-2">
                  <input 
                    type="color" 
                    value={settings.primaryColor} 
                    onChange={(e) => setSettings({ ...settings, primaryColor: e.target.value })}
                    className="w-6 h-6 rounded-md bg-transparent cursor-pointer border-none p-0"
                  />
                  <input 
                    type="text" 
                    value={settings.primaryColor} 
                    onChange={(e) => setSettings({ ...settings, primaryColor: e.target.value })}
                    className="w-20 h-6 bg-black border border-white/10 rounded px-1.5 text-[10px] font-mono text-zinc-400 focus:border-primary/50 outline-none"
                  />
                </div>
              </div>
              <div className="grid grid-cols-6 gap-3">
                {accentColors.map(c => (
                  <button
                    key={c.value}
                    onClick={() => setSettings({ ...settings, primaryColor: c.value })}
                    className={`w-full aspect-square rounded-xl border-2 transition-all ${settings.primaryColor === c.value ? 'border-white scale-110 shadow-lg' : 'border-transparent hover:scale-105'}`}
                    style={{ backgroundColor: c.value }}
                    title={c.name}
                  />
                ))}
              </div>
            </div>

            {/* Background Theme */}
            <div className="space-y-4 pt-6 border-t border-white/5">
              <div className="flex justify-between items-center">
                <label className="text-[11px] font-black text-zinc-500 uppercase tracking-wider">Background Theme</label>
                <div className="flex items-center gap-2">
                  <input 
                    type="color" 
                    value={settings.backgroundColor} 
                    onChange={(e) => setSettings({ ...settings, backgroundColor: e.target.value })}
                    className="w-6 h-6 rounded-md bg-transparent cursor-pointer border-none p-0"
                  />
                  <input 
                    type="text" 
                    value={settings.backgroundColor} 
                    onChange={(e) => setSettings({ ...settings, backgroundColor: e.target.value })}
                    className="w-20 h-6 bg-black border border-white/10 rounded px-1.5 text-[10px] font-mono text-zinc-400 focus:border-primary/50 outline-none"
                  />
                </div>
              </div>
              <div className="grid grid-cols-6 gap-3">
                {bgColors.map(c => (
                  <button
                    key={c.value}
                    onClick={() => setSettings({ ...settings, backgroundColor: c.value })}
                    className={`w-full aspect-square rounded-xl border-2 transition-all ${settings.backgroundColor === c.value ? 'border-white scale-110 shadow-lg' : 'border-transparent hover:scale-105'}`}
                    style={{ backgroundColor: c.value }}
                    title={c.name}
                  />
                ))}
              </div>
            </div>
          </div>
        </section>

        {/* Terminal Section */}
        <section className="break-inside-avoid space-y-3 mb-4 sm:mb-8">
          <div className="flex items-center gap-2 text-zinc-400 font-bold uppercase tracking-widest text-xs">
            <Settings size={14} /> Terminal Configuration
          </div>

          <TerminalFontSettings settings={settings} setSettings={setSettings} />

          <DefaultShellSetting settings={settings} setSettings={setSettings} />
          
          <ShellConfigSyncSettings servers={servers} />
        </section>

        {/* Not on Android, which has no ssh-agent to use. */}
        {!IS_ANDROID && (
          <section className="break-inside-avoid space-y-3 mb-4 sm:mb-8">
            <div className="flex items-center gap-2 text-zinc-400 font-bold uppercase tracking-widest text-xs">
              <KeyRound size={14} /> Authentication
            </div>

            <SshAgentSetting settings={settings} setSettings={setSettings} />
          </section>
        )}

        {IS_ANDROID && (
          <section className="break-inside-avoid space-y-3 mb-4 sm:mb-8">
            <div className="flex items-center gap-2 text-zinc-400 font-bold uppercase tracking-widest text-xs">
              <EyeOff size={14} /> Privacy
            </div>

            <SecureScreenSetting />
          </section>
        )}

        {/* Activity Section — moved out of the sidebar so the top-level
            navigation stays focused on primary workflows. Logs are diagnostic
            only; keeping them one click deep here cleans up the sidebar on
            mobile (six icons → five) without burying the data. */}
        {onOpenLogs && (
          <section className="break-inside-avoid space-y-3 mb-4 sm:mb-8">
            <div className="flex items-center gap-2 text-zinc-400 font-bold uppercase tracking-widest text-xs">
              <List size={14} /> Activity Log
            </div>

            <div className="bg-[#121215] border border-white/5 rounded-2xl p-6 space-y-4 shadow-xl">
              <p className="text-sm text-zinc-400 leading-relaxed">
                Diagnostic timeline of what the app has been doing this session. Useful for confirming a connection actually failed or watching a transfer's progress in retrospect.
              </p>
              <button
                onClick={onOpenLogs}
                className="px-4 h-9 bg-primary/10 border border-primary/30 text-primary rounded-xl text-xs font-bold uppercase hover:bg-primary hover:text-zinc-950 transition-all w-full flex items-center justify-center gap-2"
              >
                <List size={14} /> View Activity Log
              </button>
            </div>
          </section>
        )}

        {/* Cloud Section */}
        <section className="break-inside-avoid space-y-3 mb-4 sm:mb-8">
          <div className="flex items-center gap-2 text-zinc-400 font-bold uppercase tracking-widest text-xs">
            <Cloud size={14} /> Cloud Sync
          </div>

          <div className="bg-[#121215] border border-white/5 rounded-2xl p-6 space-y-4 shadow-xl">
            <div className="flex items-center justify-between gap-4">
              <div className="min-w-0">
                <div className="text-sm font-semibold text-zinc-100">Auto-sync</div>
                <p className="text-[12px] text-zinc-400 leading-relaxed mt-1">
                  Keep open profiles synced in the background — shortly after you make a change, when
                  you return to the window, and every few minutes to pull in others' edits on shared
                  profiles. Turn off to sync only with the manual button.
                </p>
              </div>
              <button
                role="switch"
                aria-label="Auto-sync"
                aria-checked={settings.autoSync !== false}
                onClick={() => setSettings((s: any) => ({ ...s, autoSync: !(s.autoSync !== false) }))}
                className={`relative shrink-0 w-11 h-6 rounded-full transition-colors ${settings.autoSync !== false ? 'bg-primary' : 'bg-zinc-700'}`}
              >
                <span className={`absolute top-0.5 h-5 w-5 rounded-full bg-white transition-all ${settings.autoSync !== false ? 'left-[22px]' : 'left-0.5'}`} />
              </button>
            </div>

            {settings.autoSync !== false && (
              <div className="border-t border-white/5 pt-4 text-[12px] text-zinc-400">
                Currently checking every{' '}
                <span className="text-zinc-100 font-semibold">{settings.syncIntervalMin ?? 5} min</span>. Change it
                on the Profile tab, next to Sync now.
              </div>
            )}
          </div>
        </section>

        {/* Maintenance Section */}
        <section className="break-inside-avoid space-y-3 mb-4 sm:mb-8">
          <div className="flex items-center gap-2 text-zinc-400 font-bold uppercase tracking-widest text-xs">
            <RefreshCw size={14} /> Maintenance
          </div>

          <div className="bg-[#121215] border border-white/5 rounded-2xl p-6 space-y-4 shadow-xl">
            {storage && (
              <div className="space-y-1.5 pb-4 border-b border-white/5">
                <div className="flex items-center gap-2">
                  <span className="text-[11px] font-black text-zinc-500 uppercase tracking-wider">Data folder</span>
                  {storage.portable && (
                    <span className="px-1.5 py-0.5 rounded-md text-[10px] font-bold uppercase tracking-wider text-primary bg-primary/10 border border-primary/30">
                      Portable
                    </span>
                  )}
                </div>
                <CopyValue value={storage.data_dir} className="max-w-full text-zinc-300">
                  <span className="font-mono text-[11.5px] break-all select-text">{storage.data_dir}</span>
                </CopyValue>
                {storage.warning && (
                  <p className="text-[11.5px] text-amber-400/90 leading-relaxed">{storage.warning}</p>
                )}
              </div>
            )}
            <p className="text-sm text-zinc-400 leading-relaxed">
              These preferences are persisted in your local environment. Resetting will revert all UI aesthetics to factory defaults.
            </p>
            <button
              onClick={() => {
                if(window.confirm('Reset all UI customizations?')) {
                  setSettings((s: any) => ({ ...s, primaryColor: '#60a5fa', backgroundColor: '#0a0a0c', terminalFontSize: 14, terminalFontFamily: '' }));
                }
              }}
              className="px-4 h-9 bg-zinc-900 border border-white/5 text-zinc-300 rounded-xl text-xs font-bold uppercase hover:bg-white/5 transition-all w-full"
            >
              Reset to Defaults
            </button>
          </div>
        </section>
      </div>
    </div>
  );
};

export default SettingsPanel;
