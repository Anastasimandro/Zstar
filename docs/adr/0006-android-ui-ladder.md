# 0006: Android UI ladder and spike

- Status: Accepted
- Date: 2026-11-01
- Changes: section 7 (Android) of [`../DECISIONS.md`](../DECISIONS.md), which previously only said a minimal Java/Kotlin fragment "may be needed" for the keyboard

## Context

The Android app shares its Slint UI with the desktop. Slint on Android is the riskiest part:
on-screen keyboard, long lists and accessibility (TalkBack) are not proven for this project.

## Decision

UI ladder, climbed only when the level below fails:

- **Level A:** everything in Rust with Slint, no Java/Kotlin.
- **Level B:** a minimal Kotlin/Java fragment only for the keyboard, Open with or SAF.
- **Level C:** the whole UI in Kotlin. Last resort, and only with the owner's approval.

Before building the app, a throwaway spike in a folder outside the workspace tests the keyboard,
a long list and TalkBack, each with a pass/fail result. Its time limit (about two sessions) is a
review point, not a hard stop.

## Consequences

- The spike code is never merged into the workspace.
- The spike results decide the starting level and are recorded in `docs/reports/`.
