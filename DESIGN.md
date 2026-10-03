# Space OS boot and recovery design

## 1. Atmosphere and identity
A calm, keyboard-first startup screen inspired by Windows recovery. Space OS keeps
its own name and presents the next action, detected hardware and recovery choices.
Firmware GOP rendering works before a native GPU driver exists. A light porcelain
canvas, strong ink typography and a restrained teal accent give startup, recovery
and installation the same identity. Firmware text remains the fallback.

## 2. Color
GOP tokens, exported by `boot/spaceboot/src/ui.rs`: BG = #EEF2F6,
INK = #172B42, MUTED = #52677D, ACCENT = #087F8C, WHITE = #FFFFFF,
BORDER = #D5DFE8, SOFT = #DDF0F2, WARNING = #995C12. Focus uses a teal stripe
and pale teal interior with dark labels. Status always has a written label.
Text fallback tokens: Black canvas, LightGray labels, Cyan focused action,
Black focus labels. Errors retain an explicit reason.

## 3. Typography
GOP uses bundled Noto Sans Mono bitmap Regular, base height 16 pixels. Metadata
and descriptions use scale 1 (16px); action labels also use scale 1; screen
titles use scale 2 (32px). Unsupported characters use a finite
single '?' fallback. Text fallback uses firmware monospace. ASCII labels keep
both paths portable. No font downloads, logos or bitmap screenshots.

## 4. Spacing and layout
GOP base spacing unit = 8px with explicit pixel positions below. Outer inset = 48px (32px below 800px wide),
header wordmark top = 24px, header rule = 56px, title top = 70px, subtitle
top = 112px. Content starts at 146px. Panel border = 1px; report inner inset = 20px;
action pitch = min(70px, (screen height - 190px) / 7); focus stripe = 4px;
home footer bottom inset = 35px. Descriptions appear when action pitch is at least 54px.
All supported GOP views keep primary actions on the left (60% width, 24px gap)
and a right machine-status panel (40% width). At 640x480 the compact view hides
action descriptions and bounds text to its own panel.
All primitives clip to the actual GOP dimensions; no native framebuffer access.
Text fallback uses actual console columns/rows. Horizontal inset 2 cells,
wordmark row 1, title row 3, subtitle row 5, first action row 7, action pitch 2 rows.
Labels truncate to the current width. Detail reports wrap at character boundaries
and paginate at the available row count. GOP report line pitch = 20px; report
height = screen height - 230px. Detail footers use bottom insets 56px (actions)
and 30px (page indicator). Console footers use rows - 4 and rows - 2.

## 5. Components
Screen: title, subtitle, body, footer. Action: numbered label and optional short
description, teal focus stripe. Report: label/value rows with actual detected
values and explicit limitations. Error screen: readable reason, reboot or firmware
exit; no timed disappearance. All rendering goes through the same line primitive.

## 6. Motion and interaction
No animation. Up/Down changes focus; Enter activates; number keys activate directly.
PgUp/PgDn changes detail report pages without triggering another action. The page
indicator remains visible even for one page. Network D/R/Esc and installer
Up/Down/Enter/I retain their existing actions. Hardware and result screens return
on any other key; installer review cancels on any key other than I or page navigation.
Escape exits to firmware from the main menu. Recovery
selects the terminal with storage probing disabled. No automatic disk writes.

## 7. Depth and surface
Flat porcelain background, white bordered panels and pale teal focus surfaces.
No shadows, gradients or decorative imagery. Every GOP frame is composed into an
owned, fallibly allocated pixel buffer then submitted via safe UEFI GOP BLT.
Serial markers are emitted by callers before painting; console output must not
overwrite a presented frame. The text fallback remains usable on serial consoles.
