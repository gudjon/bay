<h1 align="center">bay</h1>

<p align="center">
  <strong>The landing layer for agent fleets.</strong><br>
  Your agents submit once. bay lands exactly what was checked.
</p>

<p align="center">
  <a href="https://getbay.io">Website</a> ·
  <a href="https://getbay.io/docs">Docs</a> ·
  <a href="https://getbay.io/docs/install">Install</a> ·
  <a href="https://getbay.io/docs/quickstart">Quickstart</a> ·
  <a href="https://getbay.io/blog">Blog</a>
</p>

<p align="center">
  <a href="LICENSE"><img alt="MIT licence" src="https://img.shields.io/badge/licence-MIT-187571"></a>
  <img alt="Written in Rust" src="https://img.shields.io/badge/written%20in-Rust-c5341c">
  <img alt="Status: in development" src="https://img.shields.io/badge/status-in%20development-8d6105">
</p>

---

bay is a small open-source command for teams that run many coding agents against one Git repository.
An agent opens a change, edits, and asks to land. bay builds the exact commit it will land, checks that
commit, and lands it on trunk. An executor finishes the landing, so the agent can leave. Your repository
stays an ordinary Git repository, on the host you already have.

```sh
bay start CARD-123 --paths src/api   # a view of only the files this change needs
# … the agent edits …
bay land                             # check that exact result; returns at once
bay status                           # what every change is doing, what passed, and why it waits
bay watch                            # what changed that affects my work
bay undo c7k2m4                      # a new change that undoes one landing; history stays
```

Every command prints one JSON result on stdout. A refusal names its reason, what you can still do, and
when to retry. There is no prompt to answer.

> **Status: in development.** This README describes bay as it will ship. The benchmark figures below
> are targets, not results. Follow progress at [getbay.io](https://getbay.io).

## Why bay

Agents now write much of the code. They still land it through steps built for a person who reviews and
then clicks merge: branches, pull requests, merge queues, and scripts. The merge itself takes seconds.
The wait around it does not.

One team of about twenty agents on one repository measured a single day:

- 95 minutes, and nothing landed
- 40 minutes spent booking a test window, and 18 minutes waiting in a queue
- 12 minutes of green tests thrown away by a timeout
- a broken trunk blamed on the wrong change, because the commit that broke it named no change
- 89 leftover branches, and 73 GB of notes and data in per-agent folders¹

Each agent view also cost 27 seconds and 3.4–4.0 GB of disk to create, even for a task that read a few
files.

<sub>¹ 64 GB of it was captures and corpora in five folders. bay leaves large data outside Git and does
not store it.</sub>

## Install

macOS and Linux, on ARM and Intel:

```sh
curl -fsSL https://getbay.io/install.sh | sh
```

The script downloads one binary from this repository's GitHub Releases, checks the signed `SHA256SUMS`
and the archive's sha256, installs it to `~/.local/bin/bay`, then runs `bay install` and `bay doctor`.
Windows gets its own installer at launch. To build from source:

```sh
cargo build --release
./target/release/bay install
```

bay needs Git 2.40 or newer in an agent view (2.50 or newer is recommended), and 2.54 or newer on a
machine that hosts a trunk with `bay host init`. `bay doctor` names any floor a machine is below. See [Install](https://getbay.io/docs/install) for every option.

## What bay promises

| Promise | In plain words |
|---|---|
| **What lands is what was checked** | bay builds the merged result first, checks that result, and lands it only if trunk has not moved. If trunk moved, bay builds and checks again. |
| **Unknown is never a pass** | A check that crashed, timed out, or collected nothing blocks the landing. Checks that finished keep their results. |
| **Agents can leave** | `bay land` returns at once. While an executor runs (`bay executor run`, or your CI through the same contract), it finishes the landing, recovers it, or asks for a precise repair. |
| **A retry never lands twice** | Every operation has an id. A lost reply is reconciled, never sent again as a second landing. |
| **Stale work is stopped, and repair stays open** | When something your work depends on lands, bay tells you what changed. This covers the declared and observed inputs, and bay states its coverage. You can still edit; you cannot land until you check again. |
| **Every refusal says what to do next** | A stable reason, what you can still do, and when to retry. |
| **A view costs what the task reads** | Only the files you named, over one shared cache per machine. This holds for a warm cache; build outputs, fixtures and broad searches can still cost more. |
| **Git stays the only store** | Changes, evidence and events are Git commits, notes and refs. You can read and recover them with ordinary Git. |

Each promise has a failure test that must pass before bay calls it done.

## How it works

1. **`bay start`** opens a change: a sparse view of the declared paths over one shared object cache.
2. **`bay land`** signs a request to land and returns. The agent may leave.
3. An **executor** merges the change onto trunk without touching any working tree, runs the checks your
   `.bay/policy.toml` names on that exact candidate, and records each result as signed evidence in Git.
4. If trunk has not moved, the executor lands the candidate. If it moved, the executor builds a new
   candidate and checks again. After two such moves, the change joins a queue, and bay checks only the head.
5. **`bay status`** and **`bay watch`** show every change, its evidence, and why it waits.

An agent signs a request to land. Only an executor moves trunk. A checker signs the evidence. A registry
key, held by the people who run the repository, adds and revokes keys. No one holds another actor's key,
so an agent can leave.

## Compared with a merge queue

Most teams compare bay with what they run now: agents open pull requests, and a merge queue lands them.
That can merge, and a good merge queue keeps going after the author leaves. What it does not give is a
signed record that the landed commit is the exact one that passed, partial results kept when a worker
dies, recovery when the executor dies, or a stop on work whose inputs moved.

You keep the Git host you already have: GitHub, GitLab, a bare repository over SSH, or `bay host init`
on any machine you can reach.

## Works with

- **Claude Code first.** `bay hook` refuses a write it already knows is stale, and tells the agent in one
  line when something it depends on changes. `bay install` adds the bay skill, so the agent knows the
  commands.
- **Other coding agents** through the same JSON interface. Each agent product gets one adapter behind
  `bay hook`; see [Agents and hooks](https://getbay.io/docs/agents).
- **Any Git host.** GitHub, GitLab, a bare repository, or a bundle on a USB stick.
- **Your checks.** Any command that returns a `bay.check.result/1` JSON document.

## What we will measure

Against an agent session that starts from a full clone, and against a tuned setup with a good merge queue,
on real repositories, with the scripts and raw results published so anyone can rerun them:

- more than 1,000× less network on a warm cache
- more than 50× faster to the first check that can run
- at least 2× fewer model tokens per accepted change

These are targets until the published results say otherwise.

## Documentation

- [Overview](https://getbay.io/docs) and [Quickstart](https://getbay.io/docs/quickstart)
- [Concepts](https://getbay.io/docs/concepts): one meaning for each name
- [Configuration](https://getbay.io/docs/configuration): `.bay/config.toml` and `.bay/policy.toml`
- [Executors](https://getbay.io/docs/executors) and [Agents and hooks](https://getbay.io/docs/agents)
- [CLI reference](https://getbay.io/docs/cli) and [Results and refusals](https://getbay.io/docs/results)
- [Update](https://getbay.io/docs/update): `bay update`, channels and pins

## Contributing

bay is built in the open, and contributions are welcome once the repository opens. Before you send a pull
request, run `cargo fmt --check`, `cargo clippy --all-targets` and `cargo test`. A new promise comes with
a failure test that fails without the change. bay stays organisation-neutral: no one company's names,
hosts, paths or defaults go into this repository.

Questions and ideas: [hello@getbay.io](mailto:hello@getbay.io).

## Licence

[MIT](LICENSE).
