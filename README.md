# Rivulet

**Tidy your Windows desktop. One small exe, 2-3 MB of RAM, no installer, and your files never move.**

![Sections flowing open from a rail of labels](docs/demo.gif)

*Recorded from the running program on a generated wallpaper, showing the Liquid, Aurora and Neon styles with the Settings panel, so no real file names appear. GIF colours are coarser than on screen.*

Rivulet sorts your desktop icons by type into a slim rail of labels on the left. Click a label and that section flows open. Click it again, or click empty desktop, and it folds away.

## Why

- **Tiny.** A single exe of about 564 KB, written in Rust.
- **Light.** Measured at 2-3 MB of its own memory while idle (10-17 MB counting Windows libraries shared with other programs), and almost no CPU.
- **Safe for your files.** Nothing is moved, renamed or hidden on disk. Only icon positions change, so every path keeps working.
- **Leaves your wallpaper alone.** It draws in its own see-through layer behind the icons.

## Features

- Sections by file type: Folders, Word, Excel, PowerPoint, PDF, Notes, Code, Apps, Zips.
- **Recent** shows what you created or changed in the last 3 days, wherever it belongs.
- **Folder portals** add a label for any folder, such as Downloads or a cloud folder, without moving anything. Scroll through it, open subfolders inside the panel, and go Back or Up.
- **A dot** on a label means something new arrived that you have not looked at. Open the section and each new item has a patch of the section's colour and the same dot, until the next time you open it.
- **Drag an icon onto a label** to file it there for good.
- **Drag a label up or down the rail** to reorder it: it lifts off, the others slide aside, and it settles into its new place. Recent stays at the top and Settings at the bottom.
- **Ctrl+Alt+D** shows the desktop with your last section open. Press again to go back.
- **A Settings label** for themes, labels on the left or right, animation speed and style, and more.
- **Double-click empty desktop** to hide the labels for a completely clean screen, and again to bring them back.
- **Six styles.** Plain is the default and uses 2-3 MB. Smooth adds soft corners and shadows (60-90 MB). Glass is frosted glass made from your own wallpaper (55-100 MB). Liquid is clear glass drawn by a shader on the graphics card: the wallpaper bends through the rim with a touch of colour fringing, the panel, neck and label melt into one shape, the light follows your mouse, and the glass deepens over bright wallpaper so text stays readable (80-130 MB). Aurora fills the glass with slow curtains of colour from your wallpaper and sections, moving only while a section is open or the mouse is near. Neon is dark glass with a lit tube round each edge in the section's colour, spilling light that brightens as the mouse comes close. All but Plain need the graphics card; with Plain, those libraries are never loaded.
- Colours per section, five themes, and tints that follow your wallpaper.
- New files are sorted as they arrive, and it recovers by itself if Explorer restarts.

### Rivulet's own icons

Set **Settings > General > Icons** to Rivulet and it draws your icons itself, so Explorer's hover box never shows. It is off by default, and switching back is one click. In this mode you also get:

- Hover and selection highlights, Ctrl, Shift and box selection, and the real right-click menu.
- Dragging files onto a label or into another program.
- Scrolling, list view with dates and sizes, and a sort order for each section.
- **Pin** to keep a section open, and tabs that join related sections in one panel.
- **Ctrl+Alt+F** to find any desktop file by typing.
- **Peek**: an option that makes Ctrl+Alt+D bring the sections up over your open windows.

### Settings pages

- **Appearance:** font, text and icon size, icon tint, corners, how solid panels are, and panel, text, border and tint colours.
- **Sections:** add, rename, reorder, recolour or remove sections, choose symbols, add folder portals and join tabs, without editing any text.
- **Rules:** add or remove rules, including `size>100mb` and `age<7d` as well as name patterns.
- **Layouts:** save and switch named layouts (Ctrl+Alt+Page Up / Down), and optionally keep one for each screen setup.

### Animation styles

Set **Settings > General > Animation style** to choose how a section opens and closes. Each style moves the panel and its files together:

| Style | Opening | Closing |
|---|---|---|
| **Pour** (default) | The panel pours out of the label through the curved neck, and each file lifts into its place | Folds back into the label |
| **Unfold** | The panel opens downwards one fold at a time, like paper, and each icon swings open on a hinge | The icons swing shut, then the panel folds back up |
| **Curtain** | A lit circle spreads across the panel from the neck, uncovering each file as it passes | The circle shrinks back into the neck |
| **Pop** | The panel springs out like a bubble, a little past full size, and the files pop in from the middle outwards | The files shrink away, then the bubble swells slightly and is drawn back into the label |
| **Drift** | The panel fades in, settling from just above, and the files fall into place, swaying like leaves | The files fall away downwards, then the panel sinks and fades |
| **Trace** | A lit line draws the panel's outline outwards from the neck, the glass fills, then the files light up | The files go dark, the glass empties, and the outline draws back into the neck |

The styles work with the Liquid, Aurora, Neon and Clear looks. Plain, Smooth and Glass always use Pour. **Animation speed** sets how quickly they run; at Fast, closing still takes at least a third of a second so the style can be seen. In `sections.ini` the setting is `entrance = pour` (or `unfold`, `curtain`, `pop`, `drift`, `trace`) under `[general]`.

## Install

1. Download `rivulet.exe` from [Releases](../../releases) and put it in its own folder, such as `Documents\Rivulet`. It keeps its settings beside itself.
2. Double-click it.

To start it with Windows, run `rivulet.exe --install-login` from a terminal in that folder. This uses a Task Scheduler logon task, because Windows can take minutes to work through the Startup folder on a busy PC, and icons would sit in columns until then. `rivulet.exe --install` adds a Start menu entry.

**Windows may warn you.** Rivulet is unsigned, so SmartScreen can say it is unrecognised, and antivirus tools may be wary because it watches mouse clicks on the desktop and moves icons inside Explorer. It has to do both to work: the click watcher is how labels respond, and it reads nothing else. It makes no network connections. The source is all here, so you can build it yourself:

```
cargo build --release
```

## Using it

| To do this | Do this |
|---|---|
| Open or close a section | Click its label |
| Close the open section | Click empty desktop |
| File an icon in a section | Drag it onto the label |
| Show the desktop and last section | Ctrl+Alt+D |
| Hide or show the labels | Double-click empty desktop |
| Change themes and behaviour | Click the Settings label |
| Re-sort, edit rules, undo or stop | Right-click any label |
| Find a file (Rivulet's icons) | Ctrl+Alt+F, or click a file and type |
| Move between saved layouts | Ctrl+Alt+Page Up / Down |
| Delete, rename, open | Delete, F2, Enter, after clicking a file |
| Select all, copy, cut, paste | Ctrl+A, Ctrl+C, Ctrl+X, Ctrl+V |

## Your own rules

Right-click a label and choose **Edit rules** to open `sections.ini`. For example, to send anything with "thesis" in its name to Word:

```ini
[names]
*thesis* = Word
```

To make a section of your own, such as Thesis, add one line under each of these headings:

```ini
[types]
Thesis =

[names]
*thesis* = Thesis

[layout]
column5 = Thesis

[colours]
Thesis = 38C6D9
```

To add a folder as a section of its own:

```ini
[portals]
Downloads = %USERPROFILE%\Downloads
Thesis drive = D:\Thesis
```

The same file also sets the shortcut and the screen. It explains each setting.

## Stopping and removing it

- **Undo and stop** in the right-click menu puts icons back where they were when Rivulet first started.
- To remove it: choose Undo and stop, run `rivulet.exe --remove-login` if you turned on starting at login, then delete its folder.
- **If icons ever seem to vanish,** right-click the desktop and choose Refresh. That always brings them back, even if Rivulet has crashed. It hides icons by placing them off-screen, and Windows pulls them back on Refresh.

## Status and limits

This is an early release, so please read this.

- **Tested on one setup:** Windows 11 version 26H2 (build 26300), one 1920x1080 screen at 125%. Tests covered a forced crash, an Explorer restart, Undo and repeated Refreshes.
- **Older Windows versions are untested.** There is a fallback for the older desktop layout, but it has never been run on a real machine. It needs Windows 10 version 1703 or later.
- **A second screen is untested.** Choosing a screen in the rules is written and unit tested, but has not been tried on real hardware.
- In the Plain style, rounded corners are slightly rough, because Windows does not allow fine per-pixel transparency in this layer without a much heavier drawing method.
- Glass and Liquid follow Windows' Fill, Fit, Stretch, Center and Tile settings on the main screen. With a different wallpaper on a second screen, or Span across screens, the picture in the glass may not line up with the real one.
- Hidden icons can flash for a moment on Refresh.
- It relies on undocumented Windows behaviour, so a Windows update could break it.
- Portals show a folder's newest 24 files and open them on a double-click. They have no right-click menu or drag and drop yet, and they refresh when opened.
- One section opens at a time, and a section cannot hold more icons than fit on the screen.
- If the mouse comes back to a section while it is closing, it reopens, but that close uses the Pour motion whatever the animation style.

If something breaks, run `rivulet.exe --doctor` and [open an issue](../../issues/new/choose) with the report. It contains no file names.

## How it compares

[Stardock Fences](https://www.stardock.com/products/fences/) is the established paid tool for this, and it has many more features and years of testing. Choose it if you want polish and support. Rivulet is for people who want something tiny and open source that does less. I have not measured Fences, so this README makes no claim about its memory use.

## Licence

MIT
