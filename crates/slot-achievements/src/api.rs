//! RetroAchievements requests and responses, built and parsed by rcheevos' own `rapi` (D1),
//! and what each server answer means for us (design/v6-ra-architecture.md section 8.2).
//!
//! The transport is not here: callers send [`Call`]s with slot-net's HTTPS client.

use std::ffi::{c_char, c_int, c_void, CStr, CString};

use crate::{Achievement, GameSet};

/// What the server sees us as (architecture 10.4). Never upstream slot's, the fork's or
/// RetroArch's.
pub fn user_agent(version: &str) -> String {
    format!(
        "slot.advance/{version} (Linux aarch64) rcheevos/{}",
        crate::RCHEEVOS_VERSION
    )
}

pub const HOST: &str = "retroachievements.org";

/// rcheevos' result codes (rc_error.h) that matter here.
pub mod rc {
    pub const OK: i32 = 0;
    pub const INVALID_JSON: i32 = -26;
    pub const API_FAILURE: i32 = -27;
    pub const NO_RESPONSE: i32 = -32;
    pub const ACCESS_DENIED: i32 = -33;
    pub const INVALID_CREDENTIALS: i32 = -34;
    pub const EXPIRED_TOKEN: i32 = -35;
    pub const NOT_FOUND: i32 = -39;
}

#[repr(C)]
struct RawReq {
    url: *mut c_char,
    post: *mut c_char,
    content_type: *mut c_char,
}

#[repr(C)]
struct RawStatus {
    result: c_int,
    succeeded: c_int,
    error_message: *mut c_char,
    error_code: *mut c_char,
}

#[repr(C)]
struct RawGame {
    id: u32,
    console_id: u32,
    session_game_id: u32,
    title: *mut c_char,
}

type UnlockFn = extern "C" fn(*mut c_void, u32, i64);
type AchievementFn =
    extern "C" fn(*mut c_void, u8, u32, u32, u32, *const c_char, *const c_char, *const c_char);

extern "C" {
    fn slot_ra_free(p: *mut c_void);
    fn slot_ra_req_free(r: *mut RawReq);
    fn slot_ra_login_req(
        user: *const c_char,
        password: *const c_char,
        token: *const c_char,
        out: *mut RawReq,
    ) -> c_int;
    fn slot_ra_game_sets_req(
        user: *const c_char,
        token: *const c_char,
        hash: *const c_char,
        out: *mut RawReq,
    ) -> c_int;
    fn slot_ra_game_sets_id_req(
        user: *const c_char,
        token: *const c_char,
        game_id: u32,
        out: *mut RawReq,
    ) -> c_int;
    fn slot_ra_start_session_req(
        user: *const c_char,
        token: *const c_char,
        game_id: u32,
        hash: *const c_char,
        out: *mut RawReq,
    ) -> c_int;
    fn slot_ra_award_req(
        user: *const c_char,
        token: *const c_char,
        id: u32,
        hash: *const c_char,
        seconds: u32,
        out: *mut RawReq,
    ) -> c_int;
    fn slot_ra_login_resp(
        body: *const c_char,
        len: usize,
        http: c_int,
        st: *mut RawStatus,
        user: *mut *mut c_char,
        token: *mut *mut c_char,
    ) -> c_int;
    fn slot_ra_award_resp(
        body: *const c_char,
        len: usize,
        http: c_int,
        st: *mut RawStatus,
        awarded: *mut u32,
    ) -> c_int;
    fn slot_ra_start_session_resp(
        body: *const c_char,
        len: usize,
        http: c_int,
        st: *mut RawStatus,
        ctx: *mut c_void,
        each: UnlockFn,
    ) -> c_int;
    fn slot_ra_game_sets_resp(
        body: *const c_char,
        len: usize,
        http: c_int,
        st: *mut RawStatus,
        game: *mut RawGame,
        ctx: *mut c_void,
        each: AchievementFn,
    ) -> c_int;
}

/// Takes ownership of a malloc'd C string from the bridge.
fn take(p: *mut c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    // SAFETY: the bridge returns NUL-terminated strings it malloc'd; freed once, here.
    let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
    unsafe { slot_ra_free(p as *mut c_void) };
    Some(s)
}

fn borrow(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: rcheevos passes NUL-terminated strings valid for the callback.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

fn c(s: &str) -> Result<CString, String> {
    CString::new(s).map_err(|_| "a value contains a nul".to_string())
}

/// One request, as rcheevos built it.
#[derive(Clone, PartialEq, Eq)]
pub struct Call {
    pub url: String,
    pub post: Option<String>,
    pub content_type: Option<String>,
}

impl std::fmt::Debug for Call {
    // Post data carries the password or token: never printed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Call")
            .field("url", &self.url)
            .field("post", &self.post.as_ref().map(|_| "(hidden)"))
            .finish()
    }
}

impl Call {
    /// The path and query (`/dorequest.php...`) to send to [`HOST`].
    pub fn path_and_query(&self) -> &str {
        let rest = self
            .url
            .split_once("://")
            .map(|(_, r)| r)
            .unwrap_or(&self.url);
        match rest.find('/') {
            Some(i) => &rest[i..],
            None => "/",
        }
    }

    /// The host rcheevos addressed (always retroachievements.org here).
    pub fn host(&self) -> &str {
        let rest = self
            .url
            .split_once("://")
            .map(|(_, r)| r)
            .unwrap_or(&self.url);
        rest.split('/').next().unwrap_or("")
    }

    /// The `r=` request kind, for logs.
    pub fn kind(&self) -> &str {
        let q = self.post.as_deref().unwrap_or("");
        q.split('&')
            .find_map(|p| p.strip_prefix("r="))
            .unwrap_or("?")
    }
}

fn build(f: impl FnOnce(*mut RawReq) -> c_int) -> Result<Call, String> {
    let mut raw = RawReq {
        url: std::ptr::null_mut(),
        post: std::ptr::null_mut(),
        content_type: std::ptr::null_mut(),
    };
    let r = f(&mut raw);
    // SAFETY: the bridge filled `raw` (or left it null); copied, then freed once.
    let call = unsafe {
        let url =
            (!raw.url.is_null()).then(|| CStr::from_ptr(raw.url).to_string_lossy().into_owned());
        let post =
            (!raw.post.is_null()).then(|| CStr::from_ptr(raw.post).to_string_lossy().into_owned());
        let content_type = (!raw.content_type.is_null()).then(|| {
            CStr::from_ptr(raw.content_type)
                .to_string_lossy()
                .into_owned()
        });
        slot_ra_req_free(&mut raw);
        (url, post, content_type)
    };
    match (r, call) {
        (0, (Some(url), post, content_type)) => Ok(Call {
            url,
            post,
            content_type,
        }),
        (r, _) => Err(format!("rcheevos could not build the request ({r})")),
    }
}

/// Log In with a password (used once, then dropped by the caller).
pub fn login_password(user: &str, password: &str) -> Result<Call, String> {
    let (u, p) = (c(user)?, c(password)?);
    build(|out| unsafe { slot_ra_login_req(u.as_ptr(), p.as_ptr(), std::ptr::null(), out) })
}

/// Log In with the connect token (renews it).
pub fn login_token(user: &str, token: &str) -> Result<Call, String> {
    let (u, t) = (c(user)?, c(token)?);
    build(|out| unsafe { slot_ra_login_req(u.as_ptr(), std::ptr::null(), t.as_ptr(), out) })
}

/// The game's achievement sets, by ROM hash.
pub fn game_sets(user: &str, token: &str, rom_hash: &str) -> Result<Call, String> {
    let (u, t, h) = (c(user)?, c(token)?, c(rom_hash)?);
    build(|out| unsafe { slot_ra_game_sets_req(u.as_ptr(), t.as_ptr(), h.as_ptr(), out) })
}

/// The game's achievement sets, by RetroAchievements game ID (for the live check tool;
/// slot.advance itself always asks by ROM hash).
pub fn game_sets_by_id(user: &str, token: &str, game_id: u32) -> Result<Call, String> {
    let (u, t) = (c(user)?, c(token)?);
    build(|out| unsafe { slot_ra_game_sets_id_req(u.as_ptr(), t.as_ptr(), game_id, out) })
}

/// Start a session (Softcore); answers with the account's unlocks for the game.
pub fn start_session(
    user: &str,
    token: &str,
    game_id: u32,
    rom_hash: &str,
) -> Result<Call, String> {
    let (u, t, h) = (c(user)?, c(token)?, c(rom_hash)?);
    build(|out| unsafe {
        slot_ra_start_session_req(u.as_ptr(), t.as_ptr(), game_id, h.as_ptr(), out)
    })
}

/// Award an achievement (Softcore), earned `seconds` ago (0: just now). The caller has
/// already clamped `seconds` to 14 days.
pub fn award(
    user: &str,
    token: &str,
    id: u32,
    rom_hash: &str,
    seconds: u32,
) -> Result<Call, String> {
    let (u, t, h) = (c(user)?, c(token)?, c(rom_hash)?);
    build(|out| unsafe { slot_ra_award_req(u.as_ptr(), t.as_ptr(), id, h.as_ptr(), seconds, out) })
}

/// An HTTP answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reply {
    pub status: u16,
    pub body: String,
}

/// The server's verdict, as rcheevos read it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    pub http: u16,
    pub result: i32,
    pub succeeded: bool,
    pub message: Option<String>,
    pub code: Option<String>,
}

fn status_of(raw: RawStatus, http: u16, body: &str) -> Verdict {
    let code = take(raw.error_code).or_else(|| server_code(body));
    Verdict {
        http,
        result: raw.result,
        succeeded: raw.succeeded != 0,
        message: take(raw.error_message),
        code,
    }
}

/// RAWeb's `"Code"` field, for the answers whose rcheevos parser does not read it (the
/// award answer, for one). Only a plain lowercase word is taken.
fn server_code(body: &str) -> Option<String> {
    let rest = &body[body.find("\"Code\":\"")? + 8..];
    let code = &rest[..rest.find('"')?];
    (!code.is_empty()
        && code.len() <= 40
        && code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'))
    .then(|| code.to_string())
}

fn empty_status() -> RawStatus {
    RawStatus {
        result: 0,
        succeeded: 0,
        error_message: std::ptr::null_mut(),
        error_code: std::ptr::null_mut(),
    }
}

/// A body rcheevos can read: NUL-free (a NUL ends it early, which only makes it fail to
/// parse).
fn body(reply: &Reply) -> CString {
    CString::new(reply.body.replace('\0', "")).expect("nul removed")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoginAnswer {
    pub verdict: Verdict,
    /// The user name as the server spells it.
    pub user: Option<String>,
    pub token: Option<String>,
}

impl std::fmt::Display for LoginAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?} user {:?} (token hidden)", self.verdict, self.user)
    }
}

pub fn parse_login(reply: &Reply) -> LoginAnswer {
    let b = body(reply);
    let mut st = empty_status();
    let (mut u, mut t) = (std::ptr::null_mut(), std::ptr::null_mut());
    // SAFETY: valid buffers for the call; outputs freed by `take`.
    unsafe {
        slot_ra_login_resp(
            b.as_ptr(),
            b.as_bytes().len(),
            reply.status as c_int,
            &mut st,
            &mut u,
            &mut t,
        )
    };
    LoginAnswer {
        verdict: status_of(st, reply.status, &reply.body),
        user: take(u),
        token: take(t),
    }
}

pub fn parse_award(reply: &Reply) -> (Verdict, u32) {
    let b = body(reply);
    let mut st = empty_status();
    let mut awarded = 0u32;
    // SAFETY: as above.
    unsafe {
        slot_ra_award_resp(
            b.as_ptr(),
            b.as_bytes().len(),
            reply.status as c_int,
            &mut st,
            &mut awarded,
        )
    };
    (status_of(st, reply.status, &reply.body), awarded)
}

extern "C" fn collect_unlock(ctx: *mut c_void, id: u32, _when: i64) {
    // SAFETY: `ctx` is the Vec passed by `parse_session`, alive for the call.
    let v = unsafe { &mut *(ctx as *mut Vec<u32>) };
    if !v.contains(&id) {
        v.push(id);
    }
}

/// The account's unlocks for the game (Softcore and Hardcore both count).
pub fn parse_session(reply: &Reply) -> (Verdict, Vec<u32>) {
    let b = body(reply);
    let mut st = empty_status();
    let mut ids: Vec<u32> = Vec::new();
    // SAFETY: the callback only runs during the call, with `ids` alive.
    unsafe {
        slot_ra_start_session_resp(
            b.as_ptr(),
            b.as_bytes().len(),
            reply.status as c_int,
            &mut st,
            &mut ids as *mut Vec<u32> as *mut c_void,
            collect_unlock,
        )
    };
    (status_of(st, reply.status, &reply.body), ids)
}

/// One achievement as the server defines it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Remote {
    pub id: u32,
    pub points: u32,
    pub category: u32,
    pub set_type: u8,
    pub title: String,
    pub description: String,
    pub definition: String,
}

/// rc_api_runtime.h: core and unofficial categories; the core set's type.
pub const CATEGORY_CORE: u32 = 3;
pub const SET_TYPE_CORE: u8 = 0;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GameData {
    pub id: u32,
    pub console_id: u32,
    pub session_game_id: u32,
    pub title: String,
    pub achievements: Vec<Remote>,
}

impl GameData {
    /// What the evaluator tracks: the core set's published achievements (unofficial ones
    /// never run; subsets are not part of 0.6.0), minus those already unlocked.
    pub fn game_set(&self, unlocked: &[u32]) -> GameSet {
        GameSet {
            console_id: self.console_id,
            achievements: self
                .achievements
                .iter()
                .filter(|a| a.set_type == SET_TYPE_CORE)
                .map(|a| Achievement {
                    id: a.id,
                    definition: a.definition.clone(),
                    published: a.category == CATEGORY_CORE,
                })
                .collect(),
            unlocked: unlocked.to_vec(),
        }
    }

    pub fn find(&self, id: u32) -> Option<&Remote> {
        self.achievements.iter().find(|a| a.id == id)
    }
}

extern "C" fn collect_achievement(
    ctx: *mut c_void,
    set_type: u8,
    id: u32,
    points: u32,
    category: u32,
    title: *const c_char,
    description: *const c_char,
    definition: *const c_char,
) {
    // SAFETY: `ctx` is the Vec passed by `parse_game_sets`, alive for the call.
    let v = unsafe { &mut *(ctx as *mut Vec<Remote>) };
    v.push(Remote {
        id,
        points,
        category,
        set_type,
        title: borrow(title),
        description: borrow(description),
        definition: borrow(definition),
    });
}

/// Parses an achievement-sets answer (fresh from the server or from the cache).
pub fn parse_game_sets(reply: &Reply) -> (Verdict, Option<GameData>) {
    let b = body(reply);
    let mut st = empty_status();
    let mut game = RawGame {
        id: 0,
        console_id: 0,
        session_game_id: 0,
        title: std::ptr::null_mut(),
    };
    let mut list: Vec<Remote> = Vec::new();
    // SAFETY: the callback only runs during the call, with `list` alive.
    let r = unsafe {
        slot_ra_game_sets_resp(
            b.as_ptr(),
            b.as_bytes().len(),
            reply.status as c_int,
            &mut st,
            &mut game,
            &mut list as *mut Vec<Remote> as *mut c_void,
            collect_achievement,
        )
    };
    let title = take(game.title).unwrap_or_default();
    let verdict = status_of(st, reply.status, &reply.body);
    if r != rc::OK || !verdict.succeeded || game.id == 0 {
        return (verdict, None);
    }
    (
        verdict,
        Some(GameData {
            id: game.id,
            console_id: game.console_id,
            session_game_id: if game.session_game_id != 0 {
                game.session_game_id
            } else {
                game.id
            },
            title,
            achievements: list,
        }),
    )
}

/// Why requests stop until something changes (architecture 8.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pause {
    /// The token is invalid or expired: "Log in again".
    Login,
    /// `403 access_denied`, with the server's reason (for example an unverified email).
    /// Checked again at the next connection.
    Access(String),
    /// The server does not accept this client.
    UnsupportedClient,
}

/// What an award answer means for its ledger record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Award {
    /// Accepted, or the account already had it.
    Synced,
    /// Deleted (`404`) or unpublished (`409`): not retried.
    Rejected(String),
    /// `422`: a request bug on our side. Kept, never retried automatically.
    Held(String),
    /// Stop sending until the pause is lifted; the record stays pending.
    Pause(Pause),
    /// Network error, timeout, `5xx`, an answer that is not the server's JSON: stays
    /// pending, retried later.
    Retry(String),
    /// The server's JSON, but not an answer we expect (`200` with `Success: false` and no
    /// code, another 4xx, another achievement awarded). Retried a few times, as it may be
    /// passing, then held for investigation ([`UNSURE_TRIES`]), so it can never be retried
    /// for ever or silently dropped.
    Unsure(String),
}

/// Sent attempts after which an [`Award::Unsure`] answer is held instead of retried.
pub const UNSURE_TRIES: u32 = 3;

/// Statuses rcheevos' own client retries (rc_client.c `rc_client_should_retry`).
pub fn retryable(http: u16) -> bool {
    matches!(http, 429 | 502 | 503 | 504 | 521 | 522 | 523 | 524) || http >= 500
}

fn code_is(v: &Verdict, code: &str) -> bool {
    v.code.as_deref() == Some(code)
}

/// Common to every call: retry, or pause, or nothing to say.
fn common(v: &Verdict) -> Option<Result<Pause, String>> {
    if retryable(v.http) {
        return Some(Err(format!("HTTP {}", v.http)));
    }
    if code_is(v, "unsupported_client") {
        return Some(Ok(Pause::UnsupportedClient));
    }
    if v.http == 401
        || v.result == rc::INVALID_CREDENTIALS
        || v.result == rc::EXPIRED_TOKEN
        || code_is(v, "invalid_credentials")
        || code_is(v, "expired_token")
    {
        return Some(Ok(Pause::Login));
    }
    None
}

/// What an award answer means (architecture 8.2).
pub fn classify_award(v: &Verdict, awarded: u32, id: u32) -> Award {
    if let Some(c) = common(v) {
        return match c {
            Ok(p) => Award::Pause(p),
            Err(e) => Award::Retry(e),
        };
    }
    let code = || v.code.clone().unwrap_or_else(|| format!("http_{}", v.http));
    // RAWeb answers an achievement the account already has with "User already has ..."
    // and, when nothing else unlocked with it, `Success: false` (AwardAchievementAction,
    // checked against the server's source; rcheevos' own client assumes a success). It is
    // synced either way.
    if v.http == 200
        && v.message
            .as_deref()
            .is_some_and(|m| m.starts_with("User already has"))
    {
        return Award::Synced;
    }
    if v.http == 422 {
        return Award::Held(code());
    }
    // Rejected only on the server's own JSON answer: a proxy's or CDN's HTML 404 during
    // a deploy must never drop an unlock.
    let json = v.result != rc::INVALID_JSON && v.result != rc::NO_RESPONSE;
    if json && (code_is(v, "not_found") || v.result == rc::NOT_FOUND || v.http == 409) {
        return Award::Rejected(code());
    }
    // A 403 pauses only when it is the server's own answer: a proxy's or captive portal's
    // HTML 403 is retried like any other answer that is not the server's.
    if json && (v.http == 403 || v.result == rc::ACCESS_DENIED || code_is(v, "access_denied")) {
        return Award::Pause(Pause::Access(
            v.message.clone().unwrap_or_else(|| "Access denied".into()),
        ));
    }
    // "User already has" also comes back as a success (with a message). A success that
    // names no achievement (an older or trimmed answer) is the server accepting this one.
    if v.result == rc::OK && v.succeeded && (awarded == id || awarded == 0) {
        return Award::Synced;
    }
    let what = format!(
        "unexpected answer: HTTP {}, result {}, awarded {awarded}, {:?}",
        v.http, v.result, v.message
    );
    // Not JSON, or nothing at all (a proxy, a cut-off answer): try again later.
    if !json {
        return Award::Retry(what);
    }
    // A JSON answer we do not expect: retried a few times, then held (see Unsure).
    Award::Unsure(format!("unexpected_{}", v.http))
}

/// What any other answer (login, sets, session) means.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    Pause(Pause),
    /// Permanent for this request (for example a password that is wrong, or a game the
    /// server does not know). Not retried by itself.
    Refused(String),
    Retry(String),
}

pub fn classify(v: &Verdict) -> Outcome {
    if let Some(c) = common(v) {
        return match c {
            Ok(p) => Outcome::Pause(p),
            Err(e) => Outcome::Retry(e),
        };
    }
    if v.result == rc::OK && v.succeeded {
        return Outcome::Ok;
    }
    if v.http == 403 || v.result == rc::ACCESS_DENIED || code_is(v, "access_denied") {
        return Outcome::Pause(Pause::Access(
            v.message.clone().unwrap_or_else(|| "Access denied".into()),
        ));
    }
    if (400..500).contains(&v.http) || v.result == rc::NOT_FOUND || v.result == rc::API_FAILURE {
        return Outcome::Refused(
            v.message
                .clone()
                .unwrap_or_else(|| format!("HTTP {}", v.http)),
        );
    }
    let _ = (rc::INVALID_JSON, rc::NO_RESPONSE);
    Outcome::Retry(format!("HTTP {}, result {}", v.http, v.result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake;
    use crate::hash::{hex, md5};

    fn param(call: &Call, name: &str) -> Option<String> {
        call.post
            .as_deref()
            .unwrap_or("")
            .split('&')
            .find_map(|p| p.strip_prefix(&format!("{name}=")))
            .map(|v| v.replace("%2F", "/").replace("%40", "@"))
    }

    fn reply(status: u16, body: &str) -> Reply {
        Reply {
            status,
            body: body.to_string(),
        }
    }

    const HASH: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn requests_go_to_retroachievements_dorequest() {
        let c = award("Dean", "TOKEN", 42, HASH, 0).unwrap();
        assert_eq!(c.host(), HOST);
        assert!(c.url.starts_with("https://"), "{}", c.url);
        assert_eq!(c.path_and_query(), "/dorequest.php");
        assert_eq!(
            c.content_type.as_deref(),
            Some("application/x-www-form-urlencoded")
        );
        assert_eq!(c.kind(), "awardachievement");
        assert!(
            !format!("{c:?}").contains("TOKEN"),
            "post data is never printed"
        );
    }

    /// The award signature checked against RAWeb's own formula
    /// (AwardAchievementAction::initialize, own account), not only against rcheevos:
    /// md5(id . user . hardcore) with no offset; md5(id . user . hardcore . id . offset)
    /// with one.
    #[test]
    fn the_award_signature_is_what_the_server_expects() {
        let c = award("Dean", "TOKEN", 42, HASH, 0).unwrap();
        assert_eq!(param(&c, "a").as_deref(), Some("42"));
        assert_eq!(param(&c, "h").as_deref(), Some("0"), "Softcore only");
        assert_eq!(param(&c, "m").as_deref(), Some(HASH));
        assert_eq!(param(&c, "o"), None, "no offset when earned just now");
        assert_eq!(param(&c, "v"), Some(hex(&md5(b"42Dean0"))));

        let c = award("Dean", "TOKEN", 42, HASH, 7205).unwrap();
        assert_eq!(param(&c, "o").as_deref(), Some("7205"));
        assert_eq!(param(&c, "v"), Some(hex(&md5(b"42Dean0427205"))));

        let c = award("Dean", "TOKEN", 42, HASH, 1_209_600).unwrap();
        assert_eq!(param(&c, "v"), Some(hex(&md5(b"42Dean0421209600"))));
    }

    #[test]
    fn logging_in_sends_the_password_or_the_token_never_both() {
        let c = login_password("Dean", "pa ss&word").unwrap();
        assert_eq!(c.kind(), "login2");
        assert!(param(&c, "p").is_some());
        assert!(param(&c, "t").is_none());
        assert!(!format!("{c:?}").contains("pa ss"));
        let c = login_token("Dean", "TOKEN").unwrap();
        assert_eq!(param(&c, "t").as_deref(), Some("TOKEN"));
        assert!(param(&c, "p").is_none());
    }

    #[test]
    fn sets_and_sessions_name_the_rom_hash() {
        let c = game_sets("Dean", "T", HASH).unwrap();
        assert_eq!(c.kind(), "achievementsets");
        assert_eq!(param(&c, "m").as_deref(), Some(HASH));
        let c = start_session("Dean", "T", 510, HASH).unwrap();
        assert_eq!(c.kind(), "startsession");
        assert_eq!(param(&c, "g").as_deref(), Some("510"));
        assert_eq!(param(&c, "h").as_deref(), Some("0"));
        assert_eq!(param(&c, "m").as_deref(), Some(HASH));
    }

    #[test]
    fn answers_are_parsed_by_rcheevos() {
        let l = parse_login(&reply(200, &fake::login_ok("Dean", "NEWTOKEN")));
        assert_eq!(classify(&l.verdict), Outcome::Ok);
        assert_eq!(
            (l.user.as_deref(), l.token.as_deref()),
            (Some("Dean"), Some("NEWTOKEN"))
        );
        assert!(!l.to_string().contains("NEWTOKEN"));

        let (v, ids) = parse_session(&reply(200, &fake::session(&[2, 3], &[1, 3])));
        assert_eq!(classify(&v), Outcome::Ok);
        let mut ids = ids;
        ids.sort();
        assert_eq!(ids, vec![1, 2, 3], "Hardcore unlocks count in Softcore too");

        let body = fake::game_sets(
            510,
            5,
            "Castlevania",
            &[
                fake::def(11, "0xH001000=1"),
                fake::Def {
                    flags: 5,
                    ..fake::def(12, "0xH001001=1")
                },
            ],
        );
        let (v, g) = parse_game_sets(&reply(200, &body));
        assert_eq!(classify(&v), Outcome::Ok);
        let g = g.unwrap();
        assert_eq!(
            (g.id, g.console_id, g.title.as_str()),
            (510, 5, "Castlevania")
        );
        assert_eq!(g.achievements.len(), 2);
        let set = g.game_set(&[]);
        assert_eq!(set.console_id, 5);
        assert!(set.achievements[0].published);
        assert!(!set.achievements[1].published, "unofficial never runs");
        assert_eq!(set.achievements[0].definition, "0xH001000=1");
    }

    #[test]
    fn award_answers_mean_what_the_design_says() {
        let a = |status: u16, body: &str| {
            let (v, awarded) = parse_award(&reply(status, body));
            classify_award(&v, awarded, 42)
        };
        let pair = |p: (u16, String)| a(p.0, &p.1);
        assert_eq!(a(200, &fake::award_ok(42)), Award::Synced);
        assert_eq!(
            a(200, &fake::award_already(42)),
            Award::Synced,
            "already has"
        );
        assert!(matches!(pair(fake::not_found()), Award::Rejected(_)));
        assert!(matches!(
            pair(fake::error(409, "invalid_state", "Unpublished")),
            Award::Rejected(_)
        ));
        assert!(matches!(pair(fake::missing_parameter()), Award::Held(_)));
        assert_eq!(
            pair(fake::invalid_credentials()),
            Award::Pause(Pause::Login)
        );
        assert_eq!(pair(fake::expired_token()), Award::Pause(Pause::Login));
        assert_eq!(
            pair(fake::access_denied("Verify your email address")),
            Award::Pause(Pause::Access("Verify your email address".into()))
        );
        assert_eq!(
            pair(fake::unsupported_client()),
            Award::Pause(Pause::UnsupportedClient)
        );
        for s in [429, 500, 502, 503, 504, 521, 522, 523, 524] {
            assert!(matches!(pair(fake::gateway(s)), Award::Retry(_)), "{s}");
        }
        assert!(matches!(a(200, "not json"), Award::Retry(_)));
        assert!(matches!(a(200, ""), Award::Retry(_)));
        // A proxy's or CDN's HTML 404 during a deploy never rejects an unlock.
        assert!(matches!(
            a(404, "<html><body>404 Not Found</body></html>"),
            Award::Retry(_)
        ));
        assert!(matches!(a(404, ""), Award::Retry(_)));
        // Unexpected JSON answers are retried a few times, then held (the worker counts).
        assert!(
            matches!(a(200, &fake::award_ok(43)), Award::Unsure(_)),
            "another achievement awarded: not taken as this one"
        );
        assert!(matches!(a(200, r#"{"Success":false}"#), Award::Unsure(_)));
        assert!(matches!(
            a(410, r#"{"Success":false,"Error":"Gone"}"#),
            Award::Unsure(_)
        ));
        // A success naming no achievement is this one accepted.
        assert_eq!(a(200, r#"{"Success":true}"#), Award::Synced);
        // Only the server's own 403 pauses; a captive portal's HTML 403 is retried.
        assert!(matches!(
            a(403, "<html><body>Forbidden</body></html>"),
            Award::Retry(_)
        ));
    }

    #[test]
    fn other_answers() {
        let l = parse_login(&reply(401, &fake::invalid_credentials().1));
        assert_eq!(classify(&l.verdict), Outcome::Pause(Pause::Login));
        let (v, g) = parse_game_sets(&reply(
            404,
            &fake::error(404, "not_found", "Unknown game.").1,
        ));
        assert!(g.is_none());
        assert!(matches!(classify(&v), Outcome::Refused(_)));
        let (v, _) = parse_game_sets(&reply(503, &fake::gateway(503).1));
        assert!(matches!(classify(&v), Outcome::Retry(_)));
    }

    #[test]
    fn the_user_agent_names_slot_advance_and_rcheevos() {
        assert_eq!(
            user_agent("0.6.0"),
            "slot.advance/0.6.0 (Linux aarch64) rcheevos/12.5.0"
        );
    }
}
