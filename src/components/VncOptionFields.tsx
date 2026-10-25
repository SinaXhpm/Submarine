import { ChoiceRow, QuickChoiceRow, SwitchRow } from "./FormRows";
import {
  VNC_COLOR_CHOICES, VNC_ENCODING_CHOICES, VNC_FPS_CHOICES, VNC_PRESETS, VNC_RESOLUTION_CHOICES, vncPresetOf,
  type VncColors, type VncEncoding, type VncMaxFps, type VncOptions, type VncPreset, type VncResolution,
} from "../util/vncOptions";

// A VNC server's speed and quality, as the server form and the VNC tab's
// settings panel show them. Both keep them in this device's options for the
// server (vncOptions.ts).

const PRESET_HINTS: Record<VncPreset, string> = {
  standard: "Full colour, as many updates as the link carries: what the tab asks for unless told otherwise.",
  light: "16-bit colour, 15 updates a second, and three quarters of the resolution: for a slow or metered link.",
  least: "256 colours, 5 updates a second, and half the resolution: for a very slow link.",
};

export function VncSpeedFields({ o, set }: { o: VncOptions; set: (patch: Partial<VncOptions>) => void }) {
  const preset = vncPresetOf(o);
  return (
    <div className="space-y-2.5">
      <QuickChoiceRow
        choices={VNC_PRESETS}
        current={preset}
        onPick={(v) => { const p = VNC_PRESETS.find((p) => p.value === v); if (p) set(p.set); }}
        hint={preset ? PRESET_HINTS[preset] : "Your own mix of the choices below."}
      />
      <ChoiceRow
        label="Encoding"
        hint="How the server packs the screen. Automatic suits most links. Tight packs the most but needs full colour; ZRLE packs well at any depth; Raw is the least work for both ends and the most to send."
        value={o.encoding}
        onChange={(v) => set({ encoding: v as VncEncoding })}
        choices={VNC_ENCODING_CHOICES.map((c) => ({ ...c, disabled: c.value === "tight" && o.colors !== 32 }))}
      />
      <ChoiceRow
        label="Colours"
        hint="Fewer colours is less to send on a slow link."
        value={o.colors}
        onChange={(v) => set({ colors: Number(v) as VncColors })}
        choices={VNC_COLOR_CHOICES}
      />
      <ChoiceRow
        label="Updates"
        hint="Fewer updates a second use less bandwidth and battery; the screen and the pointer move less smoothly."
        value={o.maxFps}
        onChange={(v) => set({ maxFps: Number(v) as VncMaxFps })}
        choices={VNC_FPS_CHOICES}
      />
      <ChoiceRow
        label="Resolution"
        hint="The desktop's size for the tab, where the server can change it (TigerVNC can; a server showing a physical screen can't). Lower is fewer pixels to send; the picture is drawn larger and softer."
        value={o.resolution}
        onChange={(v) => set({ resolution: Number(v) as VncResolution })}
        choices={VNC_RESOLUTION_CHOICES}
      />
      <SwitchRow
        label="Pause while hidden"
        hint="While the tab isn't shown, no updates are asked for; when it is back, what changed meanwhile comes at once."
        checked={o.pauseHidden}
        onChange={(pauseHidden) => set({ pauseHidden })}
      />
    </div>
  );
}
