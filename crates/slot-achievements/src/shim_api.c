/*
 * slot.advance: a flat C face for rcheevos' request and response code (rapi), so Rust never
 * touches rcheevos' structures. Every request is built and every response parsed by
 * rcheevos itself (D1); this file only copies the results out.
 *
 * Strings handed to Rust are malloc'd copies, freed with slot_ra_free.
 */

#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include "rc_api_runtime.h"
#include "rc_api_user.h"
#include "rc_error.h"

typedef struct slot_ra_req {
  char *url;
  char *post;
  char *content_type;
} slot_ra_req;

typedef struct slot_ra_status {
  int result;
  int succeeded;
  char *error_message;
  char *error_code;
} slot_ra_status;

static char *dup(const char *s) {
  size_t n;
  char *d;
  if (!s)
    return NULL;
  n = strlen(s);
  d = (char *)malloc(n + 1);
  if (d)
    memcpy(d, s, n + 1);
  return d;
}

void slot_ra_free(void *p) { free(p); }

void slot_ra_req_free(slot_ra_req *r) {
  free(r->url);
  free(r->post);
  free(r->content_type);
  memset(r, 0, sizeof(*r));
}

static int finish(rc_api_request_t *request, int result, slot_ra_req *out) {
  memset(out, 0, sizeof(*out));
  if (result == RC_OK) {
    out->url = dup(request->url);
    out->post = dup(request->post_data);
    out->content_type = dup(request->content_type);
    if (!out->url)
      result = RC_OUT_OF_MEMORY;
  }
  rc_api_destroy_request(request);
  return result;
}

int slot_ra_login_req(const char *user, const char *password, const char *token, slot_ra_req *out) {
  rc_api_request_t request;
  rc_api_login_request_t p;
  memset(&p, 0, sizeof(p));
  p.username = user;
  p.password = password;
  p.api_token = token;
  return finish(&request, rc_api_init_login_request_hosted(&request, &p, NULL), out);
}

int slot_ra_game_sets_req(const char *user, const char *token, const char *hash, slot_ra_req *out) {
  rc_api_request_t request;
  rc_api_fetch_game_sets_request_t p;
  memset(&p, 0, sizeof(p));
  p.username = user;
  p.api_token = token;
  p.game_hash = hash;
  return finish(&request, rc_api_init_fetch_game_sets_request_hosted(&request, &p, NULL), out);
}

int slot_ra_start_session_req(const char *user, const char *token, uint32_t game_id, const char *hash,
                              slot_ra_req *out) {
  rc_api_request_t request;
  rc_api_start_session_request_t p;
  memset(&p, 0, sizeof(p));
  p.username = user;
  p.api_token = token;
  p.game_id = game_id;
  p.game_hash = hash;
  p.hardcore = 0;
  return finish(&request, rc_api_init_start_session_request_hosted(&request, &p, NULL), out);
}

int slot_ra_award_req(const char *user, const char *token, uint32_t id, const char *hash,
                      uint32_t seconds_since_unlock, slot_ra_req *out) {
  rc_api_request_t request;
  rc_api_award_achievement_request_t p;
  memset(&p, 0, sizeof(p));
  p.username = user;
  p.api_token = token;
  p.achievement_id = id;
  p.hardcore = 0; /* Softcore only (0.6.0 scope). */
  p.game_hash = hash;
  p.seconds_since_unlock = seconds_since_unlock;
  return finish(&request, rc_api_init_award_achievement_request_hosted(&request, &p, NULL), out);
}

static void fill_status(slot_ra_status *st, int result, const rc_api_response_t *r) {
  st->result = result;
  st->succeeded = r->succeeded;
  st->error_message = dup(r->error_message);
  st->error_code = dup(r->error_code);
}

static rc_api_server_response_t server(const char *body, size_t len, int http) {
  rc_api_server_response_t s;
  s.body = body;
  s.body_length = len;
  s.http_status_code = http;
  return s;
}

int slot_ra_login_resp(const char *body, size_t len, int http, slot_ra_status *st, char **username,
                       char **token) {
  rc_api_login_response_t r;
  rc_api_server_response_t s = server(body, len, http);
  int result = rc_api_process_login_server_response(&r, &s);
  fill_status(st, result, &r.response);
  *username = dup(r.username);
  *token = dup(r.api_token);
  rc_api_destroy_login_response(&r);
  return result;
}

int slot_ra_award_resp(const char *body, size_t len, int http, slot_ra_status *st, uint32_t *awarded) {
  rc_api_award_achievement_response_t r;
  rc_api_server_response_t s = server(body, len, http);
  int result = rc_api_process_award_achievement_server_response(&r, &s);
  fill_status(st, result, &r.response);
  *awarded = r.awarded_achievement_id;
  rc_api_destroy_award_achievement_response(&r);
  return result;
}

typedef void (*slot_ra_unlock_fn)(void *ctx, uint32_t id, int64_t when);

int slot_ra_start_session_resp(const char *body, size_t len, int http, slot_ra_status *st, void *ctx,
                               slot_ra_unlock_fn each) {
  rc_api_start_session_response_t r;
  rc_api_server_response_t s = server(body, len, http);
  uint32_t i;
  int result = rc_api_process_start_session_server_response(&r, &s);
  fill_status(st, result, &r.response);
  if (result == RC_OK) {
    /* In Softcore, an achievement unlocked in either mode counts as unlocked. */
    for (i = 0; i < r.num_unlocks; i++)
      each(ctx, r.unlocks[i].achievement_id, (int64_t)r.unlocks[i].when);
    for (i = 0; i < r.num_hardcore_unlocks; i++)
      each(ctx, r.hardcore_unlocks[i].achievement_id, (int64_t)r.hardcore_unlocks[i].when);
  }
  rc_api_destroy_start_session_response(&r);
  return result;
}

typedef struct slot_ra_game {
  uint32_t id;
  uint32_t console_id;
  uint32_t session_game_id;
  char *title;
} slot_ra_game;

typedef void (*slot_ra_achievement_fn)(void *ctx, uint8_t set_type, uint32_t id, uint32_t points,
                                       uint32_t category, const char *title, const char *description,
                                       const char *definition);

int slot_ra_game_sets_resp(const char *body, size_t len, int http, slot_ra_status *st, slot_ra_game *game,
                           void *ctx, slot_ra_achievement_fn each) {
  rc_api_fetch_game_sets_response_t r;
  rc_api_server_response_t s = server(body, len, http);
  uint32_t i, j;
  int result = rc_api_process_fetch_game_sets_server_response(&r, &s);
  fill_status(st, result, &r.response);
  memset(game, 0, sizeof(*game));
  if (result == RC_OK) {
    game->id = r.id;
    game->console_id = r.console_id;
    game->session_game_id = r.session_game_id;
    game->title = dup(r.title);
    for (i = 0; i < r.num_sets; i++) {
      const rc_api_achievement_set_definition_t *set = &r.sets[i];
      for (j = 0; j < set->num_achievements; j++) {
        const rc_api_achievement_definition_t *a = &set->achievements[j];
        each(ctx, set->type, a->id, a->points, a->category, a->title, a->description, a->definition);
      }
    }
  }
  rc_api_destroy_fetch_game_sets_response(&r);
  return result;
}

int slot_ra_game_sets_id_req(const char *user, const char *token, uint32_t game_id, slot_ra_req *out) {
  rc_api_request_t request;
  rc_api_fetch_game_sets_request_t p;
  memset(&p, 0, sizeof(p));
  p.username = user;
  p.api_token = token;
  p.game_id = game_id;
  return finish(&request, rc_api_init_fetch_game_sets_request_hosted(&request, &p, NULL), out);
}
