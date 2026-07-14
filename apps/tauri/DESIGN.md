Chosen canonical direction: Technical Utility

> Historical Demo design brief ค่ะ Implementation status และ roadmap ปัจจุบันอยู่ที่ [../../PROJECT_PLAN.md](../../PROJECT_PLAN.md) ส่วน live Demo evidence อยู่ที่ [../../docs/demo/RESULTS.md](../../docs/demo/RESULTS.md) ค่ะ

---
version: demo-0.1
name: myVault local desktop workspace
description: Dense, calm Markdown workspace for one person across desktop and mobile-sized windows.
colors:
  primary: "#8DB7FF"
  surface: "#0E1116"
  surfaceRaised: "#151A21"
  surfaceInset: "#0A0D11"
  border: "#29313B"
  text: "#E8EDF3"
  textMuted: "#919BA8"
  accent: "#8DB7FF"
  success: "#72C497"
  warning: "#E0B36A"
  danger: "#E58B8B"
  focus: "#B6CEFF"
typography:
  ui: "Inter, Noto Sans Thai, system-ui, sans-serif"
  reading: "Charter, Noto Serif Thai, Georgia, serif"
  code: "SFMono-Regular, Consolas, Liberation Mono, monospace"
rounded:
  sm: 3px
  md: 6px
  lg: 10px
spacing:
  xs: 4px
  sm: 8px
  md: 12px
  lg: 18px
  xl: 24px
---

## Direction rationale

myVault is a working instrument, not a dashboard. Technical Utility supports dense navigation, quiet status signals, predictable keyboard flow, and long-form Thai/English reading without decorative chrome.

## Surface brief

- Surface: local-first Markdown editor/reader prototype.
- Primary job: open a Vault, find a note, read or edit it, and understand whether work is safely saved.
- Primary action: open a Vault when none is active; otherwise select and edit a note.
- Viewports: desktop from 760px upward; compact drawer layout at 360px and 412px.
- Data contract: `vault_status`, `vault_choose_folder`, paged `vault_list_explorer`, `vault_read_note`, and `vault_save_note` ค่ะ
- Runtime truth: frontend can compile independently; full behavior requires the Tauri commands.
- Done means: coherent three-pane shell, safe conflict states, Markdown reader, keyboard navigation, and responsive drawers.

## Layout

- Desktop: 48px activity rail, 248px explorer, flexible editor/reader, 272px context panel.
- Center toolbar owns the note title, Edit/Read mode, and save state.
- Context panel switches between outline, backlinks, and graph without floating cards.
- Compact: rail becomes a bottom command bar; explorer and context become modal drawers with backdrops; center remains full width.

## Typography and color

- UI labels use compact sans-serif text with restrained uppercase only for section labels.
- Reader uses a serif stack for paragraph rhythm; code and editor use a monospace stack.
- Blue is reserved for focus, selection, and primary action. Green, amber, and red only communicate state.
- Surfaces are distinguished by one-pixel borders and small luminance shifts. No gradients, glass, or large shadows.

## Components and states

- Vault opener: calm empty workspace, one clear button, honest local-only explanation, busy and error states.
- Explorer: filter, nested inferred folders, selected note, loading, empty, retry, and keyboard-visible focus.
- Editor: CodeMirror, autosave after 750ms idle, manual save, dirty/saving/saved/conflict/unknown/error states.
- Reader: sanitized GFM HTML, strict Mermaid, visible wiki-link tokens, readable tables and code blocks.
- Quick switcher: command dialog opened by Cmd/Ctrl-P, filtered note list, arrow/Enter/Escape controls.
- Mobile drawers: labelled close buttons, focus-visible controls, backdrop dismissal, no content hidden behind the bottom bar.

## Safety behavior

- `staleRevision` stops autosave and never retries automatically.
- `writeOutcomeUnknown` keeps the editor text, stops autosave, and asks the user to reopen/verify before another write.
- A note switch never silently discards dirty text; the current prototype keeps the note selected until save succeeds or the user explicitly reloads after a conflict.
- Reader HTML is sanitized before insertion; Mermaid runs with `securityLevel: strict`.

## Do and do not

Do use alignment, restrained borders, keyboard shortcuts, explicit empty/error states, and Thai-capable typography.

Do not use hero layouts, dashboard cards, gradients, fake metrics, glass effects, or decorative icon spam.

## Review target

- hierarchy: 4/5
- specificity: 4/5
- execution: 4/5 after browser evidence
- restraint: 5/5
- buildability: 4/5

Live Tauri picker/read/save และ macOS Copy-of-Vault UAT ผ่านแล้วค่ะ Compact 412px/360px behavior มี implementation และ automated logic checks แต่ยังไม่มีหลักฐาน physical Android viewport/IME จึงต้องไม่อ่าน review score ด้านบนเป็น cross-platform visual acceptance ค่ะ
