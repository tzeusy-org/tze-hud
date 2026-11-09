# Checked JSON examples

Every fenced JSON block in `th-hud-publish` and `hud-projection`, including
their Markdown references, needs an annotation on the line immediately above
its opening fence:

- `<!-- tool: hud_publish -->` (or another of the five HUD tools) marks a
  complete tool argument object. The MCP crate's `skill_examples` test reads
  it through the same argument types and parser as production. Runtime resource
  existence, permissions and content admission are separate from argument parsing.
- `<!-- example: response -->` or `<!-- example: error -->` marks a returned
  value. `<!-- example: partial -->` marks a fragment, protocol envelope or
  another application's configuration. These are not parsed as HUD arguments.

Missing annotations, unknown tool names, invalid argument JSON and argument
type errors fail the existing required Cargo test gate with the file and line.
This is one behavioral gate; there is no parallel source-grep check.

The token-footprint artifact also reports the full two `SKILL.md` files' UTF-8
bytes and pinned o200k tokens, including frontmatter, plus model tool calls per
canonical flow. These measurements are report-only until the later skill diet.
Protocol `tools/list` counts zero model tool calls; each `tools/call` counts one.
