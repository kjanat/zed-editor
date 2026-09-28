---
title: Markdown
description: "Configure Markdown language support in Zed, including language servers, formatting, and debugging."
---

# Markdown

Markdown support is available natively in Zed.

- Tree-sitter:
  [tree-sitter-markdown](https://github.com/tree-sitter-grammars/tree-sitter-markdown)
- Language Server: N/A

## Syntax Highlighting Code Blocks

Zed supports language-specific syntax highlighting of markdown code blocks by
leveraging [tree-sitter language grammars](../extensions/languages.md#grammar).
All [Zed supported languages](../languages.md), including those provided by
official or community extensions, are available for use in markdown code blocks.
All you need to do is provide a language name after the opening <kbd>```</kbd>
code fence like so:

````python
```python
import functools as ft

@ft.lru_cache(maxsize=500)
def fib(n):
    return n if n < 2 else fib(n - 1) + fib(n - 2)
```
````

## Configuration

### Preview Settings

All `markdown_preview` settings can be configured in user settings or overridden
in a project's `.zed/settings.json`. For example, use the full pane width for a
project with wide tables:

```json
{
  "markdown_preview": {
    "limit_content_width": false
  }
}
```

To constrain the preview to a different width, set `limit_content_width` to
`true` and `max_width` to the desired width in pixels. Omitted values inherit
from user settings, or from a parent directory's project settings. Changes
apply to open previews; untitled previews use user settings.

Projects can also override `font_family`, `code_font_family`, `font_size`,
`theme`, and `open_markdown_files_in_preview`. Automatic opening is decided
for each file when it is opened; changing that preference does not replace
editors that are already open.

Temporary preview zoom affects only that preview. Persistent font-size changes
update the nearest project settings file that specifies a preview font size,
or user settings when there is no project override. Persistent reset removes
that override so the inherited size applies again.

### Format

Zed supports using Prettier to automatically re-format Markdown documents. You
can trigger this manually via the {#action editor::Format} action or via the
{#kb editor::Format} keyboard shortcut. Alternately, you can enable format on
save.

Configure formatting in Settings ({#kb zed::OpenSettings}) under Languages >
Markdown, or add to your settings file:

```json [settings]
"languages": {
  "Markdown": {
    "format_on_save": "on"
  }
},
```

### List Continuation

Zed automatically continues lists when you press Enter at the end of a list
item. Supported list types:

- Unordered lists (`-`, `*`, or `+` markers)
- Ordered lists (numbers are auto-incremented)
- Task lists (`- [ ]` and `- [x]`)

Pressing Enter on an empty list item removes the marker and exits the list.

To disable this behavior, configure in Settings ({#kb zed::OpenSettings}) under
Languages > Markdown, or add to your settings file:

```json [settings]
"languages": {
  "Markdown": {
    "extend_list_on_newline": false
  }
},
```

### List Indentation

Zed indents list items when you press Tab while the cursor is on a line
containing only a list marker. This allows you to quickly create nested lists.

To disable this behavior, configure in Settings ({#kb zed::OpenSettings}) under
Languages > Markdown, or add to your settings file:

```json [settings]
"languages": {
  "Markdown": {
    "indent_list_on_tab": false
  }
},
```

### Trailing Whitespace

By default Zed will remove trailing whitespace on save. If you rely on invisible
trailing whitespace being converted to `<br />` in Markdown files you can
disable this behavior.

Configure in Settings ({#kb zed::OpenSettings}) under Languages > Markdown, or
add to your settings file:

```json [settings]
"languages": {
  "Markdown": {
    "remove_trailing_whitespace_on_save": false
  }
},
```
