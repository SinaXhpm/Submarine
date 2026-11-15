# Flatpak

The files here build Submarine as `com.sinaxhpm.Submarine` for Flathub:

- `com.sinaxhpm.Submarine.yml`: the manifest. It builds this checkout.
- `com.sinaxhpm.Submarine.metainfo.xml`: the store listing.
- `com.sinaxhpm.Submarine.desktop`: the launcher entry.

Flathub builds without network access, so every Rust and npm package has to be listed in advance, in two generated files.
They are too big and change too often to keep here: make them in `generated/` before a build (the folder is ignored by git).

## The source lists

With [flatpak-builder-tools](https://github.com/flatpak/flatpak-builder-tools) (needs `python3-aiohttp`, `python3-tomlkit`, `python3-yaml`):

```sh
python3 flatpak-builder-tools/cargo/flatpak-cargo-generator.py src-tauri/Cargo.lock -o packaging/flatpak/generated/cargo-sources.json
cd flatpak-builder-tools/node
python3 -m flatpak_node_generator npm <repo>/package-lock.json -o <repo>/packaging/flatpak/generated/node-sources.json
```

## A test build

```sh
flatpak install flathub org.gnome.Sdk//51 org.gnome.Platform//51 \
  org.freedesktop.Sdk.Extension.rust-stable//26.08 org.freedesktop.Sdk.Extension.node24//26.08
flatpak-builder --force-clean --user --install build-dir packaging/flatpak/com.sinaxhpm.Submarine.yml
flatpak run com.sinaxhpm.Submarine
```

## With each release

1. In the metainfo, add the version and date to `<releases>`, and change the screenshots from `main` to the tag.
2. In the Flathub repository's manifest, set the first source to the tag and its commit:

   ```yaml
   - type: git
     url: https://github.com/SinaXhpm/Submarine.git
     tag: v0.4.0
     commit: <the tag's commit>
   ```

   Keep the `generated/` file names, or put the two lists next to the manifest and change the paths.
3. Regenerate the two source lists from that tag's lockfiles and commit them to the Flathub repository.

## What the sandbox changes

- The app can't start programs installed on the system. A ProxyCommand runs inside the sandbox, with only the tools the runtime has, and RDP servers need the built-in client.
- `--filesystem=home` is for the local side of SFTP transfers and mirrors, and for `~/.ssh`. Folders outside the home folder can't be used as the local side.
- Flathub does the updating, so the app doesn't look for releases on GitHub.
