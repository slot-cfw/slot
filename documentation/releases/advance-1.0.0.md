slot.advance 1.0.0 for the Anbernic RG SP (2026) (720 x 480). It is not for the RG35XX SP. The previous release is [slot.advance 0.5.0](https://github.com/slowphase/slot.advance/releases/tag/advance-0.5.0).

**Real sleep. Instant resume. Super-efficient standby.** And now RetroAchievements, online and offline, when you want it. It stays off unless you switch it on, and everything else, sleep, wake, Wi-Fi and Syncthing included, works as it did on 0.5.0.

**Upgrading:** download `slot.advance-1.0.0-rg-sp.zip`, unzip it, and copy the contents of its `System` folder into `System` on your games card (SD2), as in the README's [upgrade steps](https://github.com/slowphase/slot.advance/blob/advance-1.0.0/README.md#installing-and-updating). Your games, saves, states and settings are not touched. **New install:** follow the README's [new install steps](https://github.com/slowphase/slot.advance/blob/advance-1.0.0/README.md#new-install).

### RetroAchievements
Open **Settings > RetroAchievements**, switch it on and log in with your [RetroAchievements](https://retroachievements.org) user name and password (Wi-Fi needs to be on and connected). Your password is sent once and never stored. Achievements are Softcore, on mGBA, for GBA, Game Boy and Game Boy Color games, so save states, rewind and fast-forward all stay available. When you earn one, a one-line notice appears at the top of the screen.

**Start a game online once.** The first time you play a game with achievements, slot.advance fetches its achievements from RetroAchievements, so Wi-Fi must be connected. After that it keeps them on the card and the game earns achievements offline too. Switching RetroAchievements on, or logging in, while a game is running doesn't track that game: tracking starts with the next game you start. [How it works](https://github.com/slowphase/slot.advance/blob/advance-1.0.0/documentation/user-guide.md#retroachievements).

### Offline, and safe
Achievements you earn offline are written to the card before the notice appears, kept through sleep, the timed shutdown and Power Off, and sent automatically the next time Wi-Fi connects, dated when you earned them where that can be traced (RetroAchievements accepts up to 14 days back). Progress toward an achievement carries on after sleep and after the game resumes at power-on. Log Out keeps your unsent achievements on the card, and each account's records are kept apart.

RetroAchievements never runs while the RG SP sleeps, never delays sleep or saving, never switches Wi-Fi on, and does nothing at all while it's off.

### Battery
Asleep, nothing changes: 1.0.0 keeps 0.5.0's hardware sleep, which used about 2% of the battery an hour in two 8-hour sleeps on 0.5.0. While playing, RetroAchievements used an estimated 9 mA more (1.8%) in one matched test, within measurement noise. [The measurements](https://github.com/slowphase/slot.advance/blob/advance-1.0.0/documentation/sleep-and-battery.md#100-retroachievements).

### Good to know
- No Hardcore mode, leaderboards or achievement list on the handheld, and no tracking on gpSP or during link play.
- Achievements sent more than 14 days after you earned them are dated 14 days before sending, RetroAchievements' limit.
- Your login token is saved on the games card unencrypted (not your password), so look after the card.
- The limitations of 0.5.0 still apply, listed in the [release notes](https://github.com/slowphase/slot.advance/blob/advance-1.0.0/documentation/release-notes.md#050-superseded-by-100).

If anything goes wrong, copy `slot.log` and `slot.log.1` (and, if there is one, the `crash` folder's `cores.log` and newest `slot-….log`) from the games card before powering on again, and [open a bug report](https://github.com/slowphase/slot.advance/issues/new/choose) with the files attached. Never attach `Config/retroachievements/account.txt`: it holds your login token.

Emulation by mGBA (MPL-2.0) and gpSP (GPL-2.0); Syncthing 2.1.6 (MPL-2.0) and rcheevos 12.5.0 (MIT), unmodified; Mozilla's root certificates through webpki-roots 1.0.11 (CDLA-Permissive-2.0, derived from the Common CA Database). Achievements by [RetroAchievements](https://retroachievements.org); the design builds on @pvaibhav's [slot fork](https://github.com/pvaibhav/slot). Licences and patches, gpSP's source, and where to get mGBA's and Syncthing's source are in `System/licenses/`. slot.advance is GPL-3.0, based on [slot](https://github.com/BrandonKowalski/slot) by Brandon T. Kowalski. [Full release notes](https://github.com/slowphase/slot.advance/blob/advance-1.0.0/documentation/release-notes.md) · [User guide](https://github.com/slowphase/slot.advance/blob/advance-1.0.0/documentation/user-guide.md).
