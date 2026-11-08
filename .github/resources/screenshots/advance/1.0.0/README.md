# 1.0.0 screenshots

Every image here is drawn by slot.advance's own code at the RG SP (2026)'s 720 x 480, through the real app, menu logic and compositor, with headless rendering. None is a mock-up or an edited image. They were rendered from `release-0.6.0` at `08261fd`, the code tested on the handheld in Phase 6.

The account name ("Player1"), the counts and the achievement ("Hall of Fame (10 points)") are invented examples set in the generator. They are not a real account or taken from any real achievement set. The RetroAchievements screens are given the status the app would show; no server, account or network is involved in drawing them. The game picture behind the unlock notice is the user-supplied Pokémon Unbound Hall of Fame image (see `../sources/README.md`), shown as a still game frame.

| Image | Shows | Made by |
|---|---|---|
| `settings.png` | Settings on the carousel: Network and RetroAchievements On, RetroAchievements selected | `readme_100_screenshots` |
| `in-game-settings.png` | Settings in a game: Date & Time hidden, RetroAchievements after Network | `render_in_game_menu` (`in-game-06-settings-in-game.png`) |
| `retroachievements-off.png` | The RetroAchievements screen, Off (the default): Account greyed, no status | `readme_100_screenshots` |
| `retroachievements-user-name.png` | Log In, step one: the user name keyboard | `readme_100_screenshots` |
| `retroachievements-logged-in.png` | Logged in: "Achievements are tracked from the next game started." | `readme_100_screenshots` |
| `retroachievements-up-to-date.png` | On and logged in: Up to date | `readme_100_screenshots` |
| `retroachievements-waiting.png` | Two achievements waiting to sync, sent when Wi-Fi connects | `readme_100_screenshots` |
| `retroachievements-log-out.png` | The Log Out question, saying the two are kept | `readme_100_screenshots` |
| `achievement-notice.png` | The one-line unlock notice in the top bar, over a game | `readme_100_screenshots` |

## Unchanged from 0.5.0

The carousel, Network, Networks, Web Interface and in-game menu images in `../0.5.0/` were rendered again from `08261fd` with `readme_050_screenshots` and `render_in_game_menu` and compared pixel by pixel with the 0.5.0 copies: all identical. They stay where they are and are used for 1.0.0. Only 0.5.0's `in-game-settings.png` changed (the new RetroAchievements row); the 1.0.0 version is here, and the 0.5.0 copy is kept as the record of that release.

## Regenerating

From the repository root:

```
cargo run -p slot --no-default-features --example readme_100_screenshots
SCRATCH_PNG_DIR=/tmp/in-game cargo test -p slot --no-default-features --test render_in_game_menu
```

The second writes `in-game-06-settings-in-game.png`, copied here as `in-game-settings.png`. On Linux without a display, set `EGL_PLATFORM=surfaceless` for both. The carousel Settings image shows the clock at the moment it is rendered. Images rendered with Mesa on Linux and on macOS can differ by a level or two in a few anti-aliased pixels (out of 255); nothing visible.
