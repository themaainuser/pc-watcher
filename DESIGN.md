---
name: PC Watcher
description: Local-only network and activity monitor — light-canvas rounded-card dashboard with hatched data inks.
colors:
  canvas: "#e9e9ed"
  sheet: "#f6f6f8"
  card: "#ffffff"
  ink: "#14161b"
  ink-2: "#5b616e"
  ink-3: "#878d9a"
  hair: "#e8e8ee"
  hair-2: "#f1f1f5"
  cobalt: "#2563eb"
  cobalt-soft: "#eaf1ff"
  pink: "#ec4899"
  pink-soft: "#fdeef6"
  emerald: "#10b981"
  emerald-soft: "#e9faf3"
  amber: "#f59e0b"
  nav: "#15161a"
  danger: "#dc2626"
  danger-soft: "#fef2f2"
typography:
  display:
    fontFamily: "Inter, Segoe UI Variable Text, Segoe UI, system-ui, sans-serif"
    fontSize: "clamp(21px, 2vw, 30px)"
    fontWeight: 800
    lineHeight: 1.05
    letterSpacing: "-0.03em"
    fontFeature: "tabular-nums"
  title:
    fontFamily: "Inter, Segoe UI Variable Text, Segoe UI, system-ui, sans-serif"
    fontSize: "14.5px"
    fontWeight: 700
    lineHeight: 1.3
    letterSpacing: "-0.01em"
  body:
    fontFamily: "Inter, Segoe UI Variable Text, Segoe UI, system-ui, sans-serif"
    fontSize: "14px"
    fontWeight: 400
    lineHeight: 1.5
  label:
    fontFamily: "Inter, Segoe UI Variable Text, Segoe UI, system-ui, sans-serif"
    fontSize: "10.5px"
    fontWeight: 600
    lineHeight: 1.2
    letterSpacing: "0.09em"
  mono:
    fontFamily: "Cascadia Mono, Consolas, SF Mono, ui-monospace, monospace"
    fontSize: "12px"
    fontWeight: 400
    lineHeight: 1.5
rounded:
  card: "20px"
  pill: "999px"
  sheet: "26px"
  small: "10px"
spacing:
  xs: "4px"
  sm: "8px"
  md: "14px"
  lg: "18px"
  xl: "22px"
  xxl: "26px"
components:
  button-primary:
    backgroundColor: "{colors.cobalt}"
    textColor: "#ffffff"
    rounded: "{rounded.pill}"
    padding: "10px 20px"
    typography: "body"
  button-primary-hover:
    backgroundColor: "#1d4ed8"
    textColor: "#ffffff"
    rounded: "{rounded.pill}"
    padding: "10px 20px"
  button-secondary:
    backgroundColor: "{colors.card}"
    textColor: "{colors.ink}"
    rounded: "{rounded.pill}"
    padding: "10px 16px"
    typography: "body"
  chip:
    backgroundColor: "{colors.card}"
    textColor: "{colors.ink}"
    rounded: "{rounded.pill}"
    padding: "6px 12px"
  card:
    backgroundColor: "{colors.card}"
    textColor: "{colors.ink}"
    rounded: "{rounded.card}"
    padding: "20px 22px"
  input:
    backgroundColor: "{colors.card}"
    textColor: "{colors.ink}"
    rounded: "{rounded.pill}"
    padding: "6px 14px"
    typography: "body"
  nav-active:
    backgroundColor: "{colors.nav}"
    textColor: "#ffffff"
    rounded: "{rounded.pill}"
    padding: "9px 18px"
---

# Design System: PC Watcher

## Overview

**Creative North Star: "The Glass Desk"**

PC Watcher's dashboard is a sheet of frosted paper laid on a cool grey desk. Everything the machine did is printed on that sheet in crisp black ink, sorted into soft white cards you could pick up. The aesthetic refuses two defaults: the incumbent's flat undifferentiated list page, and the monitoring-category cliché of dark dense tables with tiny type and glowing accent lines. It is bright, organized, and glanceable — one screen answers "what talked, to whom, when" before any click.

The personality is calm confidence with a playful data hand. Structure is neutral and quiet (grey canvas, white cards, hairline rules, black type); all the color energy is spent on the data itself, which is drawn in four hatched inks so a series is recognisable by texture as well as hue. The single saturated cobalt is rationed to things you can press.

**Key Characteristics:**
- Cool light-grey canvas under a rounded white "sheet" that holds the whole app
- Oversized black tabular-figure numbers above small tracked-caps labels
- Hatched/striped data inks (cobalt, pink, emerald, amber) — never flat fills
- Pill-shaped controls everywhere; one saturated action color, rationed
- Hairline rules as the only dividers — no heavy borders, no nested cards

## Colors

A quiet neutral spine carrying four loud data inks.

### Primary
- **Cobalt** (#2563eb): the ONE saturated action color. Reserved strictly for pressable things — Apply, the active nav pill's accent, drill-down affordances, focus rings, the `::selection` tint, and the cobalt data series. Its rarity is what makes an action unmistakable.

### Secondary (data inks)
- **Hot Pink** (#ec4899): second data series (top hosts). Always hatched.
- **Emerald** (#10b981): third data series (foreground time). Always hatched.
- **Amber** (#f59e0b): fourth data series (reserved). Always hatched.

Each ink has a `-soft` companion (`#eaf1ff`, `#fdeef6`, `#e9faf3`) for tinted wash backgrounds.

### Neutral
- **Canvas** (#e9e9ed): the page field, the "desk".
- **Sheet** (#f6f6f8): the rounded app sheet floating on the canvas.
- **Card** (#ffffff): every content card.
- **Ink** (#14161b): primary text and display numbers.
- **Ink-2** (#5b616e): secondary text, subtitles, counts.
- **Ink-3** (#878d9a): tertiary text, labels, hints, axis ticks.
- **Hair** (#e8e8ee) / **Hair-2** (#f1f1f5): dividers and table-row separators.
- **Nav** (#15161a): the near-black active nav pill.
- **Danger** (#dc2626) with **Danger-soft** (#fef2f2): error surfaces and the Stop button's live dot.

### Named Rules
**The One Action Rule.** Cobalt appears only on things you can press. It is never a decorative background, never body text, never a chart series fill on its own. If a surface is cobalt, clicking it does something.

**The Hatched Ink Rule.** Data series are drawn as 135° repeating-linear-gradient hatching, never flat color fills. A series keeps its one ink across every view, so a color is learnable as an identity.

## Typography

**Display Font:** Inter (embedded as a base64 woff2, weights 400–800; fallback Segoe UI Variable Text → Segoe UI → system-ui)
**Body Font:** Inter (same stack)
**Mono Font:** Cascadia Mono → Consolas → SF Mono → ui-monospace (timestamps, IPs, ports, protocol/direction, error detail)

**Character:** One humanist sans doing all the work, with a mono voice reserved strictly for machine values. Personality comes from the scale jump — a huge tight-tracked display number sitting directly above a tiny wide-tracked caps label — not from a decorative second face.

### Hierarchy
- **Display** (800, `clamp(21px, 2vw, 30px)`, 1.05, -0.03em, tabular-nums): KPI numbers. The hero of every card.
- **Display-small** (700, `clamp(15px, 1.35vw, 19px)`, -0.015em): display values that are long text (app names, hostnames) rather than numerals.
- **Title** (700, 14.5px, -0.01em): card headings.
- **Body** (400, 14px, 1.5): default UI text.
- **Sub** (400, 12.5px, ink-2): card subtitles.
- **Label** (600, 10.5px, 0.09em, uppercase, ink-3): KPI captions and table headers.
- **Data** (mono 12px / 13px tabular): every machine value in a table.

### Named Rules
**The Tabular Rule.** All numerals that can change or line up — KPI values, counts, ranks, timestamps, table numbers — use `font-variant-numeric: tabular-nums`. Columns of numbers must align.

## Layout

A single centered **sheet** (max-width 1380px, 26px radius) floats on the 22px-padded canvas. Inside it: a sticky **top bar** (brand left, pill nav center, Stop right), a sticky **filter bar** beneath it, then the scrolling **main** region.

Main uses a two-column split above 1120px: a flexible **col-main** and a fixed **372px col-side** rail, 18px gutter. Below 1120px the columns stack (rail drops under main). Cards stack vertically in each column with a 18px rhythm; `.grid2` is a two-up grid (`minmax(0, 1fr)`).

KPI strips are a 5-across grid of `.kpi` cells divided by 1px left hairlines (first cell has no border and no left padding), collapsing to fewer columns on narrow widths.

Spacing rhythm: 4 / 8 / 14 / 18 / 22 / 26px. Card padding is 20px vertical, 22px horizontal.

**Responsive:** the mobile breakpoint un-nests the sheet (full-bleed, 0 radius, 100vh min-height), un-sticks the filter bar, halves the KPI grid to 2-up, and stacks `.grid2`. Every grid uses `minmax(0, 1fr)` so long hostnames shrink instead of forcing overflow.

## Elevation & Depth

Hybrid. Depth is carried by soft, low-opacity, offset diffuse shadows on a neutral tint — never a colored glow, never a hard offset block. Cards sit nearly flat at rest and lift on hover; pills carry a tighter shadow because they are small.

### Shadow Vocabulary
- **Card** (`0 1px 2px rgba(18,20,28,.05), 0 16px 36px -18px rgba(18,20,28,.18)`): resting elevation for every card.
- **Lift** (`0 2px 4px rgba(18,20,28,.06), 0 24px 48px -20px rgba(18,20,28,.26)`): hover state for cards and secondary buttons.
- **Pill** (`0 1px 2px rgba(18,20,28,.10), 0 3px 8px -4px rgba(18,20,28,.14)`): small controls — date range, fields, badges, Stop.
- **Sheet** (`0 2px 6px rgba(18,20,28,.06), 0 40px 80px -40px rgba(18,20,28,.35)`): the whole app sheet on the desk.
- **Nav-active** (`0 2px 8px -2px rgba(18,20,28,.4)`): the active nav pill.

### Named Rules
**The Offset-Blur Rule.** Every shadow carries a downward offset AND a soft blur with negative spread. A zero-offset halo is decoration and is not used.

## Shapes

Fully rounded pills (999px) for every control — buttons, nav items, filter fields, date range, badges. Cards get a generous 20px radius; the app sheet 26px; small inner elements (skeleton blocks) 10px. Borders are essentially absent on cards (`border: none` — the shadow defines the edge); hairlines appear only as internal dividers and table-row separators, always 1px in `hair`/`hair-2`. No hard corners anywhere.

## Components

### Buttons
- **Primary (Apply):** cobalt fill, white text, full pill, `10px 20px`, `600` at 13px. Hover deepens to `#1d4ed8` and lifts 1px.
- **Secondary (Stop, Reset, back pill):** white fill, ink text, 1px `hair` border, pill, pill shadow. Hover lifts 1px and takes the Lift shadow; `:active` settles back. Stop is disabled-at-`opacity:.55` while shutting down.
- **Focus:** all controls take `2px solid cobalt` outline at 2px offset via `:focus-visible` (6px radius on the ring).

### Chips / Badges
White fill, ink text, 1px `hair` border, full pill, pill shadow, `600` 12px. A `6px` cobalt dot marks a live/peak badge; the emphasized half renders at 800.

### Cards / Containers
White, 20px radius, no border, Card shadow, `20px 22px` padding. `.hoverable` lifts 2px and takes the Lift shadow on transition (180ms). Card headings are Title-size, with an optional right-aligned badge; a 12.5px ink-2 `.sub` sits under the heading. Cards are never nested.

### KPI Strip
A 5-column grid of cells split by 1px left hairlines. Each: a tracked-caps Label, then a Display number (tabular), then an optional 11.5px ink-3 note. Long display values use the display-small variant and ellipsize rather than overflow.

### Progress Rows (ranked lists)
A `.row` button: rank (mono, tabular, right-aligned) / name (600, ellipsized) / count (tabular, ink-2), with a full-width hatched bar (9px, pill) spanning beneath the name and count. Bar width animates in (600ms). The bar's ink identifies the series. Row hover tints the background and turns the name cobalt.

### Tables
Uppercase tracked-caps header row on a near-white ground with a `hair-2` top rule and `hair` bottom rule; rows separated by 1px `hair-2`, hover tinted. First column and all machine values render in mono; numbers are tabular. Cells pad `11px 14px`, first/last cells inset to 22px.

### Inputs / Fields
Pill-shaped white fields (`.pillfield`, `.daterange`) with 1px `hair` border and pill shadow; the label sits inline before the control. `:focus-within` turns the border cobalt and adds a 3px 14%-cobalt ring. Native date/select controls keep their affordance; text inputs set a cobalt caret.

### Navigation
A `.pillnav` group on a `#ececf1` pill trough, centered in the top bar. Inactive items are transparent with ink-2 text (500, 13.5px); hover darkens to ink; the active item becomes a near-black `nav` pill with white text and a soft shadow. A context pill (drill-down name + `×`) joins the group only while a drill-down is open and is dismissible.

### States
- **Loading:** skeleton blocks (10px radius, `#f0f0f4`→`#f8f8fb` shimmer sweep, 1.1s linear) standing in for the title, hero, and cards.
- **Empty:** centered ink-3 tray/clock SVG, an ink-2 bold title, and a 12.5px ink-3 hint capped at 44ch.
- **Error:** `danger-soft` panel with a 1px `#fca5a5` border, a bold `#7f1d1d` heading, and the message plus a mono `#b91c1c` detail line naming the recovery.
- **Motion:** one authored moment — cards settle in (opacity 0→1, translateY 14px→0, 500ms, `cubic-bezier(.16,1,.3,1)`, staggered), KPI numbers count up (480ms), chart bars grow from the baseline (550ms, staggered per hour). `prefers-reduced-motion` collapses all of it to instant.

## Do's and Don'ts

### Do:
- **Do** use tabular-nums on every numeral that can change or align.
- **Do** draw data series as 135° hatching in their one assigned ink, consistent across views.
- **Do** keep cobalt rationed to pressable things only.
- **Do** use `minmax(0, 1fr)` on every grid/flex track so long hostnames shrink instead of overflowing.
- **Do** separate groups with 1px hairlines and let shadows (not borders) define a card's edge.
- **Do** theme the browser surfaces — selection tint, caret, scrollbar, focus ring — from the palette.

### Don't:
- **Don't** nest cards or pad a card inside another card.
- **Don't** add a colored glow or a zero-offset halo shadow.
- **Don't** use a hard offset block shadow (`4px 4px 0`) — this is not a neobrutalist world.
- **Don't** fill a data series with flat color where hatching is the house style.
- **Don't** put a `border-left`/`border-right` accent stripe wider than 1px on a card.
- **Don't** render machine values (IPs, ports, timestamps, protocol) in the sans face — they are mono.
- **Don't** invent a second display face; the scale contrast is the personality.
