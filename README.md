# Grimoire

[![CI](https://github.com/yycholla/Grimoire/actions/workflows/ci.yml/badge.svg)](https://github.com/yycholla/Grimoire/actions/workflows/ci.yml)

Grimoire is a Rust-native peer-to-peer community prototype with end-to-end encrypted text and small-group voice, owner-controlled membership, embedded Turso storage, and Iroh connectivity.

## Enter the development environment

With direnv and Lorri:

```console
direnv allow
```

Lorri loads `shell.nix`, which bridges to the flake through an explicit `path:` reference so JJ-managed files are visible to Nix. After changing or updating `flake.nix`, refresh the environment before running Cargo:

```console
direnv reload
command -v cmake pkg-config protoc
pkg-config --modversion xcb
```

If direnv reports that the Lorri daemon is stopped and loads a cached environment,
restart `lorri.service` and reload direnv. If a command is still missing, use
`nix develop path:.` directly rather than running Cargo from the host shell.

Or directly with Nix:

```console
nix develop path:.
```

The environment provides Rust 1.97, CMake, Protobuf, pkg-config, ALSA, Opus, and rust-analyzer.

## Continuous integration

GitHub Actions verifies Nix builds on Linux x86_64, Linux ARM64, and Apple Silicon
macOS. Windows x86_64 uses the same pinned Rust 1.97.0 toolchain and builds the
native binaries directly. These are verification builds; portable, signed
release bundles are a separate packaging milestone.

## Install on Linux

Run the packaged desktop application directly:

```console
nix run path:.
```

Or install it into your Nix profile:

```console
nix profile install path:.
grimoire
```

The package installs `grimoire`, the `grimoire-cli` diagnostics harness, and a **Grimoire** desktop-menu entry. Updates remain controlled by Nix; the application does not self-update.

## Run the checks

Run these inside the development environment:

```console
cargo fmt --all -- --check
cargo nextest run --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The exact-tree release gate runs the same build, formatting, test, and Clippy checks through Crane:

```console
nix flake check path:.
```

Useful focused suites:

```console
cargo nextest run -p grimoire-core --test replication
cargo nextest run -p grimoire-audio --test codec
cargo nextest run -p grimoire-audio --test playout
```

Build and open the Rust API documentation locally:

```console
cargo doc --workspace --no-deps --open
```

## Run the desktop client

```console
cargo run -p grimoire
cargo run -p grimoire -- /path/to/community-a /path/to/community-b
cargo run -p grimoire -- --preview
```

With no path, the client reopens the last Community or shows native Create,
Join, and Recover forms. `--preview` opens the representative 3a design state.
Positional paths open existing Communities and make them available in the left
sidebar. The active Voice Channel remains connected while browsing channels or
Communities; use the persistent voice dock to mute, deafen, or leave.

The current GPUI runtime does not expose screen-reader semantics for custom
controls. Text fields support platform clipboard shortcuts, selection, word
navigation, and undo/redo, and slash commands cover the in-Community actions;
onboarding and navigation controls still require a pointer. This is a known
runtime limitation, not an accessibility guarantee.

Create an owner Community:

```console
cargo run -p grimoire -- --create /path/to/owner-community
```

To exercise two peers locally:

1. Start the owner, choose **create a Community**, enter its data directory and display name, then select **copy invite** in the header.
2. Start the joining client, choose **join with an invite**, and enter a different data directory, display name, and the copied invite.
3. Select **admit** beside the yellow request in the owner's member roster. The joining client remains on its Waiting screen until admission, then admitted members reconnect to one another automatically. Display names are encrypted Community data and appear afterward.
4. Send text in **# general**. Share a file with the **⊕** composer button, drag it onto the window, or use `/attach /path/to/file`; the other peer can save or forget its local copy from the attachment chip. Owners create channels with the **+** beside the text or voice section.
5. Select the same voice channel in both clients to join immediately. Use the voice dock in the sidebar to mute, deafen, leave, or monitor the connection while browsing elsewhere.

Select the **≡** self control for display-name editing, local-address copy,
manual peer connection diagnostics, voice input/output selection, encrypted identity backup,
transport mode, and Community switching. Voice device changes apply on the next
join; each selector cycles through detected devices and the system default. Select a member for their identity fingerprint, direct/relay paths,
selected path and RTT, or owner removal controls. `/help` lists equivalent compact
commands for keyboard-driven testing. The bottom status bar summarizes live
connectivity.

Display names are encrypted with community content. After a new admission, an offline member may appear by a short identity fingerprint until that member reconnects and republishes their name under the new membership key. Earlier community keys are not shared with newly admitted members.

To check offline catch-up, close the second client, send one or more messages from the owner, then reopen it. Admitted members reconnect automatically, exchange encrypted message inventories, and transfer only missing text; the timeline should converge in the same order without duplicate messages.

## Back up and recover an identity

Open **≡**, enter a backup path and matching passphrase of at least 12 characters,
then select **create identity backup**. The encrypted backup contains both
long-term identity keys, the Community identity, and the latest locally available
content key. Keep the backup and passphrase separate. `/backup` remains available
for keyboard-driven testing.

To recover, stop the old installation, start `grimoire` without arguments, and choose **recover an identity**. Recovery passphrases stay inside the masked native field instead of shell history or process arguments. After opening, use **≡ → connect** with a current Community peer so membership, key envelopes, channels, and history can catch up. Running the same recovered identity on two devices at once is unsupported.

Data directories created before content encryption keep their old plaintext messages as local-only history. New messages use encrypted storage and wire operations. For full encryption guarantees in manual acceptance testing, create fresh data directories.

Attachments up to 8 MiB, including their filenames, are encrypted with the active community content key and replicated eagerly to admitted peers. Availability is best-effort: a file remains obtainable only while at least one reachable peer retains a copy. **forget local** hides an attachment in the app and prevents later re-downloads on this device; it does not delete another member's copy or guarantee forensic erasure from database free pages, journals, or backups.

Use headphones or mute one client when testing both voice peers on the same machine to avoid feedback.

## Run an Availability Peer

An Availability Peer is a headless Community member that stays online to retain
and forward encrypted data while participant devices are offline. Provision one
manually:

1. In the owner client, select **copy invite**.
2. Start the headless peer with a new persistent data directory:

   ```console
   cargo run -p grimoire-cli -- availability --data-dir "$HOME/.local/share/grimoire/availability" --invite '<community-invite>'
   ```

   To prohibit direct paths, put the global flag before the subcommand:

   ```console
   cargo run -p grimoire-cli -- --relay-only availability --data-dir "$HOME/.local/share/grimoire/availability" --invite '<community-invite>'
   ```

3. The command prints its stable identity and current Iroh address, connects to
   the invite owner, and waits. In the owner's member roster, select **admit
   availability peer** for that identity, not **admit member**.
4. Leave the command running. It retains encrypted Community ciphertext until
   Ctrl-C; restart it with the same data directory to keep the same identity.

Availability admission deliberately does not issue Community content keys. The
peer cannot decrypt text bodies, attachment bytes or filenames, or member
profiles, and availability mode never prints that content. It still observes
the routing identifiers, timing, sizes, and peer addresses required to store
and transport ciphertext. The machine running it is therefore trusted for
availability and metadata exposure, not for Community content.

## Run the temporary Grimoire harness

Inspect the available commands:

```console
cargo run -p grimoire-cli -- --help
```

Start a peer and print its Iroh address:

```console
cargo run -p grimoire-cli -- serve --data-dir /tmp/grimoire-a
```

Inspect the selected direct or relay path and its RTT after connecting an existing community data directory:

```console
cargo run -p grimoire-cli -- diagnose --data-dir /tmp/grimoire-a --address '<peer-address>'
cargo run -p grimoire-cli -- --relay-only diagnose --data-dir /tmp/grimoire-a --address '<peer-address>'
```

The selected line reports `kind=Direct` or `kind=Relay`, `selected=true`, and `rtt_ms`. The forced public-relay smoke test is intentionally ignored by the offline test suite and can be run explicitly:

```console
cargo test -p grimoire-core --test wan_acceptance -- --ignored --nocapture
```

The current CLI is a transport, audio, and headless availability harness, not
the eventual application UI. Normal participant invite admission remains in the
desktop client.

Before calling a build accepted, run the [four-peer Linux acceptance checklist](docs/acceptance.md) across two real internet connections.
