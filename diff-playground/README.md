# Diff playground

Deliberately fake content for exercising a diff viewer.

## Things worth checking

- New files show their entire contents as additions.
- The removed release workflow shows only deletions.
- `crates/store/src/settings.rs` contains deletion-only hunks.
- `crates/app/src/http.rs` contains a one-number replacement.
- `crates/app/src/theme.rs` contains separated changes.
- `crates/diff-core/src/lib.rs` contains a large inserted block.
- The root README contains a large replacement and smaller distant edits.
- JSON, Rust, Markdown, YAML, CSV, and HTML get different highlighting.

## Unicode samples

English: the goose reviews the moon.
French: l’oie révise le café.
Japanese: ガチョウがコードをレビューします。
Emoji: 🪿 🌙 🧀 ✨

## A deliberately long line

This line keeps wandering across the screen because the imaginary goose has decided that horizontal scrolling deserves a little exercise, so here are some extra words about lunar cheese, suspicious umbrellas, unreasonably enthusiastic code reviewers, and the peculiar feeling that a semicolon has just migrated south for the winter without telling the rest of the syntax tree where it plans to land.
