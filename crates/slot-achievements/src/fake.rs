//! Server answers shaped like retroachievements.org's (for tests only, feature
//! `test-support`): the field names rcheevos' `rapi` reads, and the error shapes RAWeb's
//! `BaseApiAction` and `AwardAchievementAction` send (checked against RAWeb's source).

/// One achievement in a set.
#[derive(Clone, Debug)]
pub struct Def {
    pub id: u32,
    pub title: &'static str,
    pub mem: String,
    pub points: u32,
    /// 3 core, 5 unofficial.
    pub flags: u32,
}

pub fn def(id: u32, mem: &str) -> Def {
    Def {
        id,
        title: "Test",
        mem: mem.to_string(),
        points: 5,
        flags: 3,
    }
}

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// `r=achievementsets`: one core set.
pub fn game_sets(game_id: u32, console_id: u32, title: &str, defs: &[Def]) -> String {
    let achievements: Vec<String> = defs
        .iter()
        .map(|d| {
            format!(
                r#"{{"ID":{},"Title":"{}","Description":"Do the thing","Flags":{},"Points":{},"MemAddr":"{}","Author":"tester","BadgeName":"00000","Created":1700000000,"Modified":1700000000,"Type":"","Rarity":50.0,"RarityHardcore":25.0,"BadgeURL":"","BadgeLockedURL":""}}"#,
                d.id,
                esc(d.title),
                d.flags,
                d.points,
                esc(&d.mem)
            )
        })
        .collect();
    format!(
        r#"{{"Success":true,"GameId":{game_id},"Title":"{}","ConsoleId":{console_id},"ImageIconUrl":"","RichPresenceGameId":{game_id},"RichPresencePatch":"","Sets":[{{"AchievementSetId":{},"GameId":{game_id},"Title":null,"Type":"core","ImageIconUrl":"","Achievements":[{}],"Leaderboards":[]}}]}}"#,
        esc(title),
        game_id + 100_000,
        achievements.join(",")
    )
}

/// `r=login2` accepted.
pub fn login_ok(user: &str, token: &str) -> String {
    format!(
        r#"{{"Success":true,"User":"{user}","AvatarUrl":"","Token":"{token}","Score":0,"SoftcoreScore":0,"Messages":0,"Permissions":1,"AccountType":"Registered"}}"#
    )
}

/// `r=startsession` with the account's unlocks.
pub fn session(softcore: &[u32], hardcore: &[u32]) -> String {
    let list = |ids: &[u32]| {
        ids.iter()
            .map(|i| format!(r#"{{"ID":{i},"When":1700000000}}"#))
            .collect::<Vec<_>>()
            .join(",")
    };
    format!(
        r#"{{"Success":true,"HardcoreUnlocks":[{}],"Unlocks":[{}],"ServerNow":1794000000}}"#,
        list(hardcore),
        list(softcore)
    )
}

/// `r=awardachievement` accepted.
pub fn award_ok(id: u32) -> String {
    format!(
        r#"{{"Success":true,"Score":10,"SoftcoreScore":25,"AchievementID":{id},"AchievementsRemaining":7}}"#
    )
}

/// The account already had it (RAWeb sends `Success: false` when nothing else unlocked).
pub fn award_already(id: u32) -> String {
    format!(
        r#"{{"Success":false,"Error":"User already has this achievement unlocked.","Score":10,"SoftcoreScore":25,"AchievementID":{id},"AchievementsRemaining":7}}"#
    )
}

/// An error in RAWeb's shape (`BaseApiAction`): `(status, body)`.
pub fn error(status: u16, code: &str, message: &str) -> (u16, String) {
    (
        status,
        format!(r#"{{"Success":false,"Status":{status},"Code":"{code}","Error":"{message}"}}"#),
    )
}

pub fn not_found() -> (u16, String) {
    error(404, "not_found", "Unknown achievement.")
}

pub fn invalid_credentials() -> (u16, String) {
    error(
        401,
        "invalid_credentials",
        "Invalid user/password combination. Please try again.",
    )
}

pub fn expired_token() -> (u16, String) {
    error(
        401,
        "expired_token",
        "The access token has expired. Please log in again.",
    )
}

pub fn access_denied(message: &str) -> (u16, String) {
    error(403, "access_denied", message)
}

pub fn unsupported_client() -> (u16, String) {
    error(403, "unsupported_client", "This client is not supported.")
}

pub fn missing_parameter() -> (u16, String) {
    error(
        422,
        "missing_parameter",
        "One or more required parameters is missing.",
    )
}

/// Cloudflare or nginx in front of the server.
pub fn gateway(status: u16) -> (u16, String) {
    (
        status,
        format!("<html><head><title>{status}</title></head><body>Bad gateway</body></html>"),
    )
}
