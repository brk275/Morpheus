# Emoji catalog

Morpheus uses the already locked `emojis` **0.9.0** catalog: Unicode **17.0**, with names and GitHub shortcodes from gemoji **4.1.0**. The direct dependency is exact and disables optional features. The catalog contains **3,944 canonical entries** across nine groups: 1,914 default entries and 2,030 non-default skin-tone variants, including mixed-tone pairs. The library's default iterator excludes tones; Morpheus explicitly expands `skin_tones()` and deduplicates canonical strings. These counts come from the pinned generated source, not a claim about future Unicode releases or every possible text representation.

## Behavior and limits

- The existing 44 starter choices retain their order, keys, labels and familiar aliases. Remaining entries follow the library's CLDR order, with related tone variants together.
- Search accepts English names, GitHub shortcodes (including optional surrounding colons), existing aliases, and complete emoji glyphs. Hyphens/underscores are treated as word separators; all query terms must match. Toned entries inherit their default entry's aliases and have their own named tone. Glyph search uses the library's qualified/unqualified normalization and selects one canonical row.
- Search is offline and does not send or retain a query history. One immutable process-wide index contains only compiled catalog metadata; it contains no account data. Results remain in the existing virtualized list. The limit is 4,096 entries, 64 UTF-8 bytes per query, 64 bytes per emoji and 1,024 bytes per precomputed index entry; control-character queries are rejected. The pinned data's longest emoji is 35 bytes and longest index entry is 122 bytes.
- Choosing a row still uses the existing exact-row, account-selection, room/thread, composition revision, caret/selection and IME checks. It inserts literal text, never a mention or an automatic send. This expansion does not alter insertion or keyboard handlers.
- This is Unicode text, not an embedded image/font set. Actual glyph appearance, combined sequence rendering and support for new Unicode 17 entries depend on installed fonts and platform rendering. Names remain readable when a font cannot render a symbol. Search is English, without transliteration, fuzzy matching, localized names, categories UI, favorites, custom room emoji or stickers. No automatic `:shortcode:` replacement is introduced.

Five catalog/search regression sources cover complete canonical/tone coverage, stable starter aliases/order, broader names/shortcodes, mixed tones and literal glyph lookup, and rejected queries. Native compilation, these Rust tests, picker performance at full catalog size, keyboard/screen-reader navigation and font behavior remain hosted/manual verification gates. Source inspection or catalog counts do not establish those runtime results.

## Provenance and license notices

- Upstream: [rossmacarthur/emojis](https://github.com/rossmacarthur/emojis), crate 0.9.0, recorded source commit `7367564e64297773e802df60b69280a76b9dee43`.
- Crates.io archive SHA-256: `0b1514ced566c94991ed9563258ecf61e311fd773bf0a3f20606830386bf66e3` (already in Cargo.lock).
- Declared license: **(MIT OR Apache-2.0) AND Unicode-3.0**. The following upstream MIT and Unicode notices are retained verbatim. Applicable notices must accompany distributed packages; this source document alone does not establish completion of the package license inventory.

### Upstream MIT notice

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.

### Upstream Unicode notice

UNICODE LICENSE V3

COPYRIGHT AND PERMISSION NOTICE

Copyright © 1991-2023 Unicode, Inc.

NOTICE TO USER: Carefully read the following legal agreement. BY
DOWNLOADING, INSTALLING, COPYING OR OTHERWISE USING DATA FILES, AND/OR
SOFTWARE, YOU UNEQUIVOCALLY ACCEPT, AND AGREE TO BE BOUND BY, ALL OF THE
TERMS AND CONDITIONS OF THIS AGREEMENT. IF YOU DO NOT AGREE, DO NOT
DOWNLOAD, INSTALL, COPY, DISTRIBUTE OR USE THE DATA FILES OR SOFTWARE.

Permission is hereby granted, free of charge, to any person obtaining a
copy of data files and any associated documentation (the "Data Files") or
software and any associated documentation (the "Software") to deal in the
Data Files or Software without restriction, including without limitation
the rights to use, copy, modify, merge, publish, distribute, and/or sell
copies of the Data Files or Software, and to permit persons to whom the
Data Files or Software are furnished to do so, provided that either (a)
this copyright and permission notice appear with all copies of the Data
Files or Software, or (b) this copyright and permission notice appear in
associated Documentation.

THE DATA FILES AND SOFTWARE ARE PROVIDED "AS IS", WITHOUT WARRANTY OF ANY
KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT OF
THIRD PARTY RIGHTS.

IN NO EVENT SHALL THE COPYRIGHT HOLDER OR HOLDERS INCLUDED IN THIS NOTICE
BE LIABLE FOR ANY CLAIM, OR ANY SPECIAL INDIRECT OR CONSEQUENTIAL DAMAGES,
OR ANY DAMAGES WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS,
WHETHER IN AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION,
ARISING OUT OF OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THE DATA
FILES OR SOFTWARE.

Except as contained in this notice, the name of a copyright holder shall
not be used in advertising or otherwise to promote the sale, use or other
dealings in these Data Files or Software without prior written
authorization of the copyright holder.
