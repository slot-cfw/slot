/*
 * slot.advance's thin layer over rcheevos (vendored, unmodified).
 *
 * rcheevos' event, peek and address-validation callbacks carry no user-data pointer of
 * their own (rc_runtime.h), so the state of the call in progress lives in a thread-local
 * for exactly the length of that call. Everything here runs on the emulator thread,
 * between core calls; nothing here allocates.
 */
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include "rc_libretro.h"
#include "rc_runtime.h"
#include "rhash/md5.h"

typedef struct slot_ra_call {
  const rc_libretro_memory_regions_t *regions;
  int wanted; /* the one event type collected by this call */
  uint32_t *ids;
  size_t capacity;
  size_t count;
  size_t overflow;
  int invalid; /* a read fell outside the mapped memory */
} slot_ra_call;

static _Thread_local slot_ra_call *current;

static void no_core_memory(uint32_t id, rc_libretro_core_memory_info_t *info) {
  (void)id;
  info->data = NULL;
  info->size = 0;
}

rc_libretro_memory_regions_t *slot_ra_regions_new(void) {
  return (rc_libretro_memory_regions_t *)calloc(1, sizeof(rc_libretro_memory_regions_t));
}

void slot_ra_regions_free(rc_libretro_memory_regions_t *regions) {
  if (regions) {
    rc_libretro_memory_destroy(regions);
    free(regions);
  }
}

/* Builds the region table for `console_id` from a core's memory map. The table only
 * records pointers into the core's memory; the descriptors themselves are not kept.
 * Returns non-zero when at least one region has memory behind it. */
int slot_ra_regions_init(rc_libretro_memory_regions_t *regions,
                         const struct retro_memory_descriptor *descriptors,
                         unsigned count, uint32_t console_id) {
  struct retro_memory_map map;
  map.descriptors = descriptors;
  map.num_descriptors = count;
  rc_libretro_memory_destroy(regions);
  return rc_libretro_memory_init(regions, count ? &map : NULL, no_core_memory, console_id);
}

void slot_ra_regions_clear(rc_libretro_memory_regions_t *regions) {
  rc_libretro_memory_destroy(regions);
}

uint32_t slot_ra_regions_total(const rc_libretro_memory_regions_t *regions) {
  return (uint32_t)regions->total_size;
}

static uint32_t peek(uint32_t address, uint32_t num_bytes, void *ud) {
  uint8_t buffer[4];
  (void)ud;
  if (current == NULL || num_bytes == 0 || num_bytes > 4) {
    if (current)
      current->invalid = 1;
    return 0;
  }
  if (rc_libretro_memory_read(current->regions, address, buffer, num_bytes) < num_bytes) {
    /* Unmapped memory is never an observed zero: the caller cancels the frame. */
    current->invalid = 1;
    return 0;
  }
  switch (num_bytes) {
  case 1:
    return buffer[0];
  case 2:
    return buffer[0] | ((uint32_t)buffer[1] << 8);
  case 3:
    return buffer[0] | ((uint32_t)buffer[1] << 8) | ((uint32_t)buffer[2] << 16);
  default:
    return buffer[0] | ((uint32_t)buffer[1] << 8) | ((uint32_t)buffer[2] << 16) |
           ((uint32_t)buffer[3] << 24);
  }
}

static void on_event(const rc_runtime_event_t *event) {
  if (current == NULL || (int)event->type != current->wanted)
    return;
  if (current->count < current->capacity)
    current->ids[current->count++] = event->id;
  else
    current->overflow++;
}

static int valid_address(uint32_t address) {
  return current != NULL && rc_libretro_memory_find(current->regions, address) != NULL;
}

/* Evaluates one frame. Triggered achievement IDs are written to `ids` (never more than
 * `capacity`); the runtime is not changed by this call beyond rcheevos' own frame
 * processing, so the caller deactivates them afterwards. Returns 1 if any read fell
 * outside the mapped memory, in which case the caller must ignore the triggers. */
int slot_ra_do_frame(rc_runtime_t *runtime, const rc_libretro_memory_regions_t *regions,
                     uint32_t *ids, size_t capacity, size_t *count, size_t *overflow) {
  slot_ra_call call;
  memset(&call, 0, sizeof(call));
  call.regions = regions;
  call.wanted = RC_RUNTIME_EVENT_ACHIEVEMENT_TRIGGERED;
  call.ids = ids;
  call.capacity = capacity;
  current = &call;
  rc_runtime_do_frame(runtime, on_event, peek, NULL, NULL);
  current = NULL;
  *count = call.count;
  *overflow = call.overflow;
  return call.invalid;
}

/* Disables every achievement that reads memory outside the map, writing their IDs to
 * `ids`. Returns how many were disabled (all of them, even past `capacity`). */
size_t slot_ra_validate(rc_runtime_t *runtime, const rc_libretro_memory_regions_t *regions,
                        uint32_t *ids, size_t capacity) {
  slot_ra_call call;
  memset(&call, 0, sizeof(call));
  call.regions = regions;
  call.wanted = RC_RUNTIME_EVENT_ACHIEVEMENT_DISABLED;
  call.ids = ids;
  call.capacity = capacity;
  current = &call;
  rc_runtime_validate_addresses(runtime, on_event, valid_address);
  current = NULL;
  return call.count + call.overflow;
}

/* Reads `num_bytes` (1 to 4) at a RetroAchievements address through the region table,
 * as the runtime would. Returns 0 and sets *ok to 0 when the read is unmapped. For tests
 * and diagnostics. */
uint32_t slot_ra_peek(const rc_libretro_memory_regions_t *regions, uint32_t address,
                      uint32_t num_bytes, int *ok) {
  slot_ra_call call;
  uint32_t value;
  memset(&call, 0, sizeof(call));
  call.regions = regions;
  current = &call;
  value = peek(address, num_bytes, NULL);
  current = NULL;
  *ok = !call.invalid;
  return value;
}

/* MD5 of a buffer (rhash's md5.c), for resume fingerprints. */
void slot_ra_md5(const uint8_t *data, size_t len, uint8_t digest[16]) {
  md5_state_t md5;
  md5_init(&md5);
  while (len > 0) {
    int chunk = len > 0x10000000 ? 0x10000000 : (int)len;
    md5_append(&md5, data, chunk);
    data += chunk;
    len -= (size_t)chunk;
  }
  md5_finish(&md5, digest);
}
