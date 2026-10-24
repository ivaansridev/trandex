# trandex

A fast, floating file, app, and command launcher for Wayland, built with GTK4 and gtk4-layer-shell. It runs as a resident daemon, so the window opens instantly.

## Features

- Search desktop apps, commands on your `PATH`, and files in your home directory.
- Streaming file search: results appear as `fd` finds them.
- Calculator: type an expression like `2+2*3` and it shows the answer.
- Alt + 1 to Alt + 0 launch the first ten results.
- Optional preview panel on the left or right, showing images, text, folder listings, and app details.
- Monochrome theme, fully customizable with CSS.

## Requirements

- A Wayland compositor that supports wlr-layer-shell (Sway, Hyprland, niri, and similar).
- GTK 4 and gtk4-layer-shell.
- [`fd`](https://github.com/sharkdp/fd) for file search. Without it, trandex falls back to `find`.
- Optional: `eza` for folder previews. Without it, `ls` is used.

## Installation

```sh
git clone https://github.com/ivaansridev/trandex.git
cd trandex
cargo install --path . --locked
```

## Usage

Start the daemon once, for example from your compositor's startup config:

```sh
trandex -d --config ~/.config/trandex/config.yaml
```

Then bind a key to toggle the window:

```sh
trandex --config ~/.config/trandex/config.yaml
```

Running `trandex` again hides the window.

### Keys

| Key | Action |
| --- | --- |
| Type | Search |
| Up / Down | Move the selection |
| Enter | Open the selected result (or the first one) |
| Alt + 1 … Alt + 0 | Open results 1 to 10 |
| Escape | Hide the window |

## Configuration

Copy `src/config.yaml` to `~/.config/trandex/config.yaml` and edit it:

```yaml
styles: style.css                     # CSS theme, relative to this file (or absolute)
position: center                      # center, top, left, or right
placeholder: "Start typing..."
preview: right                        # left, right, or false
items:                                # order matters
  - AppList
  - Commands
  - Math
  - Index
ignore:                               # directory names skipped by file search
  - node_modules
  - target
  - .git
  - .cache
```

- `items` accepts only `AppList` (desktop apps), `Commands` (executables on your `$PATH`), `Math` (calculator), and `Index` (files). Sections appear in the order you list them, and unlisted sections are not loaded.
- `ignore` matches directory names. Add any folder you don't want searched.

Styles are CSS. The classes you can target are listed in `src/themes/styling.md`, and `src/themes/basic.css` is a good starting point.

## Project layout

- `src/main.rs`: command line, daemon, and socket handling
- `src/ui.rs`: the GTK window, search, results, and preview
- `src/indexer.rs`: app and command discovery, and file search
- `src/config.rs`: config loading and validation
- `src/themes/basic.css`: the default theme

## Troubleshooting

- **"daemon not running"**: start it with `trandex -d` first.
- **Files don't show up**: check that `fd` is installed and that the folder isn't listed under `ignore`.
- **Window is in the wrong place**: check `position` and that your compositor supports layer-shell.
- **Debug timing**: set `TRANDEX_DEBUG=1` before starting the daemon.

## License

GPL-3.0-only. See [LICENSE](LICENSE).
