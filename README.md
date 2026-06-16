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

## Install

Prebuilt binaries (no toolchain needed) ship with each tagged release. Download
the archive for your platform from the [Releases](https://github.com/hydrate-sh/cli/releases)
page, check it against its published `.sha256`, and put `hydrate` (and the `hyd`
alias) on your `PATH`:

```sh
# Linux x86_64 — adjust the version and target for your platform.
tag=v0.1.0
target=x86_64-unknown-linux-gnu
curl -fsSLO "https://github.com/hydrate-sh/cli/releases/download/${tag}/hydrate-${tag}-${target}.tar.gz"
curl -fsSLO "https://github.com/hydrate-sh/cli/releases/download/${tag}/hydrate-${tag}-${target}.tar.gz.sha256"
sha256sum -c "hydrate-${tag}-${target}.tar.gz.sha256"
tar xzf "hydrate-${tag}-${target}.tar.gz"
./hydrate --version
```

Each release publishes archives for Linux (x86_64, aarch64), macOS (x86_64,
aarch64), and Windows (x86_64), each with a `.sha256` checksum. The archives
also carry signed build provenance — verify it with
`gh attestation verify <archive> --repo hydrate-sh/cli`.

## Building from source

Requires the Rust toolchain pinned in `rust-toolchain.toml`.

```
cargo build
cargo test
```

## Configuration

- `HYD_API_KEY` — your API key (read from the environment or a `.env` file).
- `HYD_BASE_URL` — override the service URL (for local development).

## License

MIT — see [`LICENSE`](LICENSE).
