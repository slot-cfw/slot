# slotadvance.com

The slot.advance website: one static page, served by Cloudflare (Workers with static assets).

| File | What it is |
|---|---|
| `public/index.html` | The page. |
| `public/logo.png` | The striped slot.advance logo, cut from `artwork/bootlogo/slot-advance-logo-source.png` on a transparent background. |
| `public/og-1.0.0.jpg` | Link-preview image (1200 x 630), a centred crop of the README's 1.0.0 hero (`.github/resources/slot-advance-hero-1.0.0.webp`). |
| `public/battery-standby-1.0.0.svg` | The battery chart, a copy of `.github/resources/battery/battery-standby-1.0.0.svg` (as in the README): semi-stock software standby against 1.0.0 hardware sleep. |
| `public/favicon.svg` | Browser-tab icon: the six logo stripes. |
| `public/slot-advance-1.0.0.mp4` | The 1.0.0 video, played on the page: H.264 High 1080p at 30 fps, 61.0 s, about 9.9 MB (Cloudflare's limit is 25 MiB per file), with fast start. Version-free picture: the 0.5.0 teaser's opening, sleep and Syncthing beats, with RetroAchievements added (unlock notices drawn by the firmware's own notice code, illustrative titles verified on RetroAchievements) and the tagline Real Sleep. Real Saves. Real Achievements. Audio: the same soundtrack as r3 with the same single gain (-2.73 dB, no compression or limiting), measured -13.0 LUFS integrated and -2.0 dBFS peak, AAC-LC 320 kbps stereo. A new video gets a new file name, so browsers never show a cached old one. |
| `public/video-poster-1.0.0.jpg` | The video's opening card at 5 s, shown before it plays. |
| `wrangler.jsonc` | Cloudflare settings: serve everything in `public/`. |

Cloudflare builds from this folder (root directory `website`). A change pushed to `main` goes live automatically once the repository is connected.
