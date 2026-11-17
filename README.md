# m0onpatcher

A small standalone program that applies `.mod` files (KittyPatcher format) to a **Course of Temptation**
(SugarCube / Twine) game HTML. Its patch engine, the `kitty-engine` crate, is a Rust reimplementation of the
**m0on-kittypatcher v0.7.6 fork**, with a plain window around it. No Python and no separate KittyPatcher are needed.

**m0onpacker** is the sibling project: a fork of ScRepacker (extract, `.diff` mods, repack) with this engine built in.
It uses the `kitty-engine` crate from this repository.

## Using it

Start `m0onpatcher` (Windows `.exe` or Linux binary). It has three tabs.

### Patcher

1. Choose the game HTML and the folder with the `.mod` files, or drag them onto the window. A `mods` folder next to the HTML is picked automatically.
2. Press **Patch**. The result is `<game>.modded.html` next to the original (use *Save as* to pick another name).

If any block fails, nothing is written. Like KittyPatcher on Windows, the output file uses Windows line endings.
The three KittyPatcher-style logs (`FailsPatchLog.txt`, `MainPatchLog.txt`, `ModPatchLog.txt`) are written to
`<game folder>\logs`.

### Modding tools

* **Extract source** writes the editable sources of the game HTML (passages as `.twee`, scripts, styles) to a `src` folder next to it. If `src` already exists you are asked first, because extracting again replaces its files.
* **Compile HTML** builds `<game>.merged.html` from `src` again, using the game HTML as the reference.
* **Open source folder** opens `src` in the file manager.

The extractor and the compiler are ScRepacker's.

### About

What the program is, the copyright notice and the credits.

**Show log** (on the Patcher and Modding tools tabs) switches between the log window (the same scrolling log as the
packer) and a plain progress bar. The program remembers its settings in `m0onpatcher-config.json` next to the exe.

### Mod load order

Mods load in alphabetical order of their file names (ignoring the extension and case). `# Priority:` headers
are read; `.json` dependency files and `[load_order]` in `kitty_config.toml` are **not** supported and are ignored.

## Directives the internal engine supports

The engine reproduces the m0on-kittypatcher v0.7.6 v2 behaviour; for the packs it was tested on, the patched
HTML is byte-for-byte identical to what that Python patcher produces.

**Replace and passage edits:** `Replace:` / `With:`, the old `~~` / `~` format, `Replace In Passage`,
`Replace In All Passages`, `Append To Passage`, `Prepend To Passage`, `Insert Before`, `Insert After`
(an anchor that only exists inside another mod's `Add Passage` is handled too), `Delete In Passage`,
`Delete Span`, `Delete Block`, `Rename Passage`, `Add Tag To Passage`, `Remove Tag From Passage`,
`Clone Passage` (write it as `Clone Passage [Source]: As [Copy]`).

**New things:** `Add Passage` (full and shorthand), `Add Javascript`, `Add CSS`, `Add Events`, `Add Function`,
`Add Variable`, `Add StoryVar`.

**Script edits:** `Insert Into Function`, `Replace In Function`, `Replace Function`,
`Replace Function Signature`, `Hook` (before / after / around), `ReplaceReg`.

**Arrays, objects and classes:** `Append Array`, `Insert Into Array`, `Insert Into Object`, `Merge Into Object`,
`Append Variable to Class`, `Append Function to Class`.

**Guards:** `IfExists`, `IfNotExists`, `IfRegExists`, `IfRegNotExists`, `IfModLoaded`, `IfModNotLoaded`,
`IfPassageExists`, `IfPassageNotExists`, `IfPassageContains`, `IfPassageHasTag`, `IfFunctionExists`,
`IfFunctionNotExists`.

Also: the `<e> ... </e>` wrapper, the `[to]` marker, `pid="auto"` / `position="auto"`, the `[ONCE]`, `[soft]`
and `[typeof:...]` insert modifiers, and the conflict report (two blocks aimed at the same target make the run fail).

Checked against the Python patcher on real packs and on test mods: everything above except the rarely used
parts (guards other than `IfExists`, the `[to]` marker, the insert modifiers), which are implemented the
same way but have not been compared run by run.

### Differences from the Python patcher

* `Add StoryVar` and `Clone Passage` **work** here. In the Python v2 patcher they abort the whole run
  (`KeyError: 'if_exists'`).
* `Replace In All Passages` looks every passage up fresh. The Python version writes to stale positions after
  the first change and can corrupt the output when several passages match.

## Not supported (yet)

| Directive / feature | Why |
|---|---|
| `Move Passage` | The Python version rewrites references using positions that go stale; use `Rename Passage` plus normal `Replace` blocks. |
| `Wrap Passage` | Use `Prepend To Passage` / `Append To Passage`. |
| `IfVersionAtLeast` | In the Python patcher it compares Twine's version, not the game's, so it always passes. |
| Anchors containing a backslash followed by `n` (a JavaScript `"\n"`) | The Python matching of these is broken; such anchors fail there too. |
| `.json` dependency files, `[load_order]` | Alphabetical order only. |
| Encrypted (KittyEncode) mod files | Not read. |

When a mod set uses something the engine does not support, it stops with a message naming the directive.

## Building

Rust 1.85 or newer (edition 2024).

```
cargo build --release -p m0onpatcher      # the program
cargo test  -p kitty-engine               # engine tests
cargo dist                                # zip for this platform in target/dist/
```

The patch engine is the `crates/kitty-engine` crate; m0onpacker depends on it. It compiles for Windows, Linux
and macOS (checked with `cargo check`; Linux needs the usual X11/Wayland development libraries for the GUI).

## Credits and copyright

Original parts: Copyright (c) 2026 m0on. All rights reserved. ScRepacker and KittyPatcher are published without
a licence; their authors keep all rights in their work, and no licence for it is granted here. See `NOTICE.txt`.

* **LK / kitty**: original creator of KittyPatcher (0.1.5c and 0.2.0a)
* **CoderMaverick**: author of KittyPatcher v0.4.0, v0.6.0 and the v0.7.x versions, and of the KittyPatcher ecosystem
* **nuqqur**: ScRepacker (https://gitgud.io/nuqqur/screpacker-rs/), the source of the extractor, the merger and the window design
* **Pictogrammers**: the "shield-moon" icon from Material Design Icons (Apache 2.0)
* The Course of Temptation modding community
