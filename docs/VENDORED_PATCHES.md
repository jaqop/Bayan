# Local portable-pty changes

The vendored crate stays at upstream 0.9.0. Preserve these changes when
refreshing it; do not overwrite the tree with a clean upstream archive.

- `conpty_resolve.rs`: System32-only kernel32 fallback; no PATH/CWD DLL lookup.
- `conpty_bundle.rs`: Bayan-owned module. Reads the compile-time
  `resources/conpty.json` manifest. Both Microsoft package files require
  their exact architecture-specific SHA-256, a valid Authenticode chain,
  leaf publisher `Microsoft Corporation`, and cached revocation checks for
  the chain excluding its root. No network or permission UI is used at
  runtime. Unknown/stale revocation state rejects the bundle.
- The verifier opens ancestors from root to leaf, checks the opened
  directory handles for reparse points, and retains them without delete
  sharing. It retains both image handles without write/delete sharing.
  This prevents replacement between verification, DLL mapping, and the
  later OpenConsole launch. LoadLibraryExW takes a path, not a file handle;
  the retained handles protect that path throughout the host lifetime.
- `psuedocon.rs`: optional pinned bundle first, with inbox fallback on
  rejection or launch failure. Resize and close use the API that created
  each pseudoconsole. Elevated processes never use the optional bundle.
  The 1.22+ Microsoft rewrite forwards VT by default; historical bit 0x8
  now selects grapheme width and is not used for passthrough.
- The Windows dependency section adds `sha2` and `serde_json`, already
  present in Bayan's locked dependency graph. No crate version was upgraded.

The host-loading approach was reviewed against PR #73 and Microsoft's
Windows Terminal source, then implemented with mandatory pins, strict
publisher/revocation checks, retained handles, and no elevated bundle.
The rest of #73 is not imported or marked merged by this change.

`scripts/install-conpty.ps1` downloads only the pinned Microsoft NuGet
package and verifies both file hashes and Microsoft signatures before
copying the pair. `src/conpty_load.rs` compiles the production verifier
into the Bayan test harness. The explicitly invoked native probes cover
signature/pin acceptance, write locks, and a real console OSC 52 round trip.

- `cmdbuilder.rs`: `env_keys()` exposes raw `OsStr` environment names so
  elevated-spawn sanitation also covers inherited registry values and names
  whose values are not Unicode. The normal command builder remains unchanged.

Elevated Git Bash uses a mandatory protected startup plan independently of
OSC prompt integration. Startup/environment hooks are removed from the final
command builder, including its HKCU environment base. This does not certify
all of PR #71: completion helper/path ownership and WSL-root isolation need
separate validation. Do not treat WSL elevation as a security boundary.
