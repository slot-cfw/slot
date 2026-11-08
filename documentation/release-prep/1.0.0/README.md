# slot.advance 1.0.0: release preparation

*Working notes for the `docs/1.0.0-release-prep` branch (Phase 7). Not user documentation. Remove this folder before merging into `main`, or keep it as a record, as for 0.5.0.*

**Status (8 November 2026):** documentation prepared against `release-0.6.0` at `08261fd`, the code tested on the handheld in Phase 6 (Checkpoint E approved, Phase 6 passed). Nothing merged, tagged, released, deployed or published. The version is still 0.5.0 in `Cargo.toml`.

## What this branch contains

`release-0.6.0` (`08261fd`) plus documentation, screenshots, charts and their generators. No firmware, card script, workflow or website change: `git diff 08261fd <this branch>` lists only `README.md`, `documentation/`, `.github/resources/` and `crates/slot/examples/readme_100_screenshots.rs` (an example, not built into the release).

| Area | Files |
|---|---|
| README | `README.md`: one RetroAchievements feature bullet, the two-bar battery chart and its paragraph, the 1.0.0 version and download links, the RetroAchievements and rcheevos credit |
| User guide | `documentation/user-guide.md`: a RetroAchievements section, plus Settings, files, going back, troubleshooting, limitations, reporting and the comparison with slot |
| Technical reference | `documentation/technical.md`: a RetroAchievements section, plus the intro, architecture, lid close, Power Off and Restart, security, performance, logging, limitations and source map |
| Sleep and battery | `documentation/sleep-and-battery.md` (a 1.0.0 section and validation status; history unchanged), `documentation/battery-tests/` (`charts.py`, new `calculate-1.0.0.py`) |
| Release notes | `documentation/release-notes.md` (1.0.0 section, version-number note) |
| GitHub Release text | `documentation/releases/advance-1.0.0.md`. The release workflow publishes this file **from the tagged commit**, verbatim |
| Charts | `.github/resources/battery/battery-standby-1.0.0.svg` (README) and `battery-awake-ra-1.0.0.svg` (sleep and battery). Every earlier chart regenerates byte for byte unchanged |
| Screenshots | `.github/resources/screenshots/advance/1.0.0/` (with a README on how each was made) |

## How the battery evidence is presented

- **The README's chart** compares semi-stock software standby (~7% an hour, Test B) with slot.advance 1.0.0 (~2% an hour). The 1.0.0 bar is not labelled projected or estimated; a note on the chart and in the text says it comes from the two extended 8-hour 0.5.0 sleeps (V1-B, V1-D) of the same hardware sleep implementation. Nothing says a sleep was measured on 1.0.0.
- **The 0.4.x and 0.5.0 history is unchanged** in `sleep-and-battery.md`, and the earlier README chart (`battery-drain-hourly.svg`) is still generated, unchanged, as the release-by-release record.
- **RetroAchievements while playing:** 494 against 503 mA, +9 mA (+1.8%), standard error 6, within measurement noise; both runs Wi-Fi On, 80% to 71%; described as the observed difference, not a maximum; not compared with 0.5.0's figures; plainly an active-play measurement.

## Not claimed anywhere

- Partial progress across a cold-boot resume on the handheld (host tests only; stated as a limitation).
- Any sleep or standby measurement on a 1.0.0 build.
- A same-night or Wi-Fi Off RetroAchievements battery pair.

## Release checklist

Each step needs Dean's explicit approval at the release checkpoint.

1. **Review** this branch (with ChatGPT if wanted).
2. **Version:** "Set the version to 1.0.0" on `release-0.6.0`: `Cargo.toml` `[workspace.package] version` and the workspace crates in `Cargo.lock`, nothing else. The user agent then reads `slot.advance/1.0.0`. Host tests, fmt and clippy at the result.
3. **Docs into the release branch:** merge `docs/1.0.0-release-prep` into `release-0.6.0`, so the tagged commit carries `documentation/releases/advance-1.0.0.md`.
4. **Check the tagged tree:** `Cargo.toml` says 1.0.0; `git diff 08261fd <commit> -- crates cores card scripts .github/workflows taskfile.yml` shows only the version lines and `crates/slot/examples/readme_100_screenshots.rs`.
5. **Tag and publish:** tag `advance-1.0.0` (or run the release workflow by hand with `release_tag` `advance-1.0.0`). The README's download link (`releases/download/advance-1.0.0/slot.advance-1.0.0-rg-sp.zip`) works only once this is published.
6. **After publishing:** download the asset through the README's link, check the top folder `slot.advance-1.0.0-rg-sp` and that `System/licenses/` holds `rcheevos-MIT.txt`; note the asset's SHA-256.
7. **main:** merge `release-0.6.0` into `main` (separate approval).
8. **Website:** slotadvance.com still shows the 0.5.0 battery blocks and copy (separate approval).
