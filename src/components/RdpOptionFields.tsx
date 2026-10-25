import { invoke } from "@tauri-apps/api/core";
import { ChoiceRow, QuickChoiceRow, SwitchRow } from "./FormRows";
import {
  RDP_COLOR_CHOICES, RDP_EFFECT_CHOICES, RDP_NETWORK_CHOICES, RDP_PRESETS, RDP_RESOLUTION_CHOICES,
  rdpPresetOf, rdpShareChoices,
  type RdpColors, type RdpEffects, type RdpNetwork, type RdpOptions, type RdpPreset, type RdpResolution, type RdpShareKind,
} from "../util/rdpOptions";
import { IS_WINDOWS } from "../util/platform";

// The built-in RDP client's choices of what it shares and of its speed and
// quality, as the server form and the RDP tab's panels show them. Both keep
// them in this device's options for the server (rdpOptions.ts).

type Fields = { o: RdpOptions; set: (patch: Partial<RdpOptions>) => void };

// Files of this computer's shared with the server (rdp_share.rs). Not on a phone.
export function RdpShareFields({ o, set, drives }: Fields & { drives: string[] }) {
  return (
    <div className="space-y-2">
      <ChoiceRow
        label="Share files"
        hint={o.share === "off" ? "Nothing of this computer's is shared." : "The server sees it as \\\\tsclient\\<name>, and nothing else of this computer's."}
        value={o.share}
        onChange={(v) => {
          const share = v as RdpShareKind;
          // A whole drive starts read only; a folder, writable.
          if (share !== o.share) set({ share, sharePath: "", shareReadOnly: share === "drive" });
        }}
        choices={rdpShareChoices(IS_WINDOWS)}
      />
      {o.share === "folder" && (
        <div className="flex items-center gap-2">
          <input
            type="text"
            readOnly
            aria-label="Shared folder"
            value={o.sharePath}
            placeholder="No folder chosen"
            className="flex-1 min-w-0 h-9 bg-[#1a1a1e] rounded-lg px-3 text-[12px] text-zinc-300 border border-white/10 outline-none shadow-inner"
          />
          <button
            type="button"
            onClick={() => invoke<string | null>("pick_local_directory").then((p) => { if (p) set({ sharePath: p }); }, () => {})}
            className="shrink-0 h-9 px-3 rounded-lg bg-white/5 hover:bg-white/10 text-[12px] font-semibold text-zinc-200 border border-white/10 transition-colors"
          >
            Choose…
          </button>
        </div>
      )}
      {o.share === "drive" && (
        <select
          aria-label="Shared drive"
          className="w-full h-9 bg-[#1a1a1e] rounded-lg px-3 text-[12px] text-zinc-300 border border-white/10 outline-none focus:border-primary/50 focus:bg-[#232328] transition-all shadow-inner"
          value={o.sharePath}
          onChange={(e) => set({ sharePath: e.target.value })}
        >
          <option value="" className="bg-[#1a1a1e]">Pick a drive</option>
          {drives.map((d) => <option key={d} value={d} className="bg-[#1a1a1e]">{d}</option>)}
        </select>
      )}
      {o.share !== "off" && (
        <SwitchRow
          label="Read only"
          hint="The server can open and copy the files, but not change, add or delete any."
          checked={o.shareReadOnly}
          onChange={(shareReadOnly) => set({ shareReadOnly })}
        />
      )}
    </div>
  );
}

const PRESET_HINTS: Record<RdpPreset, string> = {
  best: "Full colour and all desktop effects: for a fast link.",
  balanced: "What this client asks for unless told otherwise.",
  light: "16-bit colour, no desktop effects, less detail, and three quarters of the resolution: for a slow or metered link.",
};

// The speed and quality, with the quick sets of them first.
export function RdpSpeedFields({ o, set }: Fields) {
  const preset = rdpPresetOf(o);
  return (
    <div className="space-y-2.5">
      <QuickChoiceRow
        choices={RDP_PRESETS}
        current={preset}
        onPick={(v) => { const p = RDP_PRESETS.find((p) => p.value === v); if (p) set(p.set); }}
        hint={preset ? PRESET_HINTS[preset] : "Your own mix of the choices below."}
      />
      <ChoiceRow
        label="Colours"
        hint="Fewer bits is less to send; smooth gradients may show bands."
        value={o.colors}
        onChange={(v) => set({ colors: Number(v) as RdpColors })}
        choices={RDP_COLOR_CHOICES}
      />
      <ChoiceRow
        label="Desktop effects"
        hint="Windows servers only. Minimal leaves out the wallpaper, visual styles and animations, for a faster picture on a slow link; Full turns them all on."
        value={o.effects}
        onChange={(v) => set({ effects: v as RdpEffects })}
        choices={RDP_EFFECT_CHOICES}
      />
      <ChoiceRow
        label="Network"
        hint="Tells the server how fast the link is. On a slower setting it may send less detail."
        value={o.network}
        onChange={(v) => set({ network: v as RdpNetwork })}
        choices={RDP_NETWORK_CHOICES}
      />
      <ChoiceRow
        label="Resolution"
        hint="The desktop's size for the tab. Lower is fewer pixels to send; the picture is drawn larger and softer."
        value={o.resolution}
        onChange={(v) => set({ resolution: Number(v) as RdpResolution })}
        choices={RDP_RESOLUTION_CHOICES}
      />
      <SwitchRow
        label="Pause while hidden"
        hint="While the tab isn't shown, the server sends nothing for it; when it is back, it sends the whole picture again."
        checked={o.pauseHidden}
        onChange={(pauseHidden) => set({ pauseHidden })}
      />
      <SwitchRow
        label="Offer RemoteFX"
        hint="A picture codec of Windows servers. Used only if the server has it; if the picture looks wrong, turn it off."
        checked={o.remoteFx}
        onChange={(remoteFx) => set({ remoteFx })}
      />
    </div>
  );
}
