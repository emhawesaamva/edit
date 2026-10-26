# Third-party notices (fork-only)

This file is fork-only; it isn't part of upstream microsoft/edit. The fork's spell check
feature compiles the following into the `edit` binary, in addition to upstream's own dependencies.

The crate versions below are the ones in `Cargo.lock`. Cargo may pick other versions after
`cargo update`: `spellbook = "0.4.2"` in `crates/edit/Cargo.toml` allows any 0.4.x, the fork's own
`foldhash = "0.2"` (for spellbook's fixed hash seed) any 0.2.x, and spellbook itself accepts
`hashbrown >= 0.15` and `foldhash >= 0.1`. Recheck this file after `cargo update`;
`cargo tree -p edit -e normal` lists what gets compiled in.

## spellbook 0.4.2

* License: MPL-2.0 (Mozilla Public License, version 2.0). The full text is at
  https://mozilla.org/MPL/2.0/ and in the `LICENSE` file of the crate.
* Used unmodified, as a crates.io dependency.
* Source code of this version: https://crates.io/crates/spellbook/0.4.2
  (repository: https://github.com/helix-editor/spellbook).

## hashbrown 0.17.1

* License: MIT OR Apache-2.0. Used under the MIT license, reproduced below from the crate's
  `LICENSE-MIT`. The Apache-2.0 text is in the crate's `LICENSE-APACHE`.
* Source code: https://crates.io/crates/hashbrown/0.17.1
  (repository: https://github.com/rust-lang/hashbrown).

```text
Copyright (c) 2016 Amanieu d'Antras

Permission is hereby granted, free of charge, to any
person obtaining a copy of this software and associated
documentation files (the "Software"), to deal in the
Software without restriction, including without
limitation the rights to use, copy, modify, merge,
publish, distribute, sublicense, and/or sell copies of
the Software, and to permit persons to whom the Software
is furnished to do so, subject to the following
conditions:

The above copyright notice and this permission notice
shall be included in all copies or substantial portions
of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF
ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED
TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A
PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT
SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION
OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR
IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
DEALINGS IN THE SOFTWARE.
```

## foldhash 0.2.0

* License: Zlib, reproduced below from the crate's `LICENSE`.
* Source code: https://crates.io/crates/foldhash/0.2.0
  (repository: https://github.com/orlp/foldhash).

```text
Copyright (c) 2024 Orson Peters

This software is provided 'as-is', without any express or implied warranty. In
no event will the authors be held liable for any damages arising from the use of
this software.

Permission is granted to anyone to use this software for any purpose, including
commercial applications, and to alter it and redistribute it freely, subject to
the following restrictions:

1. The origin of this software must not be misrepresented; you must not claim
    that you wrote the original software. If you use this software in a product,
    an acknowledgment in the product documentation would be appreciated but is
    not required.

2. Altered source versions must be plainly marked as such, and must not be
    misrepresented as being the original software.

3. This notice may not be removed or altered from any source distribution.
```

## en_US Hunspell dictionary (SCOWL 2020.12.07)

* Files: `assets/dictionaries/en_US/index.aff` and `index.dic`, bundled into the binary.
* License: (MIT AND BSD). The full notices, including the copyright lines, are in
  [`assets/dictionaries/en_US/license`](assets/dictionaries/en_US/license).
* A binary distribution must include `assets/dictionaries/en_US/license` alongside this file.
* [`assets/dictionaries/en_US/README.md`](assets/dictionaries/en_US/README.md) records where the
  files come from (wooorm/dictionaries at a pinned commit), the SCOWL version and their SHA-256 hashes.
