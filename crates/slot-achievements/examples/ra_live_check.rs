//! Read-only live check against retroachievements.org, for the separate RetroAchievements
//! test account (0.6.0 Phase 3). Run by the `ra-live-check` workflow, never on the
//! handheld.
//!
//! It logs in with the password (once, from the environment), then reads the test games'
//! achievement sets and starts a session for each. It **never sends an unlock**: there is
//! no award request anywhere in this program. Live unlocks only ever come from real play.
//!
//! Environment:
//! - `RA_TEST_USERNAME`, `RA_TEST_PASSWORD`: the test account (GitHub secrets). The
//!   password is sent once over TLS; the token it returns is never printed.
//! - `RA_GAMES`: comma-separated `id:<game id>` or ROM hashes, e.g. `id:510,id:785`.
//! - `RA_FOCUS`: comma-separated achievement titles whose logic to print (for choosing
//!   test achievements); everything else is summarised.
//! - `GITHUB_STEP_SUMMARY`: if set, the report is also written there.

use std::fmt::Write as _;
use std::sync::Arc;

use slot_achievements::api::{self, Call, Outcome, Reply};
use slot_net::web::{self, Site};

fn send(site: &Site, call: &Call) -> Result<Reply, String> {
    assert!(
        call.kind() != "awardachievement",
        "this tool never sends unlocks"
    );
    if call.host() != api::HOST {
        return Err(format!("refused a request to {}", call.host()));
    }
    let clock: web::Clock = Arc::new(|| web::unix_time(slot_achievements::store::time::wall_ms()));
    let body = call.post.as_deref().map(|p| {
        (
            call.content_type
                .as_deref()
                .unwrap_or("application/x-www-form-urlencoded"),
            p,
        )
    });
    site.request(call.path_and_query(), body, clock)
        .map(|r| Reply {
            status: r.status,
            body: r.body,
        })
        .map_err(|e| e.to_string())
}

fn main() {
    let user = std::env::var("RA_TEST_USERNAME").expect("RA_TEST_USERNAME is not set");
    let password = std::env::var("RA_TEST_PASSWORD").expect("RA_TEST_PASSWORD is not set");
    let games = std::env::var("RA_GAMES").unwrap_or_else(|_| "id:510,id:785".into());
    let focus: Vec<String> = std::env::var("RA_FOCUS")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    let version = format!("{}-live-check", env!("CARGO_PKG_VERSION"));
    let site = Site::new(api::HOST, &api::user_agent(&version));
    let mut out = String::new();
    let _ = writeln!(
        out,
        "## slot.advance RetroAchievements live check (read-only)\n"
    );
    let _ = writeln!(out, "- User agent: `{}`", site.user_agent);

    // 1. Log in with the password: TLS to the real certificate, rcheevos' request and parser.
    let call = api::login_password(&user, &password).expect("request");
    drop(password);
    let reply = match send(&site, &call) {
        Ok(r) => r,
        Err(e) => {
            let _ = writeln!(out, "- **Log in: could not reach the server**: {e}");
            finish(&out);
            std::process::exit(1);
        }
    };
    let login = api::parse_login(&reply);
    let (canon, token) = match (api::classify(&login.verdict), login.user, login.token) {
        (Outcome::Ok, Some(u), Some(t)) => (u, t),
        (o, _, _) => {
            let _ = writeln!(
                out,
                "- **Log in failed**: {o:?} (HTTP {}, {:?})",
                login.verdict.http, login.verdict.message
            );
            finish(&out);
            std::process::exit(1);
        }
    };
    let _ = writeln!(
        out,
        "- Log in: OK (HTTP {}; the account name and token are not shown: these logs are public)",
        reply.status
    );

    // 2. Renewing with the token, as the handheld does once a boot.
    match send(&site, &api::login_token(&canon, &token).unwrap()) {
        Ok(r) => {
            let l = api::parse_login(&r);
            let same = l.token.as_deref() == Some(token.as_str());
            let _ = writeln!(
                out,
                "- Token renewal: {:?} (token {})",
                api::classify(&l.verdict),
                if same { "unchanged" } else { "changed" }
            );
        }
        Err(e) => {
            let _ = writeln!(out, "- Token renewal: failed: {e}");
        }
    }

    // 3. Each game's sets and a session.
    for g in games.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let call = match g.strip_prefix("id:") {
            Some(id) => api::game_sets_by_id(&canon, &token, id.parse().expect("game id")),
            None => api::game_sets(&canon, &token, g),
        }
        .expect("request");
        let _ = writeln!(out, "\n### `{g}`\n");
        let reply = match send(&site, &call) {
            Ok(r) => r,
            Err(e) => {
                let _ = writeln!(out, "- Sets: could not reach the server: {e}");
                continue;
            }
        };
        let (verdict, game) = api::parse_game_sets(&reply);
        let Some(game) = game else {
            let _ = writeln!(
                out,
                "- Sets: {:?} (HTTP {}, {:?}){}",
                api::classify(&verdict),
                verdict.http,
                verdict.message,
                if g.starts_with("id:") {
                    ""
                } else {
                    ": this ROM hash is not known to RetroAchievements"
                }
            );
            continue;
        };
        let core: Vec<&api::Remote> = game
            .achievements
            .iter()
            .filter(|a| a.set_type == api::SET_TYPE_CORE && a.category == api::CATEGORY_CORE)
            .collect();
        let _ = writeln!(
            out,
            "- {} (game {}, console {}, session game {}): {} core achievements, {} in all sets (HTTP {}, {} bytes)",
            game.title,
            game.id,
            game.console_id,
            game.session_game_id,
            core.len(),
            game.achievements.len(),
            reply.status,
            reply.body.len()
        );
        // Do the definitions parse in rcheevos' runtime, as the handheld will load them?
        let set = game.game_set(&[]);
        let mut eval = slot_achievements::Evaluator::new(set.console_id);
        let loaded = eval.load(&set);
        let _ = writeln!(
            out,
            "- rcheevos loads {} of them; {} failed to parse",
            loaded.activated.len(),
            loaded.failed.len()
        );
        let counters: Vec<&&api::Remote> = core
            .iter()
            .filter(|a| a.definition.contains("M:") || has_hits(&a.definition))
            .collect();
        let _ = writeln!(
            out,
            "- With a hit count or measured value (partial progress): {}",
            counters.len()
        );
        for a in &core {
            if focus.iter().any(|f| a.title.to_lowercase() == *f) {
                let _ = writeln!(
                    out,
                    "  - **{}** ({}, {} pts): {}\n    - logic: `{}`",
                    a.title, a.id, a.points, a.description, a.definition
                );
            }
        }
        match send(
            &site,
            &api::start_session(&canon, &token, game.session_game_id, "").unwrap(),
        ) {
            Ok(r) => {
                let (v, ids) = api::parse_session(&r);
                let _ = writeln!(
                    out,
                    "- Session start: {:?}, {} unlocks already on this account",
                    api::classify(&v),
                    ids.len()
                );
            }
            Err(e) => {
                let _ = writeln!(out, "- Session start: failed: {e}");
            }
        }
    }
    drop(token);
    finish(&out);
}

/// A condition with a hit count target, like `0xH1234=1.10.`.
fn has_hits(def: &str) -> bool {
    def.split('_').any(|c| {
        let parts: Vec<&str> = c.split('.').collect();
        parts.len() >= 3 && parts[parts.len() - 2].parse::<u32>().is_ok_and(|n| n > 1)
    })
}

fn finish(out: &str) {
    println!("{out}");
    if let Ok(p) = std::env::var("GITHUB_STEP_SUMMARY") {
        let _ = std::fs::write(p, out);
    }
}
