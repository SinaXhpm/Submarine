import { Server, KeyRound, Library, Activity, Settings, LogOut } from "lucide-react";

// Vertical rail on desktop, horizontal bottom dock on mobile. Layout swap is
// driven by `isMobile` so the terminal/session views can use the full screen
// width on phones — vertical sidebars eat ~50px of width that's expensive on
// a 360px-wide viewport. The parent in DesktopApp uses `flex-col-reverse` on
// mobile so this component visually lands at the bottom while staying first
// in DOM order (keyboard tab-order stays intuitive).
export const Sidebar = ({ activeTab, setActiveTab, isMobile, onLogout }: any) => {
  // Commands + Notes used to be separate rails. Both are per-profile lists
  // of small text blobs stored under the vault; the only real distinction
  // was "runnable snippet vs freeform prose". A single Library rail with
  // an internal segmented control uses one sidebar slot instead of two —
  // room the user asked us to give back on narrow viewports.
  const items = [
    { id: 'nodes', icon: Server, label: 'Servers' },
    { id: 'vault', icon: KeyRound, label: 'Logins' },
    { id: 'library', icon: Library, label: 'Library' },
    { id: 'monitor', icon: Activity, label: 'Monitor' },
  ];

  if (isMobile) {
    return (
      // `pb-[env(safe-area-inset-bottom)]` clears the Android gesture-nav pill
      // and iPhone home-indicator so the tab icons don't sit under system UI.
      // Height stays 56px minimum (h-14) via `min-h-14` — the extra padding
      // expands the aside downward into safe-area space, not upward into the
      // content area, so the terminal above doesn't lose vertical room.
      <aside
        style={{ paddingBottom: 'env(safe-area-inset-bottom)' }}
        className="w-full min-h-14 shrink-0 bg-background border-t border-white/5 flex items-center px-2 relative z-10 shadow-2xl brightness-95"
      >
        <nav className="flex-1 flex flex-row items-center justify-around gap-1">
          {items.map((item) => (
            <button
              key={item.id}
              onClick={() => setActiveTab(item.id)}
              className={`p-2.5 rounded-xl transition-all duration-300 relative group flex items-center justify-center ${
                activeTab === item.id
                  ? 'text-primary bg-primary/10 shadow-[0_0_20px_rgba(var(--primary),0.15)]'
                  : 'text-zinc-500 hover:text-zinc-200 hover:bg-white/5'
              }`}
              title={item.label}
            >
              <item.icon size={20} className={activeTab === item.id ? "drop-shadow-[0_0_8px_rgba(var(--primary),0.5)]" : ""} />
              {/* Bottom indicator stripe instead of left rail in mobile mode. */}
              {activeTab === item.id && <div className="absolute bottom-0 left-1/2 -translate-x-1/2 h-0.5 w-1/2 bg-primary rounded-t-full shadow-[0_0_10px_rgba(var(--primary),1)]" />}
            </button>
          ))}
        </nav>

        {/* Trailing cluster (logout + settings) — kept at the right edge of
            the horizontal bar so the muscle-memory mapping "settings is the
            last icon" still holds, just now read left-to-right. */}
        <div className="flex items-center gap-1 pl-2 ml-1 border-l border-white/5">
          {onLogout && (
            <button
              onClick={onLogout}
              className="p-2.5 rounded-xl transition-all flex items-center justify-center text-zinc-300 hover:text-amber-300 hover:bg-amber-500/10"
              title="Lock & switch profile"
            >
              <LogOut size={20} />
            </button>
          )}
          <button
            onClick={() => setActiveTab('settings')}
            className={`p-2.5 rounded-xl transition-all flex items-center justify-center ${
              activeTab === 'settings'
                ? 'text-primary bg-primary/10 shadow-[0_0_20px_rgba(var(--primary),0.15)]'
                : 'text-zinc-500 hover:text-zinc-200 hover:bg-white/5'
            }`}
            title="Settings"
          >
            <Settings size={20} className={activeTab === 'settings' ? "drop-shadow-[0_0_8px_rgba(var(--primary),0.5)]" : ""} />
          </button>
        </div>
      </aside>
    );
  }

  return (
    <aside className="w-14 h-full bg-background border-r border-white/5 flex flex-col items-center py-6 shrink-0 relative z-10 shadow-2xl brightness-95 transition-all">
      <nav className="flex flex-col gap-3 w-full px-2">
        {items.map((item) => (
          <button
            key={item.id}
            onClick={() => setActiveTab(item.id)}
            className={`p-2.5 rounded-xl transition-all duration-300 relative group flex items-center justify-center ${
              activeTab === item.id
                ? 'text-primary bg-primary/10 shadow-[0_0_20px_rgba(var(--primary),0.15)]'
                : 'text-zinc-500 hover:text-zinc-200 hover:bg-white/5'
            }`}
            title={item.label}
          >
            <item.icon size={20} className={activeTab === item.id ? "drop-shadow-[0_0_8px_rgba(var(--primary),0.5)]" : ""} />
            {activeTab === item.id && <div className="absolute left-0 top-1/2 -translate-y-1/2 w-0.5 h-1/2 bg-primary rounded-r-full shadow-[0_0_10px_rgba(var(--primary),1)]" />}
          </button>
        ))}
      </nav>

      {/* Bottom cluster: logout (lock current profile, return to picker)
          and settings. Logout sits above settings so the gear stays as the
          last-most icon — matches the muscle memory from when it was the
          only bottom action. */}
      <div className="mt-auto w-full px-2 flex flex-col gap-2">
        {onLogout && (
          <button
            onClick={onLogout}
            className="p-2.5 rounded-xl transition-all flex items-center justify-center text-zinc-300 hover:text-amber-300 hover:bg-amber-500/10"
            title="Lock & switch profile"
          >
            <LogOut size={20} />
          </button>
        )}
        <button
          onClick={() => setActiveTab('settings')}
          className={`p-2.5 rounded-xl transition-all flex items-center justify-center ${
            activeTab === 'settings'
              ? 'text-primary bg-primary/10 shadow-[0_0_20px_rgba(var(--primary),0.15)]'
              : 'text-zinc-500 hover:text-zinc-200 hover:bg-white/5'
          }`}
          title="Settings"
        >
          <Settings size={20} className={activeTab === 'settings' ? "drop-shadow-[0_0_8px_rgba(var(--primary),0.5)]" : ""} />
        </button>
      </div>
    </aside>
  );
};
