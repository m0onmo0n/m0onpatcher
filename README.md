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

The old `~` / `~~` blocks can also be made to replace every match (the **Legacy** checkbox, or a `# Legacy:` line in a mod), see "Legacy phase" below.
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

### Comments in mod files

A line that starts with `##` in the very first column is a comment. It is removed before the mod is read, so it
can go anywhere, including inside a `Replace:` / `With:` body, an `Add Passage:` text or an `Add Javascript:`
block, and it never ends up in the game:

```
## Makes the shower scene a bit longer
Replace:
<<shower>>
## the line above is the anchor
With:
<<shower>><<shower>>
```

Only lines that begin with `##` count: an indented `##`, or one in the middle of a line, is ordinary text. (A
single `#` is not a comment inside a mod, because lines like `#ui-dialog { ... }` are normal CSS; the `# Name:` /
`# Priority:` header lines at the top of a file work as before.)

This is a feature of m0onpatcher only. KittyPatcher would take a `##` line as part of the text it injects, so a
mod that uses these comments must not be shared with people who patch with KittyPatcher. Mods without `##` lines
patch exactly as before.

## Directives

The engine reproduces the m0on-kittypatcher v0.7.6 v2 behaviour; for the packs it was tested on, the patched
HTML is byte-for-byte identical to what that Python patcher produces. Every directive starts at the beginning of a line.

### Changing existing text

| Directive | What it does / what it is for |
|---|---|
| `Replace:` / `With:` | Swaps one piece of text for another anywhere in the game. The everyday directive for changing a line of a passage or of the script. Replaces the first match in passage text (every match inside the script block). The `[to]` marker lets the middle of the anchor vary. |
| `~~` / `~` | The old KittyPatcher format for the same thing (text to find, then its replacement). By default it behaves exactly like `Replace:`. With `# Legacy: all` in the mod (or the **Legacy** option on) it runs first and replaces every match (see "Legacy phase" below). |
| `Replace In Passage [Name]:` | Like `Replace:`, but only looks inside one named passage. For simple plain-text anchors. |
| `Replace In All Passages [tag]:` | Like `Replace:`, but applied in every passage that has the given tag. |
| `ReplaceReg [pattern]:` | Replace with a regular expression; each capture group gets its own `With{N}:` text. For changes that are too variable for a fixed anchor. |
| `Delete In Passage [Name]:` | Removes the given text from one passage. |
| `Delete Span [start] To [end]:` | Removes everything between two anchors. |

### Passages

| Directive | What it does / what it is for |
|---|---|
| `Prepend To Passage [Name]:` | Adds text at the start of a passage. Also works on a passage that another mod added. |
| `Append To Passage [Name]:` | Adds text at the end of a passage. |
| `Insert Before [anchor]:` / `Insert After [anchor]:` | Inserts raw text (JavaScript) just before or after the first occurrence of an anchor, without replacing anything. Modifiers in the brackets: `[ONCE]`, `[soft]`, `[typeof:...]`. An anchor that only exists inside another mod's `Add Passage` is found too. |
| `Add Tag To Passage [Name]:` | Adds one or more tags to a passage. |
| `Remove Tag From Passage [Name]:` | Removes a tag from a passage. |
| `Rename Passage [Old]: New` | Renames a passage (the name only; links to it are not rewritten). |
| `Clone Passage [Source]: As [Copy]` | Makes a copy of a passage, with its tags, under a new name. Handy as the starting point for a variation. |
| `Delete Block [Name]:` | Deletes a whole passage. Together with `Add Passage` of the same name it replaces a passage completely. |

### Adding new things

| Directive | What it does / what it is for |
|---|---|
| `Add Passage:` | Adds a new passage, either short (`Add Passage: Name`, an optional `tags:` line, then the text) or as a full `<tw-passagedata>` tag. `pid="auto"` and `position="auto"` are filled in for you. Never overwrites a passage that already exists. |
| `Add Javascript:` | Appends JavaScript to the game's story script. One block per mod file. |
| `Add CSS:` | Appends styles to the game's stylesheet. |
| `Add Events:` | Adds entries to the game's event list (`setup.Events.db`). |
| `Add Function [name]:` | Defines a JavaScript function (skipped if one with that name already exists). Usually paired with `Hook`. |
| `Add Variable [name]: value` | Sets a story variable (`$name`) when a new game starts (in `StoryInit`). |
| `Add StoryVar [name]: value` | Like `Add Variable`, and also registers it in `setup.saveStateFields` so it is kept in saves. |

### Changing scripts

| Directive | What it does / what it is for |
|---|---|
| `Hook [function]:` | Wraps an existing function so your code runs `before:` it, `after:` it (can change the result through `_result`) or `around:` it, without editing the function itself. The least invasive way to change behaviour. |
| `Insert Into Function [function]:` | Inserts code after an anchor inside one function. |
| `Replace In Function [function]:` | Replaces an anchor inside one function. |
| `Replace Function [function]:` | Replaces the whole body of a function. A last resort, because it breaks other mods that touch the same function. |
| `Replace Function Signature [function]: params` | Changes the parameter list of a function. |

### Arrays, objects and classes

| Directive | What it does / what it is for |
|---|---|
| `Append Array [name]:` | Adds entries to the end of a JavaScript array, for example a list of items or locations. Skipped if the entries are already there. Can be limited to one function with `[name In Function [fn]]`. |
| `Insert Into Array [name]:` | Adds entries to a named array (same function scoping as `Append Array`). |
| `Insert Into Object [name]:` | Adds entries to a named object. |
| `Merge Into Object [name]:` | Merges an object into an existing one, adding only the keys that are missing. |
| `Append Variable to Class [Name]:` | Adds lines at the end of a class's constructor, so a new field exists on every object. |
| `Append Function to Class [Name]:` | Adds a method to a class. |

### Guards

A guard makes a directive run only when a condition holds. Write it on the line above the directive (or inside its
body); if a block has several guards, all of them must pass.

| Guard | Runs the directive when... |
|---|---|
| `IfExists [text]` / `IfNotExists [text]` | the text is / is not present in the game. |
| `IfRegExists [regex]` / `IfRegNotExists [regex]` | the regular expression does / does not match. |
| `IfModLoaded [File.mod]` / `IfModNotLoaded [File.mod]` | another mod file is / is not loaded. For features that depend on another mod. |
| `IfPassageExists [Name]` / `IfPassageNotExists [Name]` | a passage with that name does / does not exist. |
| `IfPassageContains [Name\|text]` | the passage contains the text. |
| `IfPassageHasTag [Name\|tag]` | the passage has the tag. |
| `IfFunctionExists [function]` / `IfFunctionNotExists [function]` | a function with that name does / does not exist. |

### Also supported

| Feature | What it does / what it is for |
|---|---|
| `<e> ... </e>` | Lets you write a replacement text or a new passage as plain Twine; the engine escapes it for the HTML. |
| Conflict report | Two blocks aimed at the same target are reported, and the run fails instead of silently picking one. |

Checked against the Python patcher on real packs and on test mods: everything above except the rarely used
parts (guards other than `IfExists`, the `[to]` marker, the insert modifiers), which are implemented the
same way but have not been compared run by run.

### Legacy phase for `~` / `~~` blocks

KittyPatcher 0.1.5c replaced **every** match of an anchor. The later versions (and this engine by default) replace
only the first match. The legacy phase brings the old behaviour back for the old `~` / `~~` format, and only for that
format. Each mod chooses for itself with a line in its header (the `#` lines at the top of the file):

| Header line | The mod's `~` / `~~` blocks... |
|---|---|
| *(none)* | follow the **Legacy** checkbox on the Patcher tab (`legacy_replace_all` in `m0onpatcher-config.json`, `--legacy-all` in the `kitty-patch` command-line tool). Off by default: first match only. |
| `# Legacy: all` | replace every match, whatever the checkbox says. |
| `# Legacy: first` | replace only the first match, whatever the checkbox says. For a mod written for first-match behaviour. |

Any other value is ignored, with a warning. The blocks that replace every match behave like this:

* They are applied **first**, before any other directive (including `Replace:`), in load order.
* Each block replaces **every** occurrence of its anchor in the whole file, passages and script block alike.
* `Replace:` blocks, every other directive and the `~` blocks of mods that stay on first-match work exactly as before, after the legacy phase.
* A block whose anchor is not found is a failure. The other legacy blocks still run, but the run fails as a whole and no output is written.

Replacing every match can change text that a later block needs. When a block fails for that reason, its log entry
names the earlier block that rewrote part of its anchor. A big legacy-format mod written for first-match behaviour
may therefore need `# Legacy: first`, or the checkbox left off. This is not a byte-for-byte copy of 0.1.5c: it applied each file's `~`
blocks before the same file's `Replace:` blocks (file by file, not all legacy blocks first), merged blocks that
share an anchor, and let a line break in an anchor match any whitespace; none of that is reproduced here.

### Differences from the Python patcher

* `Add StoryVar` and `Clone Passage` **work** here. In the Python v2 patcher they abort the whole run
  (`KeyError: 'if_exists'`).
* `Replace In All Passages` looks every passage up fresh. The Python version writes to stale positions after
  the first change and can corrupt the output when several passages match.

## Not supported

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
