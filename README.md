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

#### Stage builder

The other half of the Modding tools tab builds a game in **stages** in one go, instead of running the patcher once
per stage folder and copying the HTML between them. It is an ordered list; add to it with the buttons or by dropping
files and folders on the window, and move things with *Up* / *Down*:

```
1. Input HTML    CourseOfTemptation.html
2. Mod file      Bits&Fun V_1.4.2.3_v3.mod
3. Output HTML   stage1.html                    (written to disk, and the input of what follows)
4. Mods folder   stage 2\mods
5. Output HTML   CourseOfTemptation.modded.html
```

* The list starts with one **Input HTML**. **Mods** items are a single mod file or a whole folder. An **Output HTML** ends a stage.
* The Mods items between two outputs are **one patch run**, like a stage folder: the mods load in list order (a folder inside itself by `# Priority:` and file name) and can see each other's changes.
* Each output is written in the form ScRepacker writes (the patched HTML is read into its parts and merged back into the HTML the stage started from), and the next stage reads that file. Mods written against ScRepacker's output, like a stage 2 pack, need this: they do not match the raw patcher output. On the pack it was tested on, a two-stage chain gives exactly the files ScRepacker writes for the same two stage folders.
* Dropping a `.html` sets the input, a `.mod` / `.kdiff` / `.patch` file or a folder is added as a Mods item at the end of the list (in front of the last Output instead, if that Output has no mods above it yet), and outputs are added with *Add output HTML* (a save dialog, so an existing file is never picked by accident).
* The stages run top to bottom. The first stage that fails stops the chain; the outputs written before it stay. Every stage writes its three logs to `<output folder>\logs\<output name>\`.
* The **Legacy** checkbox applies to every stage. The list is remembered between runs.
* **Save list** writes the list to a `.json` file and **Load list** replaces the current list with one from a file, so a build can be kept, shared or started again later. The file is the same one `m0onpatcher-cli stages --file` reads (see "Command line"). It stores full paths.

Note that the Patcher tab writes the raw patched HTML, like KittyPatcher, not this normalized form.

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

## Command line

**`m0onpatcher-cli`** is the terminal version: the same engine and the same code as the window, as a console
program that the terminal waits for and that returns an exit code, so it can be used in scripts. It comes next to
`m0onpatcher` in the release zip. (On Windows the window program is built as a window app, so a terminal does not
wait for it: use `m0onpatcher-cli` there. On Linux and macOS `m0onpatcher` itself also understands these commands
when it is started with arguments.)

```
m0onpatcher-cli <command> [options]
```

| Command | What it does |
|---|---|
| `patch` | Apply a folder of mods to a game HTML (the Patcher tab). |
| `stages` | Run a chain of patch stages in one go (the Stage builder). |
| `extract` | Extract a game HTML into editable sources (Modding tools, Extract). |
| `compile` | Compile the sources back into an HTML (Modding tools, Compile). |
| `help [command]` | Show the help of a command. |

Options for every command:

| Option | What it does |
|---|---|
| `-h`, `--help` | Show the help of the command. |
| `-q`, `--quiet` | Print only warnings, errors and the final result. |
| `-v`, `--verbose` | Also print the per-block patch log (about a thousand lines for a big pack; it is always in `ModPatchLog.txt`). |
| `-V`, `--version` | Show the version (instead of a command). |

The exit code is **0** when the command is done, **1** when it failed (a block did not apply, a file is missing, an
existing source folder would be overwritten, ...), and **2** for wrong arguments. Progress and results go to stdout,
warnings and errors to stderr, so `> log.txt` and `2> errors.txt` work.

#### `patch`

```
m0onpatcher-cli patch <game.html> <mods-folder> [options]
```

| Option | What it does |
|---|---|
| `--html <file>`, `--mods <folder>` | The same two things as options, instead of in the first two places (a folder that is given without an option is taken as the mods folder). |
| `-o`, `--out <file>` | Where to write the result. Default: `<game name>.modded.html` next to the game. |
| `--legacy-all` | The **Legacy** option: the old `~` / `~~` blocks of mods without a `# Legacy:` header are applied first and replace every match (see "Legacy phase"). |

It does what the Patcher tab does: the raw patched HTML, the three logs in `<game folder>/logs`, and no output file when a block fails.

#### `stages`

```
m0onpatcher-cli stages --input <game.html> --mods <path> --output <file> [--mods <path> --output <file> ...]
m0onpatcher-cli stages --file <list.json>
```

The `--input`, `--mods` and `--output` options are the items of the Stage builder list, and they run in the order you
write them (the same rules apply, see "Stage builder"). `--mods` is a mod file or a folder and can be repeated.

```
m0onpatcher-cli stages --input game.html --mods "Bits&Fun.mod" --output stage1.html --mods "stage 2/mods" --output final.html
```

`--file` reads the list from a JSON file instead (the same file the Stage builder's *Save list* writes): an array of items, each one
`{"Input": "path"}`, `{"Mods": "path"}` or `{"Output": "path"}`. `--legacy-all` applies to every stage.

#### `extract` and `compile`

```
m0onpatcher-cli extract <game.html> [--src <folder>] [--force]
m0onpatcher-cli compile <game.html> [--src <folder>] [--out <file>]
```

| Option | What it does |
|---|---|
| `--html <file>` | The game HTML, as an option. |
| `--src <folder>` | The sources: where `extract` writes them and `compile` reads them. Default: the `src` folder next to the game HTML. A folder given without an option is taken as this, so `compile "my src" --html game.html` works. |
| `-f`, `--force` | `extract` only: replace an existing source folder. Without it the command refuses, because extracting again replaces the files and changes made there are lost. |
| `-o`, `--out <file>` | `compile` only: where to write the result. Default: `<game name>.merged.html` next to the game. |

`compile` needs the original game HTML too: the sources are merged into it as the reference document. Extracting a game
and compiling it again without changes gives back the original file, byte for byte.

#### The older `kitty-patch` tool

The engine crate also has a small tool, `kitty-patch <input.html> <mods_dir> <out.html> [--crlf] [--legacy-all]`
(built with `cargo build --release -p kitty-engine --bin kitty-patch`), used to compare the engine with other
patchers: it writes the raw result even when blocks failed, no logs, and exits with 1 when a block failed
(`KITTY_PROGRESS=1` prints stage messages, `KITTY_LOG=<file>` dumps the full log). `kitty-dump <mods_dir> <out.json>`
writes what the engine understood from a mods folder as JSON. For everyday use, `m0onpatcher-cli patch` is the one to use.

## Building

Rust 1.85 or newer (edition 2024).

```
cargo build --release -p m0onpatcher      # the window and the command line program
cargo test  -p kitty-engine               # engine tests
cargo dist                                # zip for this platform in target/dist/
```

The patch engine is the `crates/kitty-engine` crate; m0onpacker depends on it.

It builds the same way on Windows, Linux and macOS (`cargo build --release -p m0onpatcher`). Notes per platform:

* **Windows:** the exe starts on PCs without the Visual C++ runtime.
* **Linux:** the file dialogs use the desktop's `xdg-desktop-portal` at run time (present on most desktops). If a build
  stops on a missing system library (for example `xkbcommon` or `xcb`), install its `-dev` package. The window is forced
  onto X11 (XWayland on Wayland desktops) because drag-and-drop does not work on native Wayland.
* **macOS:** `cargo build --release` gives a plain binary. `cargo dist` builds an `.app` with `cargo bundle`, so run
  `cargo install cargo-bundle` first. The `.app` has no icon yet (only the Windows exe and the window icon are set) and
  is only ad-hoc signed (not notarized by Apple), so on the first start macOS asks you to confirm it: right-click the app and choose Open, or allow it under System Settings, Privacy & Security. If macOS says the app is "damaged", run `xattr -cr` on the app once to clear the download flag. The zip has the app only,
  no command line program: build `m0onpatcher-cli` with cargo if you want it on a Mac.

If you hit a problem on any of them, please report it.

## Credits and copyright

Original parts: Copyright (c) 2026 m0on. All rights reserved. ScRepacker and KittyPatcher are published without
a licence and their authors keep all rights in their work; they have given m0on their permission to build on it and
to distribute this program. That permission covers this program only. See `NOTICE.txt`.

* **LK / kitty**: original creator of KittyPatcher (0.1.5c and 0.2.0a)
* **CoderMaverick**: author of KittyPatcher v0.4.0, v0.6.0 and the v0.7.x versions, and of the KittyPatcher ecosystem
* **nuqqur**: ScRepacker (https://gitgud.io/nuqqur/screpacker-rs/), the source of the extractor, the merger and the window design
* **Pictogrammers**: the "shield-moon" icon from Material Design Icons (Apache 2.0)
* The Course of Temptation modding community
