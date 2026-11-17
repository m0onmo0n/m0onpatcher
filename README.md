# m0onpacker

A packer and mod patcher for **Course of Temptation** (SugarCube / Twine). It is a fusion of
**ScRepacker v0.1.5** (extract the game HTML into editable sources, apply `.diff` mods, repack) and the
**m0on-kittypatcher v0.7.6 fork** (apply `.mod` files), rewritten in Rust as a built-in engine. One program
handles both mod formats; no Python and no separate KittyPatcher are needed.

This is the `packer` branch of the m0onpatcher repository. The `patcher` branch is **m0onpatcher**: the same patch
engine (the `kitty-engine` crate) in a small standalone window, without the packer.

## Using it

1. Start `m0onpacker` (Windows `.exe`, Linux binary or macOS app).
2. Open the game HTML (drag it onto the window, or pick it on the *Modding* tab). Put your mods in a `mods` folder next to it.
3. Press **Patch**. The result is `<game>.modded.html` next to the original.

On a full 189-mod pack the patch step takes a few seconds.

### Patch engine (Config tab)

| Setting | What it does |
|---|---|
| **Internal engine** (default) | The built-in engine only. KittyPatcher is never called. |
| **Internal engine, KittyPatcher as fallback** | Built-in engine first; an external KittyPatcher is used only for a mod set the built-in engine cannot handle yet. |
| **External KittyPatcher** | The original ScRepacker behaviour: run a KittyPatcher `.py` / `.exe` that you point it to. |

The setting is the `engine` key of `screpacker-config.json` (`builtin`, `auto`, `external`). The config and log
file names (`screpacker-config.json`, `screpacker-logs\`) are unchanged from ScRepacker.

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

When a mod set uses something the engine does not support, it stops with a message naming the directive (or,
with *KittyPatcher as fallback*, hands the pack to the external patcher).

## Building

Rust 1.85 or newer (edition 2024).

```
cargo build --release -p m0onpacker      # the program
cargo test  -p kitty-engine               # engine tests
cargo dist                                # zip for this platform in target/dist/
```

The patch engine is the `crates/kitty-engine` crate; its design notes are in `docs/engine-design.md`.
It compiles for Windows, Linux and macOS (checked with `cargo check`; Linux needs the usual X11/Wayland
development libraries for the GUI).

## Credits and copyright

Original parts of this fork: Copyright (c) 2026 m0on. All rights reserved. ScRepacker and KittyPatcher are
published without a licence; their authors keep all rights in their work, and no licence for it is granted here.
See `NOTICE.txt`.

* **nuqqur**: ScRepacker (https://gitgud.io/nuqqur/screpacker-rs/), the extractor, packer and interface this program is built on
* **LK / kitty**: original creator of KittyPatcher (0.1.5c and 0.2.0a)
* **CoderMaverick**: author of KittyPatcher v0.4.0, v0.6.0 and the v0.7.x versions, and of the KittyPatcher ecosystem
* The Course of Temptation modding community
