# What this fork adds

This is a personal fork of [microsoft/edit](https://github.com/microsoft/edit). Everything below is
fork-only and isn't part of the upstream editor.

## Spell checking

Misspelled words get a red curly underline, with suggestions on F7, Ctrl+T or a right-click.
See [SPELLCHECK.md](SPELLCHECK.md).

## Remembered View choices

`edit` remembers the last choice for **View > Word Wrap** (Alt+Z) and **View > Spell Check**:
- Word wrap: every document you open starts with word wrap on or off, as you last set it.
  (Alt+Z still switches only the current document.)
- Spell check: `edit` starts with it on or off, as you last set it. A `"spellcheck.enabled"` in
  `settings.json` wins over the remembered choice.

## Reopen files at startup

Turn on **File > Reopen Files at Startup**, and starting `edit` without files reopens the files
that were open last time, with their cursor positions. The one you had in front is in front again.
- Files on the command line, or text piped into `edit`, win: `edit notes.md` opens just that
  file. Whatever you then have open is what comes back next time.
- Only saved files come back, as they are on disk. Untitled documents and unsaved changes don't,
  and files that were deleted or moved are skipped.
- The list is saved as you open and close files, so it's kept even when you close the terminal
  window instead of quitting with File > Exit. Cursor positions are saved as you work (at most once
  a second) and when you quit; after closing the window instead, a cursor may come back slightly
  off.
- Turning the option off forgets the list.

## Where these are saved

Both are saved next to `settings.json` (see [SPELLCHECK.md](SPELLCHECK.md#settings) for where that
is), in files that `edit` writes itself. It never changes `settings.json`.
- `view-state.json`: the View choices and whether Reopen Files at Startup is on. Delete it to
  forget them.
- `session.json`: the files to reopen, only while the option is on.

## Replace All fixes

Replace All could freeze the editor (losing unsaved changes) when the last match was at the very end
of a file without a final newline and the replacement matched the search again (`foo` to `Foo`,
since the search ignores case), or when replacing an empty regex match with nothing (`[ \t]*$` to
strip trailing spaces). This fork carries a fix until upstream has one
([microsoft/edit#994](https://github.com/microsoft/edit/issues/994)).

## Building and installing

With [Rust](https://rustup.rs) installed, from this folder:

```sh
cargo install --path crates/edit --locked
```

That installs `edit` to `~/.cargo/bin`. Run the same command again after pulling changes, and
`cargo uninstall edit` to remove it.
