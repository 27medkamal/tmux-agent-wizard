# Changes taw made to nucleo-matcher

This is nucleo-matcher 0.3.1 (MPL-2.0, https://github.com/helix-editor/nucleo) as the crates.io
registry ships it: `src/`, `Cargo.toml`, `LICENSE` and `README.md`. The fuzz script and the case
fold table generator are left out, as the build does not need them. taw depends on this copy by
path (see the root `Cargo.toml`). Every changed file keeps its MPL-2.0 header and licence, and
every change is marked with a comment that says it is taw's.

## Changes

1. `src/lib.rs`, `Matcher::exact_match_impl`, the ASCII text and ASCII term arm when case
   matters: upstream compared `haystack == needle`, the whole text, instead of the slice
   `[start..end]`. A prefix (`^Web`), a postfix (`Api$`) or an exact term with a capital in it
   (smart case) matched only a text that was the term itself, and an inverse one (`!^Web`)
   hid nothing. The case-ignoring arm already sliced.
2. `src/exact.rs`, `Matcher::substring_match_non_ascii`: upstream computed the end of the places
   a term can start as `haystack.len() - needle.len()` and sliced `haystack[start..end]` with
   it, an end the range leaves out, so the last place was never tried. A substring term (`'pi`)
   at the very end of a text that holds any non-ASCII character (`±1 api`, an accented
   letter, an emoji) never matched, and an inverse one (`!'pi`) there never
   hid the text. The end is now one past the last start.
3. `Cargo.toml`: a `[lints.rust]` table allows `mismatched_lifetime_syntaxes` and
   `unnecessary_transmutes`, two rustc lints newer than this release that warn on three lines of
   its code (two in `src/utf32_str.rs`, one in `src/chars/normalize.rs`), so that taw's builds
   print no warnings. No code changed for it.
