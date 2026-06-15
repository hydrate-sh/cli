# hydrate CLI

Command-line client for [hydrate.sh](https://hydrate.sh) — author your system
graph from the terminal.

> **Status: early scaffold.** The command surface is in place; the verbs are
> stubs while transport and authoring land. Not yet usable for real work.

## What it is

`hydrate` is a thin client over the hydrate.sh `/v1` API. The graph is the source
of truth and the **server is the sole authority for validation** — the CLI stages
edits locally and commits them as one typed delta batch under optimistic
concurrency control. It does not mirror the server's rules; a bad batch is
rejected by the server, loudly.

The binary is `hydrate`, with a short alias `hyd`.

## Command surface

```
hydrate fork <name>          Fork a working branch from main, bind this directory to it
hydrate branches             List your working branches
hydrate node add ...         Stage a node (behavior or boundary)
hydrate edge add ...         Stage an edge between two typed ports
hydrate status               Show the bound branch + staged-operation summary
hydrate diff                 Show staged operations in detail
hydrate commit               Commit the staged changeset to the bound branch
```

Authoring is flag-driven and explicit, so a command reads the same in a script
as on the terminal:

```
hydrate node add --kind behavior --name Rater --in raw:HotDog --out score:Score
hydrate edge add --from Maker.dog --to Rater.raw
hydrate commit
```

## Building from source

Requires the Rust toolchain pinned in `rust-toolchain.toml`.

```
cargo build
cargo test
```

Prebuilt binaries (no toolchain needed) will ship via GitHub Releases.

## Configuration

- `HYD_API_KEY` — your API key (read from the environment or a `.env` file).
- `HYD_BASE_URL` — override the service URL (for local development).

## License

MIT — see [`LICENSE`](LICENSE).
