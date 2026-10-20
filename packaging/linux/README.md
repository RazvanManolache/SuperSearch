# Linux packaging assets for SuperSearch

The GUI's **System integration** toggle (toolbar plug icon) installs
these automatically to the user directories — no sudo needed. This
folder holds the pristine copies.

## What gets installed where

| Asset here | Installed to | Used by |
|---|---|---|
| `supersearch.desktop` | `~/.local/share/applications/` (copy manually) | App launcher entry |
| `nautilus-scripts/Search with SuperSearch` | `~/.local/share/nautilus/scripts/` (+x) | Nautilus right-click → Scripts |
| `dolphin-servicemenu/supersearch.desktop` | `~/.local/share/kio/servicemenus/` and `~/.local/share/kservices5/ServiceMenus/` | Dolphin right-click on folders |
| `nemo-action/supersearch.nemo_action` | `~/.local/share/nemo/actions/` | Nemo right-click on folders |
| `hicolor/supersearch-*.png` | `~/.local/share/icons/hicolor/{16,24,32,48,64,128,256}x256/apps/supersearch.png` (copy manually) | App icon theme |

## Requirements

- `supersearch-gui` must be on `PATH` (the actions call it by name
  with `--folder <dir>`).
- After copying icons or the launcher manually, run
  `gtk-update-icon-cache` / `update-desktop-database` if your
  environment needs the nudge.
