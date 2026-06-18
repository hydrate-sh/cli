# hydrate CLI

Command-line client for [hydrate.sh](https://hydrate.sh) — author your graph
from the terminal.

## What it is

`hydrate` is a thin client over the hydrate.sh `/v1` API. The CLI stages edits
locally and commits them as one typed delta batch under optimistic concurrency
control; the **server is the sole authority for validation**. It does not mirror
the server's rules; a bad batch is rejected by the server.

The binary is `hydrate`, with a short alias `hyd`.

## Command surface

```
hydrate fork <name>          Fork a working branch from main, bind this directory to it
hydrate branches             List your working branches
hydrate pull                 Refresh the local view of the branch's graph
hydrate node add ...         Stage a node (behavior or boundary)
hydrate node set <path> ...  Stage an edit to a node (description, ports, ...)
hydrate node mv <path> ...   Stage a reparent of a node
hydrate node rm <path>...    Stage removal of nodes (cascades the subtree)
hydrate edge add ...         Stage an edge between two typed ports
hydrate edge rm ...          Stage removal of an edge
hydrate boundary flatten ... Promote a boundary's children and remove it
hydrate clear                Stage removal of every top-level node
hydrate status               Show the bound branch + staged-operation summary
hydrate diff                 Show staged operations in detail
hydrate commit               Commit the staged changeset to the bound branch
```

Run `hydrate guide` for an orientation, or see the full reference at
[docs.hydrate.sh](https://docs.hydrate.sh).

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
tag=v0.1.5
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
