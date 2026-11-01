# 0005: Milestones as quality gates, no dates

- Status: Accepted
- Date: 2026-11-01

## Context

Quality comes before deadlines. Calendar dates push towards cutting corners, so the roadmap
has none. Windows and Android come first; Linux and macOS come later.

## Decision

Work advances through milestones. A milestone is done when its gate is met, never by a date.

- **v0.1: Windows usable.** Main formats, HDD/SSD profiles with a live fast/gentle switch, CLI,
  minimal GUI, classic context menu, fuzzing per format.
- **v0.2: Android.** Spike first (see ADR 0006), then the app.
- **v0.3 onwards:** remaining formats, parallel decompression, cached index (each only with a
  measurement, see section 5 of [`../DECISIONS.md`](../DECISIONS.md)).
- **v1.0:** PGO, final name, SignPath signing, Windows 11 menu.

The final name is closed before the first public release. The first releases are unsigned.

## Consequences

- No task, report or document states a delivery date.
- A milestone that fails its gate stays open; nothing is dropped from it to ship sooner.
