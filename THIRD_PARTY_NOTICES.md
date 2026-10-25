# Third-Party Notices

clum's source code is MIT-licensed. The command knowledge base
(`crates/clum-command-kb/data/`) is derived from third-party projects whose
licenses require attribution:

| Source | Used for | License |
|---|---|---|
| safe-chains | `data/generated.toml` (bulk command risk levels) | MIT OR Apache-2.0 |
| gemini-cli | read-only / dangerous command annotations in `data/commands.toml` | Apache-2.0 |
| prodagent | read-only / mutating annotations in `data/commands.toml` | Apache-2.0 OR MIT |

> Upstream repository URLs are **not recorded in this checkout**. Maintainers
> must add them here (and preserve the upstream license texts) before
> redistributing binaries built from this repository.

The generated file records the upstream version in its header
(`# source-version: <commit>`). If that value is `unknown`, the snapshot was
not anchored to an upstream revision — regenerate with an explicit version
before relying on provenance. When run against the same upstream checkout,
regeneration is deterministic.

> **Current status: the bundled `crates/clum-command-kb/data/generated.toml`
> records `source-version: unknown`.** It is therefore unanchored and must be
> regenerated with an explicit upstream version before any binary containing it
> is redistributed. `scripts/kb-fuse-safe-chains.py` now refuses to emit an
> `unknown` version unless `--allow-unknown-source` is passed explicitly.

Apache-2.0 requires preservation of copyright and license notices. If you
redistribute a binary built from this repository, include this file and the
upstream license texts.
