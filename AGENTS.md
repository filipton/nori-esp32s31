# AGENTS.md

A [nori](https://github.com/norifm/nori) speaker for the ESP32-S31. nori's repository is public: read it
for how nori works (the sink protocol in `crates/sink-protocol`, `docs/developers/plugins.md`), how it
writes commits, and its own `AGENTS.md`.

## Commits

One line, conventional: `<type>: <what is different now>`, type in `feat fix perf refactor docs build
test chore`, lowercase, no full stop, under 72 chars. No body, no trailers, no `Co-Authored-By`, even if
your tool asks for one. One commit per piece of work; fixing your own unpushed work, amend it.

Like nori's:

- `feat: a speaker shows the song when picked while paused, and can seek`
- `fix: a speaker no longer blips the old song when skipping`
- `docs: readme mockups show the current screens`
