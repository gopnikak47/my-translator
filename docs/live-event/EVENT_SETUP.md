# Event setup guide — bilingual LED subtitles with vMix (OBS as fallback)

Pipeline:

```
Microphone → stage mixer → USB audio interface → My Translator (🎬 Event)
   → OpenAI Realtime (or Soniox) → local overlay server (127.0.0.1:8765)
   → vMix Web Browser inputs → vMix output(s) → HDMI → LED processor → LED screens
```

No OBS, NDI, extra encoding or network video is needed when vMix is the compositor.

---

## 0. Decide the video configuration first

| | Configuration A — one HDMI + LED processor region mapping | Configuration B — separate outputs |
|---|---|---|
| When | The LED processor can take **one** input and map **regions** of it to the left, main and right walls | The PC/vMix can drive **separate** outputs for each wall (multiple HDMI/DP ports, or a multi-output card) |
| vMix | One wide output canvas containing left subtitles · background · right subtitles | Main output for the background; External/Fullscreen outputs for each side screen |
| Overlay URL | `…/overlay?screen=canvas` **or** two `left`/`right` inputs placed in the wide layout | `…/overlay?screen=left` and `…/overlay?screen=right` |

> ⚠️ If the three walls are **hardware-cloned** from one HDMI signal (same picture
> on all), no software can show different content on them. You need either a
> processor that supports region mapping (A) or more outputs (B).

Get the **pixel map** from the LED technician: processor input resolution
(e.g. 3840×1080), and the x / y / width / height of each wall inside it. All
coordinates in My Translator are editable; the defaults (left 0,0 960×1080 ·
centre 960,0 1920×1080 · right 2880,0 960×1080) are examples only.

---

## 1. Prepare My Translator (once)

1. Install My Translator on the vMix PC (Windows 10/11).
2. **Settings → OpenAI**: paste the OpenAI API key (needs access to
   `gpt-realtime-translate`). **Settings → Soniox**: paste a Soniox key for the
   emergency fallback.
3. Open the **🎬 Event** tab.

## 2. Audio input

1. Connect the USB audio interface; feed it a **clean mixer send** (a mic/aux bus
   without music beds, playback or PA reverb). Do not use desktop/speaker audio.
2. Event tab → **Audio input** → choose the interface (⟳ rescans devices).
3. Speak into the stage mic: the meter must move. Keep peaks below the red
   (“CLIPPING” warning = lower the gain on the interface/mixer).
4. Noise reduction: *Off* for a clean mixer feed, *Near field* for a handheld/lav
   directly into the interface, *Far field* for a room mic.

My Translator never plays audio (no TTS) in Event mode, so there is no feedback loop.

## 3. Translation and display modes

* **Mode**: *Automatic English ↔ Vietnamese* (detects the speaker),
  *English → Vietnamese*, or *Vietnamese → English* (one-way modes are the most
  predictable fallback).
* **Bilingual strategy** (Auto + OpenAI): *Two prewarmed sessions* (instant
  switching, ~2× cost) or *One session + language switch* (~1× cost, the first
  words after a language change may come out in the previous language).
* **Display mode**
  * **A · Both screens: opposite of speaker** — MC speaks English → both side
    walls show Vietnamese; MC speaks Vietnamese → both show English.
  * **B · Left VI / Right EN** (or the mirror) — each wall keeps its language;
    when the speaker already speaks that language the wall shows the original
    words (no pointless same-language translation).
* Caption behaviour: partial captions ON for lowest latency; *Hold before
  clearing* 5–8 s; max 2 lines (per screen); roll-up or replace.

## 4. Style each screen

Left/Right LED cards: set **resolution** to the wall's pixel size (e.g. 960×1080),
font (default *Segoe UI*, full Vietnamese coverage on Windows), size, weight,
white text + dark outline (default), optional dark backing box and opacity,
safe margins, top/middle/bottom position, alignment, max lines. Changes apply
live on the overlays — no reload needed.

## 5. Start

1. Overlay server status must read *Running on 127.0.0.1 · port 8765*
   (it starts with the app; *Restart server* if you changed the port).
2. Select provider **OpenAI Realtime** → **▶ Start live subtitles**.
3. State turns **LIVE · OpenAI**. Speak — text appears on *Preview*.

## 6. vMix (preferred compositor)

1. In the Event tab click **Copy** next to `left` (and later `right`, or `canvas`).
2. vMix → **Add Input → Web Browser** → paste the URL.
3. Set the input size to the wall's resolution, e.g. **960 × 1080** for a side
   wall (for `canvas`: the processor input size, e.g. **3840 × 1080**).
   The page is transparent; vMix keys it automatically.
4. Repeat for `right` (Configuration B, or A with two inputs).
5. **Configuration A (one HDMI, region mapping):**
   1. Set the vMix output resolution to the processor input (e.g. 3840×1080 via a
      custom preset; if your vMix edition cannot exceed 1920×1080 output, use a
      1920×540-scaled map agreed with the LED technician or Configuration B).
   2. Put the background/slides/video input full-frame or into the centre region
      (Position: X 960, Y 0, W 1920, H 1080 in the example map).
   3. Add the `canvas` browser input as an overlay channel (Overlay 1) at full
      frame — or place the `left` and `right` inputs as layers at their regions.
   4. Send the output to the HDMI port feeding the processor (Settings →
      Outputs/Fullscreen).
6. **Configuration B (separate outputs):**
   1. Main HDMI: program output with background/slides as usual.
   2. Side walls: use vMix **External / Fullscreen 2** outputs (or a multi-output
      card) with the `left` and `right` browser inputs — ideally with a black or
      branded background layer under the transparent subtitles.
7. Verify on the walls: speak English and Vietnamese, check each wall shows the
   intended language, nothing is cropped by the LED processor, and text is
   readable from the back of the room. Use `…/overlay?screen=canvas&guides=1`
   during setup to see region outlines (remove `&guides=1` for the show).

## 7. OBS (alternative)

1. **Sources → + → Browser**: paste the overlay URL, Width/Height = wall
   resolution, untick “Shutdown source when not visible”, leave Custom CSS empty.
2. For Configuration A, set *Settings → Video* base/output canvas to the
   processor input size and place sources at the region coordinates; use
   *Fullscreen Projector (Program)* on the HDMI output.
3. For Configuration B, use separate scenes + *Fullscreen Projector* per output.

## 8. During the show — operator controls

| Action | Button | Hotkey (app focused) | HTTP (Stream Deck / vMix script) |
|---|---|---|---|
| Hide / show subtitles | 🙈 Hide | Ctrl+Alt+H | `POST /control/toggle` (`hide`, `show`) |
| Freeze current text | ❄ Freeze | Ctrl+Alt+F | `POST /control/toggle-freeze` |
| Clear screens | 🧹 Clear | Ctrl+Alt+C | `POST /control/clear` |
| Force English speaker | EN | Ctrl+Alt+E | `POST /control/lang-en` |
| Force Vietnamese speaker | VI | Ctrl+Alt+V | `POST /control/lang-vi` |
| Back to automatic | Auto | Ctrl+Alt+A | `POST /control/lang-auto` |

HTTP control: `http://127.0.0.1:8765/control/<action>` with header
`X-Access-Token: <token>` (button *Copy control token*). Example:
`curl -X POST -H "X-Access-Token: TOKEN" http://127.0.0.1:8765/control/hide`.

**Emergency fallback:** if the state stays *Reconnecting…* or shows an error,
press **⇄ Fall back to Soniox**. Subtitles continue on the same browser inputs;
nothing changes in vMix. Technical errors are only shown in the app, never on the
LED walls.

Health panel: provider, connection state per session, reconnects, audio
status, time-to-first-caption, update intervals, overlay paint latency, memory.

## 9. Checklist (doors open −60 min)

- [ ] Internet stable (wired); OpenAI and Soniox keys valid (do a 1-minute test)
- [ ] Correct USB interface selected, meter moving, no clipping
- [ ] Overlay server running; vMix inputs show captions on Preview
- [ ] Each LED wall shows the intended region/language (test EN and VI)
- [ ] Hold time, max lines and font size readable from the back row
- [ ] Hotkeys/Stream Deck tested (hide, clear, EN/VI override)
- [ ] Fallback rehearsed (OpenAI → Soniox → OpenAI)
- [ ] Windows power plan “High performance”, sleep and USB selective suspend disabled

## 10. Troubleshooting

| Symptom | Check |
|---|---|
| Browser input is blank | Paste the URL in a normal browser on the same PC; server state in the Event tab; port not used by another app (change port → Restart server → re-copy URLs) |
| Text on the wrong wall / cut off | Region coordinates vs processor pixel map; wall resolution in the screen card; safe margins |
| Wrong language on screen | Press EN / VI (manual override); check the mode; Auto needs a few words to detect |
| “Input lost — reconnecting” | USB cable/hub; the app reopens the device automatically and bridges the gap with silence |
| “OpenAI rejected the API key” | Key / billing in the OpenAI dashboard; switch to Soniox meanwhile |
| Captions lag behind | Network RTT in the health panel; wired connection; consider one-way mode |
