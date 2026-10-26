# Spell checking

> Fork-only: this feature is not part of upstream microsoft/edit.

`edit` underlines misspelled words and offers replacements. It uses the
[spellbook](https://github.com/helix-editor/spellbook) engine with an en_US Hunspell dictionary
that is built into the binary, so there is nothing to install.

## What gets checked

| File | Checked |
|---|---|
| No syntax definition (`.txt`, `.log`, `.csv`, `Makefile`, …) | Every word |
| Markdown (`.md`) | Prose, headings, lists, emphasis, link text. Not: inline code, link URLs, fenced code blocks (```` ``` ```` and `~~~`) and their fence lines, indented code blocks, HTML tags |
| Git commit messages (`COMMIT_EDITMSG`, `MERGE_MSG`) | The message. Not: `#` lines, or the diff that `git commit -v` appends |
| Code (Rust, C, C++, C#, Go, Java, JavaScript, Python, PowerShell, shell, SQL, …) | Comments and string literals |
| Data formats (JSON, YAML, TOML, XML/HTML, INI/properties/git config, `.gitignore`, `git-rebase-todo`) | Comments only |
| Diffs (`.diff`, `.patch`) | Nothing |

Some text is never flagged:
- URLs, email addresses, file names (`main.rs`), and paths that start with `/`, `./`, `~/` or a
  drive letter (`C:\Users`).
- `` `code` `` spans, `<tags>` and `&entities;`.
- Words that contain digits or an inner `_`, camelCase words, ALL-CAPS words (also `APIs` and
  `API's`), and single letters.

Outside string literals, text with a `\` before a letter or digit is skipped too: relative paths
(`tools\build\release`), registry keys (`HKEY_CURRENT_USER\Software`) and LaTeX commands (`\alpha`).
Inside string literals, `\` starts an escape instead. Escapes (`\n`, `\x41`, `\u{41}`) and printf
specs (`%lu`, `%02X`) separate words and aren't checked themselves. A word that contains an escaped
apostrophe (`isn\'t`) is skipped.

Accented spellings of dictionary words are accepted (`café`, `naïve`, `résumé`).

## When underlines appear

A word is underlined once you've finished it, after you type a space or punctuation, or move the
cursor away. The word you are typing isn't flagged mid-word. Undo and redo don't count as typing.

The dictionary loads in the background when `edit` starts, and underlines appear as soon as it's ready.

## Fixing a word

Each of these opens a list of up to 8 suggestions below the misspelled word at the cursor, or the
one that ends right before the cursor:
- **F7** or **Ctrl+T** (these work while the document has focus).
- **Edit > Spelling Suggestions…**
- **Right-click** (a two-finger click on a trackpad) on the misspelled word. This moves the cursor
  there and opens the list. Right-clicking a correctly spelled word only moves the cursor.

In the list:
- **Up/Down** to choose, **Enter** or a **double-click** to replace. The replacement is one undo
  step, so **Ctrl+Z** brings the old word back.
- **Esc** or a click elsewhere closes the list. It also closes if you switch documents, resize the
  terminal, or scroll the word out of view.

Instead of suggestions, the list may show one of these messages: "No suggestions", "No misspelled
word at cursor", "Loading dictionary…", or "Spell check unavailable".

**View > Spell Check** turns spell checking on or off until you quit. It isn't saved. While it's
off, the shortcuts do nothing and the Edit menu item is hidden.

## Settings

Add these keys to `settings.json`:

```json
{
    "spellcheck.enabled": true,
    "spellcheck.underline": "curly"
}
```

| Key | Values | Default |
|---|---|---|
| `spellcheck.enabled` | `true`, `false`. With `false`, the dictionary isn't loaded until you turn on View > Spell Check. | `true` |
| `spellcheck.underline` | `"curly"` (red curly underline), `"plain"` (normal underline) | `"curly"` |

Location of `settings.json`:
- Windows: `%APPDATA%\Microsoft\Edit\settings.json`
- macOS: `~/Library/Application Support/com.microsoft.edit/settings.json`
- Linux and others: `$XDG_CONFIG_HOME/msedit/settings.json`, or `~/.config/msedit/settings.json`
  if `XDG_CONFIG_HOME` isn't set.

Settings are read when `edit` starts, so restart it after a change.

If a value is invalid (for example `"spellcheck.underline": "wavy"`), `edit` shows an error
such as "Invalid Settings: spellcheck.underline" and stops reading the file at that key. The keys
are read in this order: `files.associations`, `spellcheck.enabled`, `spellcheck.underline`. Keys
before the invalid one take effect, and the ones after it keep their defaults. Invalid JSON keeps
every default.

### Terminals without curly underlines

The curly underline uses the SGR sequences `4:3` and `58`. Ghostty, for example, supports them.
If misspelled words show no underline or the wrong one, set `"spellcheck.underline": "plain"`.
That's likely needed in tmux without the `usstyle` feature, GNU screen, the Linux console, and
older Windows console hosts (conhost).

To let tmux (3.2 or later) pass curly underlines through to Ghostty, add this to `~/.tmux.conf`
(not tested here):

```
set -as terminal-features ",xterm-ghostty:usstyle"
```

The `terminal-features` option and its `usstyle` feature ("Allows underscore style and colour to be
set") are documented in [tmux(1)](https://man.openbsd.org/tmux.1). The `set -as terminal-features`
form follows the [tmux FAQ](https://github.com/tmux/tmux/wiki/FAQ). `xterm-ghostty` is Ghostty's
default `TERM`. For another terminal, use its `TERM` value, and only if it supports these underlines.

## Cmd+. on macOS (Ghostty)

Terminals don't pass Cmd shortcuts to programs. To make **Cmd+.** send F7 in Ghostty, add this line to
Ghostty's config file:

```
keybind = cmd+period=csi:18~
```

The config file is `~/.config/ghostty/config.ghostty`, or on macOS
`~/Library/Application Support/com.mitchellh.ghostty/config.ghostty` (see `ghostty +edit-config --help`).
Reload the config (Cmd+Shift+, by default) or restart Ghostty. Cmd+. then sends F7 in every program
running in Ghostty, not only in `edit`.

`csi:18~` sends `ESC [ 18 ~`, which is F7. To check the line, put it in a file on its own and run
`ghostty +validate-config --config-file=<file>`.

## Limitations

- English (en_US) only. There's no personal dictionary or "ignore word", so names and jargon get
  flagged.
- There's no command to jump to the next or previous misspelling.
- While the Find bar has focus, F7 and Ctrl+T do nothing. Use Edit > Spelling Suggestions… instead.
- Right-click does nothing while a dialog is open. With a menu open, a right-click closes the menu
  and then acts on the text.
- Code examples in comments, such as Rust doc comments, are checked as prose.
- JavaScript template literals and Go raw strings (`` `…` ``) aren't checked if they start and end
  on the same line. The lines of a multi-line one are checked.
- Inside a string literal, `\` is always read as an escape, even where the language doesn't
  treat it as one (PowerShell `"…"`, SQL, shell `'…'`, C# `@"…"`, Rust `r"…"`). So a Windows path
  in such a string can be flagged in pieces. For example, `"C:\Program Files\Microsoft Office"`
  flags `icrosoft`. A path that starts with a drive letter and has no spaces (`"C:\Users\me"`) is
  skipped.
- HTML is highlighted as XML, so only its comments are checked. A comment that starts and ends on
  the same line (`<!-- … -->`) isn't checked at all.
- Markdown: a fence inside a block quote (`> ```` ``` ````), or indented by 4 or more spaces inside a
  list item, isn't recognized as a fence, so the block's contents may be checked. A line indented
  by 4 or more spaces right after a heading is checked as text.
- Lines longer than 16 KiB and words longer than 48 bytes aren't checked.
- A misplaced accent is accepted (`cafè`). Suggestions never contain accents.
- The Find, Replace and Go to Line inputs are never checked.

## Licenses

The spellbook engine (MPL-2.0), its dependencies, and the dictionary (SCOWL, MIT and BSD) are listed in
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md), with the dictionary's own license in
[assets/dictionaries/en_US/license](assets/dictionaries/en_US/license).
