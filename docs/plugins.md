# Native Plugins (RiverDeck)

RiverDeck is pivoting to a **native, Linux-first plugin ecosystem**.

## Key rules

- **Plugins are folders** ending in `.sdPlugin` (for now), e.g. `io.github.sulrwin.riverdeck.starterpack.sdPlugin`.
- **Manifests must be plain JSON** (no Elgato-protected binary `manifest.json` containers).
- **Plugins must opt-in to RiverDeck-native support** by including:

```json
{
  "RiverDeck": { "ManifestVersion": 1 }
}
```

- **Linux-native executable required**: on Linux, `CodePathLin` must point to a native executable in the plugin folder.
- **No Node/Wine/HTML runners** for RiverDeck-native plugins.

## Folder layout

Typical layout (mirrors the Starter Pack):

```
plugins/
  my.plugin.id.sdPlugin/
    assets/
      manifest.json
      icons/
      propertyInspector/
    linux/bin/<your-plugin-binary>
    src/...
    Cargo.toml
    build.ts
```

## Marketplace (local-only, for now)

RiverDeck includes a local Marketplace UI under **Manage Plugins**.

It discovers plugins from:

- **Workspace**: `./plugins/` (when running RiverDeck from a repo checkout)
- **Installed**: `~/.config/io.github.sulrwin.riverdeck/plugins/`

Actions:

- **Install**: copies a workspace plugin folder into the config plugin directory
- **Enable/Disable**: toggles whether the plugin is launched at startup
- **Reload**: re-scan + (re)launch enabled native plugins

## Template

See the template folder:

- `plugins/io.github.sulrwin.riverdeck.template.sdPlugin/`

It’s meant as a starting point for new native plugins.


