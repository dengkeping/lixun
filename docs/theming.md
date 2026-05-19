# Theming Lixun

This guide is for users authoring their own Lixun themes. It covers
file layout, the full set of CSS classes the launcher exposes, the
specificity rules you must respect, and how live reload works.

If you only want a quick start, copy
[`docs/style.example.css`](style.example.css) into a theme directory
and edit from there.

## File layout

Themes live under your XDG config directory:

```
${XDG_CONFIG_HOME:-~/.config}/lixun/
├── config.toml
├── style.css                  # optional per-user override (layer 2)
└── themes/
    ├── tokyo-night/
    │   └── style.css          # required, the only file Lixun reads
    └── orange-orgasm/
        └── style.css
```

Activate a theme by adding its folder name to `config.toml`:

```toml
[gui]
theme = "tokyo-night"
```

Set `theme = ""` or remove the key to fall back to the built-in
stylesheet. The folder name must match the `theme` value exactly.
Missing themes are logged as a warning and the launcher continues
with no theme applied.

## Stylesheet cascade

Lixun installs three `GtkCssProvider`s at fixed priorities. Later
layers override earlier ones, GTK's normal specificity rules apply
within each layer.

| Layer | Path | GTK priority | Hot reload |
|-------|------|--------------|------------|
| 1. Built-in | embedded `crates/lixun-gui/style.css` | `APPLICATION` | no |
| 2. User override | `~/.config/lixun/style.css` | `APPLICATION + 1` | yes |
| 3. Theme | `~/.config/lixun/themes/<name>/style.css` | `APPLICATION + 2` | yes |

The chosen theme wins over the user override. This matches Hyprland's
`source =` semantics where the later source wins. The user override
is meant for small permanent patches that should apply regardless of
which theme is active; a theme is meant to be a complete look.

## Live reload

The launcher watches three files via `inotify` with an 80 ms
debounce:

- `~/.config/lixun/config.toml` — config change, including a theme
  switch or blur toggle. Reapplies the theme and rebuilds the
  watcher if the active theme path changed.
- `~/.config/lixun/style.css` — user override layer. Reloads the
  user provider in place.
- The active theme's `style.css`. Reloads the theme provider in
  place.

Saving any of these files is enough. No restart, no SIGHUP. Symlinks
are resolved before the parent directory is watched, so the standard
dotfiles pattern (config.toml symlinked into a dotfiles repo) works
out of the box.

If a save does not take effect, check the journal:

```sh
journalctl --user -u lixund.service -f -o cat
```

You should see one of:

```
INFO lixun_gui::style_manager: loading theme stylesheet from /path/to/style.css
INFO lixun_gui::style_manager: loading user CSS override from /path/to/style.css
```

GTK CSS parser errors are printed to the same log on every reload.

## CSS classes Lixun renders

Every class below is set on a real widget at runtime. The factory
that builds result rows lives in
`crates/lixun-gui/src/factory.rs`; the window scaffolding is in
`crates/lixun-gui/src/window.rs`. Class names are stable.

### Window chrome

- `.lixun-window` — the main launcher surface. The default theme
  paints a translucent dark glass with `border-radius: 14px`.
- `.lixun-window.lixun-no-blur` — added automatically when blur is
  disabled (config `[gui] blur = false`) or the compositor does not
  support `org_kde_kwin_blur`. Use this to raise the background
  opacity so the surface stays legible without compositor blur.
- `.lixun-showing` / `.lixun-hiding` — applied for 150 ms / 120 ms
  during fade-slide animations.
- `window`, `window.background`, `.background`, `#lixun-root` — must
  stay fully transparent so `.lixun-window`'s rounded corners
  actually round. The default theme uses
  `background: transparent !important;` here; GTK4 logs two harmless
  parser warnings about `!important` on every reload — ignore them.

### Search entry

- `.lixun-entry` — the search field.
- `.lixun-entry:focus`, `.lixun-entry:focus-within` — focus state.
  GTK's default focus ring is usually ugly; remove `outline`,
  `box-shadow`, `border` here.
- `.lixun-entry placeholder` and `.lixun-entry text placeholder` —
  placeholder text colour. Set `opacity: 1` so it is not faded.
- `.lixun-entry image` — the search icon. `-gtk-icon-size` controls
  its rendered size.

### Results list

- `.lixun-results` — the ScrolledWindow that contains the ListView.
  Keep its background transparent.
- `scrolledwindow`, `scrolledwindow viewport`, `listview` — keep
  these transparent too, otherwise GTK paints a default surface
  behind your rows.
- `listview row` and its `:selected`, `:selected:focus`,
  `:selected:hover`, `:active` variants — the GTK ListView wraps
  every row in its own internal `row` node. The host theme (Breeze,
  Adwaita, etc.) paints aggressive selection backgrounds here.
  Stomp them flat: `background-color: transparent;
  background-image: none; box-shadow: none; outline: none;`. The
  visible selection cue lives on the inner `.lixun-top-hit` class
  described below, not on this node.

### Result row

The factory creates each row as a `gtk::Box` and registers it with
**both** an ID and a class:

```rust
row.set_widget_name("lixun-hit");        // GTK ID, matches #lixun-hit
add_css_class(&row, "lixun-hit");        // CSS class, matches .lixun-hit
```

This is the single most important fact in this document. See the
specificity trap section below.

- `.lixun-hit` (and `#lixun-hit`) — every result row.
- `.lixun-hit:hover` — hover state.
- `.lixun-top-hit` — added to the row that holds the keyboard
  selection cursor. **This is your selection cue**, not
  `listview row:selected`. It tracks arrow-key navigation.
- `.lixun-top-hit-hero` — added to row 0 when the daemon identifies
  a confident top hit for the current query. May stack with
  `.lixun-top-hit` on the same row.

### Row labels

- `.lixun-title` — the primary line of each result.
- `.lixun-subtitle` — the secondary line.
- `.lixun-kind` — the right-aligned kind/category label.
- `.lixun-top-hit .lixun-title`, `.lixun-top-hit-hero .lixun-title`
  — bigger title on the selected row.

### Status bar

- `.lixun-status` — the container.
- `.lixun-status-label` — neutral status text.
- `.lixun-status-calc` — calculator answer (monospaced).
- `.lixun-status-action` and `.lixun-status-action:hover` — the
  action button at the right of the status bar.

### Context menu

- `popover.menu` — outer chrome.
- `popover.menu contents` — inner container.
- `popover.menu modelbutton` and `:hover` — individual menu rows.

## The `#lixun-hit` specificity trap

This is the trap every theme author hits exactly once. The row
widget has both the ID `lixun-hit` and the class `lixun-hit`. An
ID selector has specificity `1,0,0` — it beats **any** class
combination, no matter how many classes you stack. `.lixun-hit`
is `0,1,0`. `.lixun-hit.lixun-top-hit` is `0,2,0`. Both lose to
`#lixun-hit`.

The wrong way:

```css
/* DO NOT do this */
#lixun-hit,
.lixun-hit {
  background-color: transparent;
}

.lixun-top-hit {
  background-color: rgba(0, 0, 0, 0.3);  /* will never paint */
}
```

The `.lixun-top-hit` rule cannot win the cascade against
`#lixun-hit` because the ID has higher specificity. The selection
fill silently disappears even though the rule parses fine.

The right way:

```css
.lixun-hit {
  background-color: transparent;
}

.lixun-top-hit {
  background-color: rgba(0, 0, 0, 0.3);  /* wins, 0,2,0 vs 0,1,0 */
}
```

Use the class form `.lixun-hit`. Do **not** use `#lixun-hit` unless
you genuinely want a rule that cannot be overridden by anything
short of another ID selector or `!important`.

Diagnostic clue: if a rule on `.lixun-top-hit` applies some
properties (e.g. `box-shadow`) but not others (e.g.
`background-color`), the missing property is being set on a
higher-specificity selector elsewhere — usually `#lixun-hit`.

## Recommended structure for a new theme

1. Pick a palette and write it as a comment block at the top.
2. Reset window chrome: transparent `window/.background/#lixun-root`,
   then paint `.lixun-window` with your surface colour and border.
3. Style `.lixun-entry` and its `placeholder` / `image` children.
4. Make `scrolledwindow`, `listview` transparent.
5. Stomp `listview row, listview row:selected*` flat.
6. Style `.lixun-hit` and `.lixun-hit:hover`.
7. Style `.lixun-top-hit` and `.lixun-top-hit-hero`. This is the
   selection cue — make it visible without changing the row's box
   model (keep `padding` identical to `.lixun-hit`, paint the
   accent with `background-color` and optionally a `box-shadow:
   inset 2px 0 0 0 <accent>` left bar). Changing padding breaks
   ListView's cached row height.
8. Style `.lixun-title`, `.lixun-subtitle`, `.lixun-kind`.
9. Style the status bar (`.lixun-status*`) and the popover menu.
10. Optionally style `.lixun-window.lixun-no-blur` with a higher
    opacity for use without compositor blur.

## Compositor blur

`[gui] blur = true` in `config.toml` enables the KDE
`org_kde_kwin_blur` protocol. On compositors that implement it
(KDE / KWin), the surface gets a real backdrop blur shaped to the
window's rounded-rect region. On compositors that do not (sway,
niri, GNOME), the call is a silent no-op.

Hyprland users get blur via a compositor-side rule:

```ini
layerrule = blur, ^(lixun-gui)$
```

The launcher sets its layer-shell namespace to `lixun-gui`
specifically so this rule can target it.

When blur is disabled or unsupported, the `.lixun-window.lixun-no-blur`
class is added so your stylesheet can paint a heavier background
instead. The built-in theme uses `rgba(28, 28, 32, 0.92)` for that
state.

## Testing your theme

Have the launcher running, then in another terminal:

```sh
# Save the file — live reload kicks in after 80 ms.
touch ~/.config/lixun/themes/<name>/style.css

# Watch GTK parser errors and reload events.
journalctl --user -u lixund.service -f -o cat | grep -iE 'theme|css|style'
```

Exercise:

- Type a query, arrow up and down — the row with
  `.lixun-top-hit` must be visibly distinct.
- Run a query that produces a confident top hit (e.g. an exact app
  name) — row 0 should also carry `.lixun-top-hit-hero`.
- Toggle `[gui] blur = false` and back — the
  `.lixun-window.lixun-no-blur` class should appear and disappear,
  and the surface should remain legible in both states.
- Switch `[gui] theme = "other"` and back — the watcher should
  pick up the change without restart.

## Reference

- Built-in stylesheet: [`crates/lixun-gui/style.css`](../crates/lixun-gui/style.css)
- Example user override: [`docs/style.example.css`](style.example.css)
- Row factory: [`crates/lixun-gui/src/factory.rs`](../crates/lixun-gui/src/factory.rs)
- Style manager: [`crates/lixun-gui/src/style_manager.rs`](../crates/lixun-gui/src/style_manager.rs)
- Live-reload watcher: [`crates/lixun-gui/src/style_watcher.rs`](../crates/lixun-gui/src/style_watcher.rs)
