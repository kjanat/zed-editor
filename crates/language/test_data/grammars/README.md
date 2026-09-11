# Grammar ownership regression fixtures

These MIT-licensed Tree-sitter WASM parsers reproduce the incompatible `html`
grammar registrations in issue #89. Both export `tree_sitter_html`; the Jinja
parser additionally recognizes the `jinja` node used by HTML-Jinja injections.
Tests copy each fixture to a separate directory as `html.wasm`, matching the
extension loader's filename convention. No downloads are needed to run them.

| Fixture | Source revision | Zed extension | SHA-256 |
| --- | --- | --- | --- |
| `html.wasm` | [tree-sitter/tree-sitter-html, bfa075d](https://github.com/tree-sitter/tree-sitter-html/tree/bfa075d83c6b97cd48440b3829ab8d24a2319809) | HTML 0.3.1 | `95739db458e3ff9c8ec162295606b3844562c0c0a2b5b1b3f1b3a650b3596fb1` |
| `html-jinja.wasm` | [JaagupAverin/tree-sitter-html, 5f47d66](https://github.com/JaagupAverin/tree-sitter-html/tree/5f47d6608069f4ef35d836dc0083c90733c72810) | HTML-Jinja 0.1.0 | `7624eae99344952ae74404eaeeded2ed52469bd4d24964386ef1bc7f4d6da4b4` |

The files are the parsers distributed with those extension versions. To rebuild
from source, check out the corresponding revision and run `tree-sitter build
--wasm`; toolchain changes can change the binary checksum. Both repositories use
the accompanying `LICENSE`.
