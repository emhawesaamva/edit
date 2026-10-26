# en_US Hunspell dictionary

Bundled into the editor binary for the fork-only spellcheck feature (see `crates/edit/src/spellcheck/`).

* Source: https://github.com/wooorm/dictionaries/tree/8cfea406b505e4d7df52d5a19bce525df98c54ab/dictionaries/en
  (npm package `dictionary-en` 4.0.0)
* Upstream: SCOWL en_US Hunspell dictionary, version 2020.12.07 (http://wordlist.aspell.net/dicts/)
* License: (MIT AND BSD), see `license` in this directory.

| File        | SHA-256                                                            |
|-------------|--------------------------------------------------------------------|
| `index.aff` | `8ae1f19d4840d957728ad90555d5a8dff6cc5c046279c95ff0c00fc0a0136c7b` |
| `index.dic` | `f0b1a234bd178bdd01875b2a392a9647f888b8fe879f79c52aae62c2759b3647` |
| `license`   | `2a7e8d8ae9e8facc84818546ae2a8d83aec5e9c80a675ff789acd1c338b53b3d` |

To update, download the three files from a newer commit of the repository above,
verify they are valid UTF-8 (`include_str!` requires it), and update this table.
