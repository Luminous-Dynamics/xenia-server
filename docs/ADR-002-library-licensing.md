# ADR-002: Library-vs-binary licensing split

**Status**: accepted
**Date**: 2026-04-18
**Deciders**: tstoltz
**Supersedes**: the licensing rules in ADR-001 §Decision 3 for library crates (binaries unchanged).

## Context

`ADR-001` established a split licensing model: application binaries
(`xenia-peer`, `xenia-viewer`) under AGPL-3.0-or-later, the shared
library `xenia-peer-core` under Apache-2.0 OR MIT, and the wire
protocol (`xenia-wire`, separate repo) under Apache-2.0 OR MIT.

When subsequent library crates landed — `xenia-capture`,
`xenia-video`, `xenia-transport-ws` — they inherited AGPL by
default because they contained code ported from Symthaea (which is
AGPL). That call was defensible at the time but produced an
inconsistency: the ADR-001 intent was "permissive libraries,
copyleft flagship apps," and those three crates quietly drifted
into AGPL even though nothing about their role in the stack had
changed.

The drift surfaced when we started planning a **unified
architecture** where Symthaea consumes `xenia-peer-core` and
sibling library crates as dependencies (rather than maintaining
its own `src/swarm/rdp_*.rs` tree). Symthaea is AGPL; an AGPL
library consumed by an AGPL application is fine, but an
Apache/MIT library consumed by an AGPL application is ALSO fine —
and more importantly, a third-party tool (browser viewer, VS Code
extension, other-distro daemon) cannot consume our AGPL libraries
without inheriting the obligation. That's the opposite of what the
"libraries permissive" principle is supposed to achieve.

## Decision

All **library** crates in this repository are
**Apache-2.0 OR MIT**. All **binary** crates remain
**AGPL-3.0-or-later**.

Concrete assignment:

| Crate | Kind | License |
|---|---|---|
| `xenia-peer-core` | library | Apache-2.0 OR MIT (was already) |
| `xenia-capture` | library | Apache-2.0 OR MIT (was AGPL; flipped) |
| `xenia-video` | library | Apache-2.0 OR MIT (was AGPL; flipped) |
| `xenia-transport-ws` | library | Apache-2.0 OR MIT (was AGPL; flipped) |
| `xenia-handshake` (future) | library | Apache-2.0 OR MIT |
| `xenia-inject` (future) | library | Apache-2.0 OR MIT |
| `xenia-transport-quic` (future) | library | Apache-2.0 OR MIT |
| **`xenia-peer`** | binary | **AGPL-3.0-or-later** |
| **`xenia-viewer`** | binary | **AGPL-3.0-or-later** |
| `xenia-wire` (sibling repo) | library | Apache-2.0 OR MIT |

## Rationale

### Why libraries should be permissive

1. **Ecosystem reach.** The protocol is the commons; the
   libraries that implement the protocol are the *natural* place
   for ecosystem effort. AGPL-licensed libraries discourage any
   third-party from linking — a VS Code extension, a browser
   viewer built by someone outside this project, an embedded
   client for a different hardware platform. A permissive library
   invites that work; AGPL repels it.
2. **Downstream compatibility.** AGPL flows upward through
   linking: consuming one AGPL library makes the consumer also
   AGPL. Apache/MIT is universally consumable — permissive,
   AGPL, proprietary alike. This is the exact reason Matrix's
   SDKs are Apache, not AGPL: the SDK is consumed by both Element
   (AGPL) and third-party clients, and only the SDK can sit at
   that junction without forcing a license on either.
3. **Consistency with `xenia-wire`.** The protocol crate is
   already Apache/MIT for this reason; it would be incoherent to
   flip subsequent transport or codec crates to AGPL just because
   they happened to be ported from a different Symthaea file.
4. **Symthaea consumption pathway.** The unified architecture
   (this ADR's trigger) requires Symthaea to depend on xenia
   libraries. Symthaea is AGPL. AGPL-on-AGPL works; the
   cleaner statement is Apache/MIT libraries consumed by AGPL
   applications — which is what ADR-001 intended.

### Why binaries should stay copyleft

1. **Application-layer moat.** The daemon + viewer pair is the
   value-added product. Apache/MIT here would let an MSP vendor
   take the binaries, rebrand, and ship a proprietary remote-
   access product. AGPL forces any redistributor or
   modify-and-use-for-network-service case to either
   open-source their stack or negotiate a commercial license.
2. **Precedent.** Matrix/Element, GitLab, MongoDB (pre-SSPL).
   The pattern is well-understood; users know what AGPL on a
   flagship client means and choose accordingly.
3. **No ecosystem cost.** Third parties who want to build a
   daemon or viewer can link the libraries (now Apache/MIT) and
   ship under any license they choose; they don't have to fork
   or reimplement anything. They only hit AGPL if they
   incorporate *our* daemon/viewer binary itself, which is
   exactly the case AGPL is supposed to catch.

### Why this wasn't ADR-001's decision

It should have been. ADR-001 §Decision 3 described the principle
correctly ("libraries permissive, binaries copyleft") but only
explicitly assigned licenses to the three crates that existed at
the time (`xenia-peer-core` library, `xenia-peer` and
`xenia-viewer` binaries). Subsequent library crates inherited
AGPL from the Symthaea source they were ported from without a
deliberate decision. This ADR corrects the drift and makes the
principle explicit for all future library crates.

## Ported-code authorship note

Several library files carry code ported from Symthaea:

- `xenia-capture/src/lib.rs` — from `symthaea/src/swarm/rdp_capture.rs`
- `xenia-video/src/h264.rs` — patterns from `symthaea/crates/symthaea-phone-embodiment/src/scrcpy/decoder.rs`
- `xenia-video/src/hdc.rs` — from `symthaea/src/swarm/rdp_codec.rs`

All ported files carry the same copyright holder as their
Symthaea origins (the repository author). Relicensing to
Apache/MIT for this crate is the copyright holder's prerogative.
The file-level comments have been updated to note the relicense
explicitly. The Symthaea upstream remains AGPL; forks that carry
these files back INTO Symthaea would follow Symthaea's AGPL.

## Consequences

**Accepted:**

- Symthaea can depend on `xenia-peer-core` (and siblings) via
  `cargo add` (once published) or `git = "…"` (today) without
  license-compatibility concerns.
- Third-party tools — browser viewers, IDE extensions, embedded
  clients — can link the xenia libraries under their own
  (Apache/MIT/BSD/proprietary/AGPL) licenses.
- Commercial MSP vendors cannot redistribute `xenia-peer` or
  `xenia-viewer` as closed-source without a commercial license.
  That protection is intact.

**Deliberate non-consequence:**

- The wire format itself (SPEC in `xenia-wire`) is already
  permissive. This ADR does not change anything at the protocol
  layer.

## References

- ADR-001 (library-vs-binary intent; this ADR extends it
  consistently across all subsequent library crates).
- `ROADMAP.md` §"Open questions for humans" #3 (the prompt that
  forced this decision).
- Matrix / Element split: <https://matrix.org/blog/2021/12/22/licensing-update/>
- GNU AGPL-3.0 FAQ: <https://www.gnu.org/licenses/agpl-3.0.html>
