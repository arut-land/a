# The machine key

A home's machine identity (`protocol::MachineId`) is the first 128 bits of
the SHA-256 of an X25519 public key. `sys::machine_id` makes the secret the
first time a home needs it and reads it every time after; deriving the
public key is the only work that is not I/O.

## Which X25519

Both candidates were linked into a probe that derives the public key from a
random secret and hashes it with `sha2` 0.11 (already in the binary for
blobs), against a probe that only hashes. Release profile as shipped
(`opt-level = "z"`, fat LTO, one codegen unit), Apple M-series, macOS 27,
quiet machine; per derivation, the mean of 200 in a loop, three runs:

| | `__text` | `__const` | derive + hash | crates added |
| --- | --- | --- | --- | --- |
| hash only | 195,916 B | 19,728 B | 0.2-0.3 µs | |
| `x25519-dalek` 2.0.1 (`static_secrets`) | +3,840 B | +288 B | 57-86 µs | 4: `curve25519-dalek`, `x25519-dalek`, `subtle`, `rand_core` 0.6 |
| `ed25519-compact` 2.6.0 (`x25519`, `disable-signatures`) | +3,840 B | +328 B | 44-78 µs | 1, with no dependencies |

Size and time are the same. `x25519-dalek` brings `rand_core` 0.6, a second
version beside the 0.9 the fuzz tests resolve through `bolero`, which
`deny.toml`'s `multiple-versions = "deny"` refuses; so `ed25519-compact`,
taking its randomness from the `getrandom` 0.4 that `uuid` already links.

## Off the cold path

A client of this machine's daemon names no machine in its hello, so it
never reads the key: `a`'s cold start and attach are unchanged. The daemon
reads (or, once per home, makes) it on the blocking pool while it loads its
catalog, and only a welcome waits for it. `SshLink` reads it before it runs
`ssh`, where tens of microseconds disappear in the connection.

## Where the secret lives

A file in the home's state directory, `machine.key`, created mode 0600 (on
Windows under the profile's access list, and sealed to the user's logon
credentials with DPAPI, `CryptProtectData`, which needs no crate beyond
`windows-sys`). It is staged under a private name and published with
`sys::fs::publish_ordered`, which never replaces a name, so two processes
making a home's first key agree on one. A file that is not a key is
reported, never replaced: a new key would be a new machine.

`publish_ordered` only orders the name behind the write; on Apple the
drive cache still holds both until a full flush. A client may pin the id
in `machines.toml` as soon as a welcome names it, and a key a power cut
took back would make the home another machine, so a new key is flushed
(`F_FULLFSYNC`, once per home, before the id is known) where the platform
keeps a separate cache. On macOS the staged file also carries
`com.apple.metadata:com_apple_backup_excludeItem` (the sticky exclusion
`tmutil addexclusion` writes, a 61-byte property list, set with one
`fsetxattr`): a Time Machine restore onto another Mac, or beside this
home, would otherwise be a second machine with this one's id. Windows
needs neither: its publish is write-through, and a DPAPI-sealed key
restored under another logon does not open there.

The macOS keychain was considered and left out. A legacy (file-based)
keychain item trusts the code signature of the program that made it, and
`a` is ad-hoc signed, so every new build would raise an access prompt
(a daemon has no one to answer it, and staged upgrades are new builds). The
data-protection keychain needs a keychain-access-group entitlement, which
an unsigned command-line program cannot have. Either keychain is also one
per user, not per home, so tests with an isolated `A_HOME` would share it.
Signed app builds (`apps/apple`) can take the key into the keychain later,
behind the same function.

## Measured on the hosted runners

`perf` on this change (run 40158080189, Windows x64 again in 40159891959),
against the latest main lane. The paired harness cannot pair across it:
it builds the reference with this tree's `perf-attach`, whose hello has a
field the reference lacks. Shipped size grows 15-25 KiB (+0.4-0.5 %) on
every target, the X25519 code and the peer and host types with it. Cold
reply, attach and idle RSS are within each lane's noise: macOS arm64
19.8 ms cold reply, Linux x64 5.6 ms, Linux arm64 7.0 ms, Windows x64
22.1 ms (+0.9 ms, within noise; the first run's +9.5 ms did not repeat),
Windows arm64 54.1 ms; idle RSS moved by at most +240 KiB (Windows arm64)
and +24-80 KiB elsewhere.
