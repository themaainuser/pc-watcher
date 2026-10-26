---
version: 1
slug: "crates-pcw-dashboard-src-ui-html"
primary_target: "crates/pcw-dashboard/src/ui.html"
related_targets: []
---

# Surface brief — crates/pcw-dashboard/src/ui.html (dashboard)

## Scope and visitor mode

Operate surface: the machine owner, opening the dashboard from the tray, scanning today's activity and drilling into apps/services. One page, four views (Overview, App, Service, Usage) in one embedded file. Hard constraint: single HTML file, vanilla JS, zero external URLs (test-enforced), same-origin fetch only.

## Direction contract

THESIS: The owner's own machine activity, rendered as a bright, organized, glanceable card dashboard — refusing both the incumbent's plain undifferentiated list page and the monitoring-category default of dark dense tables with tiny type. One screen answers "what talked, to whom, when" before any click.

OWN-WORLD: Warm light-gray canvas; white cards with ~20px radius and soft diffuse shadows; near-black pill navigation; oversized black display numbers in tabular figures above small tracked-caps labels; hatched/striped chart inks in cobalt, hot pink, emerald and amber with soft gradient washes; hairline rules dividing metric groups inside cards; pill-shaped controls (date range, selects, Apply); ONE saturated action color (cobalt) reserved strictly for pressable things — Apply, Stop, active nav, drill-down affordances.

STORY: Owner opens from tray → within seconds reads today's totals, hourly rhythm, and top talkers → clicks an app or host to see destinations, ports, and what was on screen → narrows with filters → stops the dashboard when done. Belief: this is my machine, live, local, mine alone.

FIRST VIEWPORT (1440×900): sticky top bar — SVG logo mark + "PC Watcher" wordmark left, dark pill nav center (Overview active; Usage as the second item; a context pill naming the current drill-down appears only inside a drill-down, dismissible via ×), date-range pill + Stop button right. Below: page title "Overview" at display size; KPI tile row (total connections huge, top app, top host, active hours, sessions); hero card = hourly connections as hatched bars with hover tooltip pill; right rail = Top apps and Top hosts cards with striped progress bars, clickable — apps and hosts open as drill-down views reached from those rail lists, not as separate top-level tabs. Below the fold: foreground-time and title-log cards. Drill-downs replace the content region with a back affordance and set the context pill; filters live in a pill row under the header.

FORM: The brief-pinned reference's light-canvas rounded-card dashboard (resolved as canon — the user named the product and set its craft level as the bar), position: brief-pinned, beats the roll; seed key fd691bdc.

FINISH: unreviewed and undocumented is unfinished; this build ends with the finish review, the verdict, DESIGN.md, and every shipping raster carrying its provenance.
