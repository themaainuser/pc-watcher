# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Users

The machine owner running PC-Watcher on their own Windows 10/11 PC (single-user, personal monitoring of their own machine). They open the dashboard from the system tray to answer "which apps talked to which hosts, when, and what was on screen."

## Product Purpose

An always-on local network/activity monitor: per-process connection metadata, service/hostnames, and foreground app usage recorded into a local SQLite database, with a localhost dashboard to explore it. Success = the owner can see and drill into their machine's activity within seconds of opening the dashboard, with no cloud, no account, and no payload capture.

## Positioning

Metadata-only and local-first: the dashboard binds to 127.0.0.1 only, the app has no HTTP client (no telemetry, no upload), and capture excludes payloads, URLs, keystrokes, screenshots, and command lines. A neighboring product could not truthfully copy "your data never leaves this machine" because their architecture requires a server.

## Operating Context

- Served by the `pcw-dash` Rust binary at `http://127.0.0.1:7411/` (port configurable), opened from the tray of an elevated agent.
- One embedded HTML file (`crates/pcw-dashboard/src/ui.html`, `include_str!`) with vanilla JS — no frameworks, no build step, no external resources (test-enforced: the only permitted URL string is the SVG namespace).
- Four API endpoints: `/api/overview`, `/api/app`, `/api/service`, `/api/usage`, plus `POST /api/shutdown` (the Stop button).
- Data volume: ~160k connection rows after one afternoon; hourly buckets in the tens of thousands.

## Capabilities and Constraints

- Four views: Overview (default, today's range), App drill-down, Service drill-down, Usage + window-title log.
- Filter bar: from/to dates, app, protocol (tcp/udp), port, direction (out/in); filter params are lowercase.
- Hourly SVG bar chart; clickable app/service lists that navigate to drill-downs; a Stop button that shuts the dashboard down.
- Hard constraints: single file, vanilla JS, zero external URLs/fonts/CDNs, same-origin fetch only, dates must be built from local date components (the `valueAsDate` UTC bug, fixed at `9b94d5e`, must not return — guard test bans the token).
- Undecided (open): long-term information architecture beyond the four existing views.

## Brand Commitments

- Name: "PC Watcher".
- Binding visual reference: the user supplied a screenshot of a fintech dashboard ("zentra") as the aesthetic target and asked for a pixel-faithful layout language: light gray canvas, large rounded white cards with soft shadows, dark pill navigation, big display numbers, vibrant hatched/striped charts (blue/pink/green), pill-shaped date-range controls, organized card grid, more vibrant and navigation-friendly than the incumbent plain UI. *(Assumption labeled: the image lives in the conversation, not on disk; the target is its layout language and energy applied to PC-Watcher's own data — not its financial content.)*
- Voice: direct, honest, technical — no hype copy.

## Evidence on Hand

- Live data on this machine: `data.db` (~32 MB, 160,415 connections, real hostnames, window-title sessions).
- The reference screenshot (conversation attachment only — no file path).
- 117 passing tests pinning UI contract: no-external-URLs, local-date guard, route/response shapes.
- Absence future work must not fabricate: no measured resource-footprint numbers yet (README placeholder "pending manual run").

## Product Principles

1. Local-first is the product truth: nothing the UI shows implies cloud, accounts, or remote access.
2. Metadata clarity beats decoration — every pixel serves "who talked to whom, when, and what was on screen."
3. The UI must stay a single offline file; design ambition lives inside that constraint.
4. Honesty over polish: empty/error states say what is actually happening (agent paused, no data in range).
5. The owner is the only user; navigation must make drill-downs obvious without instructions.

## Accessibility & Inclusion

No product-specific requirement was established; keep baseline keyboard reachability and readable contrast as a floor (assumption labeled — inferred from good practice, not a user statement).
