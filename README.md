# Genesi Code

An IDE and terminal in one app, built in Rust. Genesi Code is a fork of the
open-source Warp client, reworked into a code editor: a project explorer, a
tree-sitter editor with language-server support, inline AI completion backed by
a local model, and the terminal it grew out of.

## Building and running

```bash
./script/bootstrap   # platform-specific setup (once)
./script/run         # build and run
./script/presubmit   # fmt, clippy, and tests
```

On Windows, use `script/run-windows.ps1`.

Builds are memory-hungry. A link step that dies with `0xc0000409` and no other
diagnostic ran out of memory — give the machine more swap rather than looking
for the mistake in your code.

## Layout

| Path | What lives there |
| --- | --- |
| `app/src/code/` | The IDE: editor view, file tree, language servers, AI completion |
| `app/src/code/editor/` | Buffer model, rendering, vim handling, find |
| `app/src/settings/` | Setting definitions |
| `app/src/settings_view/` | The settings UI |
| `crates/editor/` | The text buffer |
| `crates/syntax_tree/`, `crates/languages/` | tree-sitter grammars, highlight and indent queries |
| `crates/lsp/` | Language server client |
| `crates/warpui_core/`, `crates/warpui/` | The UI framework (retained-mode, MIT licensed) |

The `warp`-prefixed crate names are inherited from upstream and left alone
deliberately: renaming ~60 crates buys nothing and breaks everything.

## Licensing

The UI framework (the `warpui_core` and `warpui` crates) is licensed under the
[MIT license](LICENSE-MIT). The rest of the code is licensed under the
[AGPL v3](LICENSE-AGPL).

This project is a derivative of [Warp](https://github.com/warpdotdev/Warp) and
carries the same licenses.
