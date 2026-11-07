# Vendored sources

Both are copied byte for byte from upstream. Do not edit them here: update by replacing
the whole directory from the tagged upstream source and recording the new version below.

## rcheevos

- Upstream: https://github.com/RetroAchievements/rcheevos
- Version: tag `v12.5.0`, commit `1433173220a7eaede6a9ed7a18e94117be1821e0`
- Licence: MIT (`rcheevos/LICENSE`; shipped as `System/licenses/rcheevos-MIT.txt`)
- Copied: `include/`, `src/`, `LICENSE`, `README.md`, `CHANGELOG.md`. Not copied:
  upstream's tests, validator and build files.
- Compiled (see `../build.rs`): the runtime (`src/rcheevos/`), `rc_libretro.c`,
  `rc_compat.c`, `rc_util.c`, `rc_version.c`, and the ROM-only parts of `src/rhash/`.

## libretro.h

- Upstream: the libretro API header as shipped in libretro/mgba at the pinned core commit
  `e31759b24e7a4e3899285ff720d7b573ac328ae7` (`src/platform/libretro/libretro.h`), so the memory map layout matches the
  core slot.advance runs. slot.advance's mGBA patches do not touch it.
- Licence: MIT, stated in the header itself.
- Needed by `rcheevos/src/rc_libretro.c`.
