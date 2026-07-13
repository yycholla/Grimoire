# Four-peer Linux acceptance

Use this checklist for a release candidate. It complements the automated suite; one computer cannot prove NAT traversal across two real internet connections.

## Prepare

- Use four Linux computers, named A through D.
- Put A and B on one internet connection and C and D on another. Do not bridge the networks with a VPN.
- Install the exact same build on all four computers with `nix profile install path:.` or copy the same `nix build path:.` result.
- Use fresh data directories and headphones. Record the build path, date, network locations, and observed results.

## Direct-path community run

1. On A, enter display name `A`, create a community, and copy its invite.
2. On B, C, and D, enter display names `B`, `C`, and `D` respectively and join with that invite. Approve all three join requests on A.
3. Wait up to 30 seconds for admitted members to build the full mesh automatically. Select members in the roster and verify every peer appears online; use **≡ → connect** only to diagnose a failed automatic connection.
4. Verify every peer has a selected `Direct` path to at least one peer on the other internet connection. Record the displayed RTTs. If no cross-network direct path appears, this network pair does not satisfy direct-path acceptance.
5. From every peer, send text and one attachment using **⊕** or drag-and-drop. All four timelines must converge without duplicates; save each received attachment and verify its content.
6. Close D. Send text and an attachment while D is offline. Reopen D without pasting an address and verify it reconnects automatically and both items arrive once.
7. Join the same voice channel on all four peers for two minutes. Each person must speak an identifying sentence that all three others can understand. No peer may crash, and no continuous audio loss may exceed two seconds. Muting must stop transmission; leaving and rejoining must restore audio.

## Relay-only run

1. On each client, open **≡**, select **relay-only transport on next open**, then close and reopen the same data directory.
2. Wait for the full mesh to reconnect automatically; use manual address exchange only to diagnose a failure.
3. Select each member in the roster. Every selected path must display `RELAY`; record the RTTs.
4. Repeat text, attachment, two-minute four-peer voice, mute, leave/rejoin, and D offline-catch-up checks from the direct run.
5. Optionally confirm the public relay independently:

   ```console
   cargo test -p peer-core --test wan_acceptance -- --ignored --nocapture
   ```

## Pass record

The candidate passes only when both runs succeed on the same build. Keep this compact record with the release notes:

```text
Build:
Date:
Network 1 / peers:
Network 2 / peers:
Cross-network direct path + RTT:
Relay-only path + RTT:
Text and attachment convergence:
Offline catch-up:
Four-peer voice (2 min):
Mute and leave/rejoin:
Notes:
```
