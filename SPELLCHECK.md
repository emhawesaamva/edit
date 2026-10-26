# Spell checking

> Fork-only: this feature is not part of upstream microsoft/edit.

`edit` underlines misspelled words and offers replacements. It uses the
[spellbook](https://github.com/helix-editor/spellbook) engine with an en_US Hunspell dictionary
that is built into the binary, so there is nothing to install.

## What gets checked

| File | Checked |
|---|---|
| No syntax definition (`.txt`, `.log`, `.csv`, `Makefile`, …) | Every word |
| Markdown (`.md`) | Prose, headings, lists, emphasis, link text. Not: inline code, link URLs, fenced code blocks (```` ``` ```` and `~~~`) and their fence lines, indented code blocks, YAML front matter, HTML tags and single-line HTML comments |
| Git commit messages (`COMMIT_EDITMSG`, `MERGE_MSG`) | The message. Not: `#` lines, or the diff that `git commit -v` appends |
| Code (Rust, C, C++, C#, Go, Java, JavaScript, Python, PowerShell, shell, SQL, …) | Comments and string literals |
| Data formats (JSON, YAML, TOML, XML/HTML, INI/properties/git config, `.gitignore`, `git-rebase-todo`) | Comments only. In `git-rebase-todo`, commit subjects and `exec` lines count as comments and are checked, but git's `#` help text isn't |
| Diffs (`.diff`, `.patch`) | Nothing |

In Markdown, list items shift the indentation, as in CommonMark. A fence right after a list marker
(`` 1. ```bash ``) or indented to the item's text is recognized. It ends at its closing fence or, if
it's never closed, at the first non-blank line that's indented less than the item's text. Outside a
list, a fence that's never closed runs to the end of the file, and so can one after a nested list
item (whose text column isn't known exactly) or until the next fence line. A paragraph in an item after a blank
line is checked, and indented code in an item needs 4 more spaces than the item's text.

YAML front matter at the top of a Markdown file is skipped: a `---` first line that isn't followed
by a blank line, through the next `---` or `...` line. It ends early at the first line that can't be
YAML (one that isn't blank or indented, doesn't start with `#` or `-`, and has no `:`), even if a
`---` follows later. That line and everything after it are checked as Markdown. Front matter that's
never closed ends there, too.

Some text is never flagged:
- URLs, email addresses, file names (`main.rs`), and paths that start with `/`, `./`, `~/` or a
  drive letter (`C:\Users` or `C:/Users`).
- `` `code` `` spans, `<tags>` and `&entities;`.
- Words that contain digits or an inner `_`, camelCase words, ALL-CAPS words (also `APIs` and
  `API's`), letter plurals (`x's`), and single letters.
- Words with an escaped apostrophe (`isn\'t`), because a replacement would lose the escape.

Outside string literals, text with a `\` before a letter or digit is skipped too: relative paths
(`tools\build\release`), registry keys (`HKEY_CURRENT_USER\Software`) and LaTeX commands (`\alpha`).

Inside string literals:
- `\` starts an escape. Escapes (`\n`, `\x41`, `\u{41}`) separate words and aren't checked
  themselves.
- printf specs (`%lu`, `%02X`, Python's `%(name)s`) and format placeholders without spaces (`{}`,
  `{0}`, `{name}`, `{0:N2}`) aren't checked.
- Variables aren't checked: a word right after a `$` or `${` (`$user`, `${name}`, PowerShell's
  `${env:ProgramFiles(x86)}`).

PowerShell and SQL strings don't use `\` escapes, so they're read differently:
- `\` is read as outside strings, so paths, registry keys and shares
  (`'C:\Program Files\Microsoft Office'`, `'HKLM:\SOFTWARE\Microsoft'`, `'\\server\share'`) aren't
  flagged.
- PowerShell's backtick escapes, a backtick before ASCII punctuation or one of `0 a b e f n r t u v`
  (`` `n ``, `` `t ``, `` `$ ``), separate words and aren't checked. Before any other letter,
  PowerShell keeps the letter as it is, so the backtick is read as punctuation: in
  ``"Run `Get-Help` for help"``, `Get` and `Help` are checked.
- A word with a doubled apostrophe (`'isn''t'`) is skipped, like `isn\'t`.
- T-SQL's Unicode strings (`N'Hello world'`) are read without the `N`.
- PowerShell's scoped variables (`$env:windir`, `$script:name`) aren't checked.
- In SQL, a backtick is never an escape. MySQL's backtick-quoted names (`` `Customers` ``) are
  skipped like code spans, in strings, too (``'Foreign key to `customers`.`id`'``).

Accented spellings of dictionary words are accepted (`café`, `naïve`, `résumé`).

## Add-on dictionary

About 320 common words that the en_US dictionary lacks, mostly from software work, are never
underlined and show up in suggestions: `ok`, `config`, `repo`, `todo`, `backend`, `localhost`,
`rebase`, `symlink`, `stdout`, `json`, `Kubernetes`, `Ghostty` and so on, with their plurals and
other forms (`configs`, `repo's`, `rebased`, `symlinking`).

The list is [assets/dictionaries/en_US-addon.dic](assets/dictionaries/en_US-addon.dic), one word per
line. To add a word, add a line and rebuild `edit`. The file's header explains the format, including
the flags for plurals and verb forms (`repo/SM`, `rebase/DGS`). A word in lowercase also accepts its
capitalized and all-caps forms; a capitalized word only those. The tests reject a word that the
en_US dictionary already knows, a duplicate, or a malformed line.

## When underlines appear

A word is underlined once you've finished it, after you type a space or punctuation, or move the
cursor away. The word you are typing isn't flagged mid-word. Undo and redo don't count as typing.

The dictionary loads in the background when `edit` starts, and underlines appear as soon as it's ready.

## Fixing a word

Each of these opens a list of up to 8 suggestions for the misspelled word at the cursor, or the
one that ends right before the cursor. The list opens below the word, or above it if it only fits
there.
- **F7** or **Ctrl+T** (these work while the document has focus). On a Mac keyboard, F7 needs
  **Fn** (Fn+F7) unless its F keys are set to work as standard function keys. The
  [Cmd+. mapping](#cmd-on-macos-ghostty) below avoids that.
- **Edit > Spelling Suggestions…**
- **Right-click** (a two-finger click on a trackpad) on the misspelled word. This moves the cursor
  there and opens the list. Right-clicking a correctly spelled word only moves the cursor.

In the list:
- **Up/Down** to choose, **Enter** or a **double-click** to replace. The replacement is one undo
  step, so **Ctrl+Z** brings the old word back.
- **Esc** or a click elsewhere closes the list. It also closes if you switch documents, resize the
  terminal, scroll the word out of view, or move the cursor off the word (for example with **F3**
  Find Next).

Instead of suggestions, the list may show one of these messages: "No suggestions", "No misspelled
word at cursor", "Loading dictionary…", or "Spell check unavailable". A message closes as soon as the
cursor moves.

In a diff or patch, where nothing is checked, F7, Ctrl+T and Edit > Spelling Suggestions show
nothing, and a right-click only moves the cursor.

**View > Spell Check** turns spell checking on or off until you quit. It isn't saved. While it's
off, the shortcuts do nothing and the Edit menu item is hidden. A right-click still moves the cursor
and gives the document the focus.

## Settings

Add these keys to `settings.json`:

```json
{
    "spellcheck.enabled": true,
    "spellcheck.underline": "auto"
}
```

| Key | Values | Default |
|---|---|---|
| `spellcheck.enabled` | `true`, `false`. With `false`, the dictionary isn't loaded until you turn on View > Spell Check. | `true` |
| `spellcheck.underline` | `"auto"` (plain in macOS Terminal, curly elsewhere), `"curly"` (red curly underline), `"plain"` (normal underline) | `"auto"` |

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

The curly underline uses the SGR sequences `4:3` and `58`. Ghostty, for example, supports them, and
macOS Terminal doesn't, so with `"auto"` it gets a plain underline (`edit` checks for
`TERM_PROGRAM=Apple_Terminal`). If misspelled words show no underline or the wrong one in another
terminal, set `"spellcheck.underline": "plain"`.
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

## Right-click in Ghostty

Ghostty's `right-click-action` setting (default `context-menu`) doesn't get in the way. According
to Ghostty 1.3.1's source (`mouseButtonCallback` in `src/Surface.zig`), while a program has mouse
reporting on, as `edit` does, Ghostty reports the right-button press to the program and doesn't
run its right-click action, so no context menu appears. Shift+right-click still goes to Ghostty
and opens its context menu, because `mouse-shift-capture` defaults to `false`. (This comes from
reading the source; it isn't covered by tests here.)

## Limitations

- English (en_US) only. There's no personal dictionary or "ignore word", so names and jargon that
  aren't in the [add-on dictionary](#add-on-dictionary) get flagged. Adding a word there needs a rebuild.
- There's no command to jump to the next or previous misspelling.
- While the Find bar has focus, F7 and Ctrl+T do nothing. Use Edit > Spelling Suggestions… instead.
- Right-click does nothing while a dialog is open. With a menu open, a right-click closes the menu
  and then acts on the text.
- Code examples in comments, such as Rust doc comments, are checked as prose.
- JavaScript template literals and Go raw strings (`` `…` ``) aren't checked if they start and end
  on the same line. The lines of a multi-line one are checked.
- Inside a string literal, `\` is read as an escape (except in PowerShell and SQL), even where the
  language doesn't treat it as one (shell `'…'`, C# `@"…"`, Rust `r"…"`). So a Windows path in such
  a string can be flagged in pieces. For example, `"C:\Program Files\Microsoft Office"` flags
  `icrosoft`. A path that starts with a drive letter and has no spaces (`"C:\Users\me"`) is
  skipped.
- In PowerShell and SQL strings, it's the other way around: a MySQL escape (`'Helo\nWorld'`) makes
  the text around it read as a path, so it isn't checked.
- PowerShell's single-quoted strings have no escapes, but they're read like double-quoted ones. So
  a backtick before one of the escape letters still hides that letter: ``'Open `notepad` first'``
  flags `otepad`.
- In string literals, a word in braces without spaces (`"{Teh}"`) is taken for a format placeholder
  and isn't checked, and neither is Rust's `{{literal}}`.
- In `git-rebase-todo`, `exec` lines are checked as prose, so command names and options
  (`--relese`) can be flagged.
- HTML is highlighted as XML, so only its comments are checked. A second comment on the same line
  right after another one (`<!-- a --><!-- b -->`) isn't checked. In Markdown, a comment that starts
  and ends on the same line isn't checked at all, so that directives such as
  `<!-- markdownlint-disable -->` aren't flagged.
- Markdown: a fence inside a block quote (`> ```` ``` ````) isn't recognized as a fence, so the
  block's contents may be checked. A line indented by 4 or more spaces right after a heading is
  checked as text. Only the innermost list item's indentation is tracked, so in unusual nesting a
  line can be taken for code or text when it's the other. A tab right after a list marker
  (`-<Tab>text`) counts as a single space, so a paragraph in that item that's indented by 6 or 7
  spaces can be taken for code and isn't checked.
- Lines longer than 16 KiB and words longer than 48 bytes aren't checked.
- A misplaced accent is accepted (`cafè`). Suggestions never contain accents.
- The Find, Replace and Go to Line inputs are never checked.

## Licenses

The spellbook engine (MPL-2.0), its dependencies, and the dictionary (SCOWL, MIT and BSD) are listed in
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md), with the dictionary's own license in
[assets/dictionaries/en_US/license](assets/dictionaries/en_US/license). The add-on dictionary is part
of this fork and under its MIT license.
