# Zstar: decisions

Read this file fully at the start of every phase. It holds closed decisions: do not reopen them. If a task contradicts anything here, stop and tell the owner before continuing.

## 1. What this is
Zstar (provisional name) is a fast, low-memory, cross-platform archive manager written in Rust. Open source, public GitHub repository, license GPL-3.0-only. Priorities in order: performance, low memory, fast startup; then aesthetics. Everything is designed for all platforms from day one; nothing is "left for later" at architecture level.

The owner speaks Spanish. Code, comments, commits, docs and this file are in English. The app UI ships in Spanish and English.

## 2. Platforms
Windows 10/11 (x64, ARM64), Linux (x64, ARM64), macOS (universal), Android (full app with its own file browser, ZArchiver-level parity). ARM64 is a goal for the first stable release, never a blocker for -rc tags.

## 3. Workspace (one repo, one Cargo workspace)
- core: compression, formats, progress, cancellation, encryption. No UI, no OS-specific code, depends on no other workspace crate. Stream-based API (Read/Write/Seek), not only paths. It receives already-resolved drive info; it does not detect hardware itself.
- cli (binary zstar): terminal only, no graphical dependencies. Depends only on core.
- gui (binary zstar-gui): single Slint UI for all systems, desktop and touch layouts. Depends only on core.
- platform-android: Rust module with JNI; links core and gui.
- shell-win (COM DLL with IExplorerCommand, sparse/MSIX package, classic registry fallback), shell-linux (Dolphin, Nemo, Thunar, Caja, Nautilus), shell-mac (Quick Actions and .app). shell-* do NOT link core: they are thin launchers that call zstar or zstar-gui with arguments.
- One or two executables: default is two; decide by measuring cold start in Phase 0.
- The project name lives in one place (workspace metadata plus one constant) so renaming is cheap.

## 4. Formats
- Create and extract: zip, tar, tar.gz, tar.bz2, tar.xz, tar.zst, tar.lz4, 7z, single-file gz, bz2, xz, zst, lz4, br.
- Extract only: RAR (through libarchive, NEVER the unrar crate), CAB, cpio, ar, deb.
- Creating RAR is permanently forbidden (proprietary algorithm; the license forbids recreating it). Never add it, not even as an experiment.
- Out of scope: ISO and exotic formats.
- Each format sits behind a Cargo feature with a common interface. Detection by magic bytes. AES-256 only for zip and 7z.

## 5. Engine
- Streaming with fixed buffers and near-constant memory. Multithreaded with a configurable limit.
- Write to a .part file and rename on success.
- Defend against zip slip and decompression bombs. Handle long paths, Windows reserved names, Unix permissions and symlinks, CP437 names.
- Config in TOML. No tokio unless proven necessary.
- Release profile: LTO fat, codegen-units=1, panic=abort, strip. PGO comes later.
- Profiles: Fast, Balanced, Gentle (low priority and few threads, does not slow the system), Auto.
- Disk detection: ask the OS first; run a short sequential-read measurement only if the OS answers "unknown"; keep a manual per-drive override. HDD: sequential I/O, one stream, big buffers, no parallel many-file extraction. SSD: parallelism.
- core exposes a control handle: progress, cancel, plus live-adjustable threads, CPU/IO priority and throughput limit. Changing compression level mid-operation is a later improvement and only at block or file boundaries.
- Benchmarks: hyperfine and peak RAM against 7-Zip and ouch.

## 6. UI
- Slint first, egui as plan B; decided in the Phase 0 proof of concept (same dialogs, measure memory, startup, binary size, real effort). Tauri and Dioxus (WebView) are rejected.
- Use Slint only under GPL-3.0-only. Never the Royalty-free license: it is not OSI-approved and SignPath Foundation requires OSI licenses without commercial dual-licensing.
- Desktop is a lite file manager: WinRAR-style toolbar (Add, Extract to, Test, View, Delete, Info) with Windows Explorer intuition: breadcrumb bar, sidebar tree, sortable details list, multi-select, familiar shortcuts, drag and drop, own context menu, archives opened as folders. Out of v1: search, thumbnails, tabs, network.
- Progress dialog appears only if the operation lasts more than about 500 ms.
- The GUI is built only against mockups approved by the owner in Phase 0. Do not invent the design.

## 7. Android
Full app with its own file browser. Full file access as the main mode; SAF for external entries (Open with, Share) and cloud. No internet permission. Built with cargo-apk. A minimal Java/Kotlin fragment may be needed for the on-screen keyboard. Parity goals: browse contents, partial extraction, passwords, edit archives (add/remove), multi-part archives, open small files without extracting to a temp folder, multithreading.

## 8. Distribution, CI/CD, signing
- Distribution: GitHub Releases only. No Google Play, no Microsoft Store, no Google developer registration. The README documents Android installation (Google's advanced flow or ADB), after Phase 0 test 9 verifies what that flow offers.
- On every pull request: fmt, clippy, tests on the three OSes, cargo-deny, short fuzz run.
- Releases (see ADR 0003): after CI passes on a push to main, the Release workflow runs. Without "[release]" in the commit message it updates one rolling pre-release `nightly`. With "[release]" it publishes `vX.Y.Z` (the `version` in Cargo.toml) directly, no drafts, and fails if that tag exists; a version with a hyphen (-rc.1, -beta.1) is a pre-release, otherwise stable and Latest. Pull requests never release. Every release has SHA256SUMS, a `--version` smoke test per binary and generated notes. Signing and attestations join this pipeline when their accounts are approved. Rehearse with -rc versions first.
- Signing: SignPath Foundation on Windows (MSIX needs two signatures), Apple Developer on macOS, own key stored as a secret on Android. The pipeline must work unsigned until SignPath approves the project.
- Practices: Cargo.lock committed, --locked everywhere, actions pinned by hash, secrets unavailable to forks, protected release environment.
- Repo must contain LICENSE, third-party notices (cargo-deny), README with Slint attribution and SignPath credit, SECURITY.md, CONTRIBUTING.md.

## 9. Quality bar (definition of done for every phase)
- Acceptance criteria and measurements of the phase prompt are met and reported with numbers.
- Cross-tests: what Zstar creates, 7-Zip and libarchive extract, and the reverse.
- Fuzzing and a security review for every new format, not only at the end.
- Benchmarks run in CI and flag regressions.
- Warnings are errors; clippy clean; no unsafe without a comment explaining why it is sound.
- Every closed or newly made decision is recorded as a short ADR in docs/adr/.

## 10. How to work
- One phase at a time, driven by the phase prompts in docs/prompts/. Do not start the next phase until the current one meets its criteria.
- Before adding or upgrading any dependency, verify its latest version, license (must pass cargo-deny and be GPL-3.0-only compatible) and maintenance status from the official source. Cite the source. State clearly what you could not verify. Never invent versions or APIs.
- Pin exact versions in DEPENDENCIES.md and Cargo.toml.
- Separate facts from your own reasoning in reports.
- Put phase detail in docs/prompts/, not in this file.
- Never commit secrets. Never weaken a security or license rule to make something pass.
