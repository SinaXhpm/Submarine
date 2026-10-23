# ironrdp-rdpdr-native, vendored

A copy of [IronRDP](https://github.com/Devolutions/IronRDP)'s `crates/ironrdp-rdpdr-native` at commit
`62246b364a0282bab85087f658a81fe4207b6b31`, the one Submarine pins for its built-in RDP client. MIT OR
Apache-2.0, like the rest of IronRDP (`LICENSE-MIT`, `LICENSE-APACHE`).

## The changes

Both are for one thing: a folder as the root.

1. `src/windows/handles.rs`, `volume_root_nt_path`: the Windows drive backend shares a whole logical volume
   only (`X:\`). It now also takes a folder on one (`X:\a\b`), named by plain names (no empty name, `.`,
   `..`, `/` or NUL), so that Submarine can share one folder with an RDP server. Below the root nothing
   changes: every path the server sends is checked and opened relative to the directory before it, without
   following a reparse point.
2. `src/windows/file.rs`, `set_information_inner`: the root itself is neither deleted nor renamed
   (`ACCESS_DENIED`). A volume root can't be anyway; a folder root could, once the server had emptied it.

A folder can't be given a drive letter of its own instead (`subst`): Windows refuses to open such a letter
with `OBJ_DONT_REPARSE` (`STATUS_REPARSE_POINT_ENCOUNTERED`), which this backend uses for every open.

Not copied: the crate's tests and changelog. The workspace values its `Cargo.toml` inherited are written
out, and its IronRDP dependencies come from the same commit.

## Leaving it

Once an IronRDP release (or the commit Submarine moves to) can share a folder on Windows, delete this
folder and depend on `ironrdp-rdpdr-native` from IronRDP again.
