# GUI baseline RSS — notes and mitigations

After the row-menu leak fix (commit that introduces `RowMenuDef` + GUI
`MENU_CACHE`) memory no longer grows with use, but the baseline
`lixun-gui` process still holds ~200–360 MB RSS on a fresh start.
This file documents where that baseline comes from and what is worth
trimming if/when it becomes a pain point.

## Measured composition (release build, Wayland session)

Observed on one run: `VmRSS` 360 MB, `VmHWM` 460 MB peak, `VmSize`
2.4 GB, 17 threads. Split per `/proc/<pid>/status`:
`RssAnon` ≈ 219 MB, `RssFile` ≈ 141 MB.

`pmap -x <pid>` breakdown (RSS contributions, rounded):

- `libLLVM.so.22.1` ≈ 77 MB — GTK4 GSK-Vulkan renderer loads
  `llvmpipe`/`radeonsi`/`iris` shader compilers for SPIR-V → GPU ISA.
  This is the single biggest file-backed chunk.
- Vulkan ICDs (`libvulkan_intel.so`, `libvulkan_radeon.so`,
  `libvulkan_hasvk.so`) ≈ 5.7 MB combined — all loaded even though
  only one GPU is used.
- `libgtk-4.so` ≈ 8.5 MB, `libglycin` ≈ 3.4 MB (image loader pulled
  in by icon paintables).
- libc / libstdc++ / libglib / libgio / libharfbuzz / libicudata
  combined ≈ 14 MB.
- Fonts ≈ 6 MB (SourceHanSansCN 2 MB, Noto family, Emoji, DejaVu).
- `lixun-gui` binary itself ≈ 4.5 MB.
- Heap (RssAnon) ≈ 219 MB: GTK4 widget tree, icon paintables,
  `StringList` backing model, Pango/Cairo/Vulkan buffers, row-pool
  widgets (30 slots × popover/menu/action-group), tracing buffers.

File-backed total matches shared-libs + fonts + binary ≈ 130 MB;
the rest is resident anonymous pages.

## Low-effort mitigations

Pick one of these only if baseline RSS becomes a user-visible
problem (launcher is expected to be always-resident).

1. **`GSK_RENDERER=cairo`** — disables GSK-Vulkan entirely. Drops
   `libLLVM` + Vulkan ICDs + shader caches (≈ 80 MB file-backed +
   Vulkan heap). The launcher has very little animation, so Cairo
   software path is acceptable. Easiest win.
2. **`GSK_RENDERER=gl`** — switches from Vulkan to OpenGL.
   Keeps GPU compositing, drops `libLLVM` on most drivers. Expect
   baseline around 180–220 MB instead of 300–360 MB.
3. **Raster icon theme** — an SVG-only theme goes through
   `librsvg` + `libglycin` + full Cairo raster cache per size.
   A bitmap-rendered theme (or pre-rendering our own icons at
   `ICON_SIZE_NORMAL`/`ICON_SIZE_TOP_HIT`) shaves a few MB.
4. **Kill unused Vulkan ICDs** — `VK_ICD_FILENAMES=` pointing at a
   single driver file prevents the loader from mapping all ICDs.
   Minor (a few MB), but free.

## Where NOT to optimize

- `ListView` row pool of ~30 row slots is static; ~6–10 MB total.
  Already optimized in `factory.rs`.
- `CACHED_HITS: Vec<Hit>` tracks the current visible result set
  (tens of items); tens of KB at most. Not a contributor.
- `MENU_CACHE` — `source_instance → gio::Menu`. Seven keys
  permanent. Sub-MB. Added specifically to kill the per-bind
  `set_menu_model` leak.

## Wiring (if we ever decide to default-enable)

`GSK_RENDERER` is read by GTK on `gtk::init()`. To force Cairo
by default, set it in the `systemd --user` unit that launches
`lixun-gui`, or export it from the shell that invokes the
launcher. Do NOT bake it into the binary: the invariant in
`AGENTS.md` says the host should not make rendering decisions
unilaterally — leave it to the environment.
