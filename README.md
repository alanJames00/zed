> [!IMPORTANT]
> Remove this line to confirm you've reviewed this PR before submitting.
# Zed (custom fork)

> Zed is a great editor. Some of it I love, other parts I hate. So this is a custom fork of how I like my Zed.

This is a personal fork of [Zed](https://github.com/zed-industries/zed), the high-performance code editor from the creators of [Atom](https://github.com/atom/atom) and [Tree-sitter](https://github.com/tree-sitter/tree-sitter). It tracks upstream, with changes made to suit my own workflow. It is not an official build, and it is not affiliated with or supported by Zed Industries.

---

## What's different from upstream

### Added or changed

- **VS Code-style Git panel.** The Git panel now defaults to a VS Code-like layout:
  - The commit box sits at the top.
  - Changes are shown in VS Code-style groups with per-row actions.
  - A collapsible history section draws the commit graph with branch lanes.
  - A repositories section lists the repos in the workspace.
  - The panel has a VS Code-style title bar and menus.
  - The status bar shows a branch indicator, and there is a `git::Sync` action.
- **Vertical terminal list.** The terminal panel can show terminals as a vertical list instead of tabs. It supports a scrollbar, reordering, resizing, rename and other actions. Lists restored from a previous session also show correctly. It is controlled by a setting.

### Removed

These features are stripped out to make the editor lighter and quieter:

- **Telemetry, crash reporting and auto-update.** Nothing phones home, no second crash-handler process is spawned, and the editor never replaces itself with an upstream build.
- **Edit predictions and Copilot.**
- **Collaboration.** This covers calls, screen sharing, channels and the collab server.
- **REPL and Jupyter notebook support.**
- **The AI agent and LLM providers.**

[REMOVABLE_FEATURES.md](./REMOVABLE_FEATURES.md) is an audit of what else could be cut, ranked by estimated savings. Several items on it are already done (#1 to #4 and #7).

---

## Building

This fork is built from source. There are no prebuilt binaries or package-manager releases, and the upstream download and installation pages will give you stock Zed instead.

- [Building Zed for macOS](./docs/src/development/macos.md)
- [Building Zed for Linux](./docs/src/development/linux.md)
- [Building Zed for Windows](./docs/src/development/windows.md)

Typical flow on Linux:

```sh
cargo run --release
```

## Staying in sync with upstream

Upstream changes are merged in periodically, so removed features can occasionally reappear in a merge and need to be stripped out again.

## Contributing

This is a personal fork, so it is tuned to one person's preferences. If you want something that works for everyone, contribute to [upstream Zed](https://github.com/zed-industries/zed/blob/main/CONTRIBUTING.md) instead. See [CONTRIBUTING.md](./CONTRIBUTING.md) for the upstream guidelines.

## Licensing

Zed source code is licensed primarily under GPL-3.0-or-later, with Apache-2.0 components where marked. This fork keeps the same licensing.

License information for third party dependencies must be correctly provided for CI to pass.

We use [`cargo-about`](https://github.com/EmbarkStudios/cargo-about) to automatically comply with open source licenses. If CI is failing, check the following:

- Is it showing a `no license specified` error for a crate you've created? If so, add `publish = false` under `[package]` in your crate's Cargo.toml.
- Is the error `failed to satisfy license requirements` for a dependency? If so, first determine what license the project has and whether this system is sufficient to comply with this license's requirements. If you're unsure, ask a lawyer. Once you've verified that this system is acceptable add the license's SPDX identifier to the `accepted` array in `script/licenses/zed-licenses.toml`.
- Is `cargo-about` unable to find the license for a dependency? If so, add a clarification field at the end of `script/licenses/zed-licenses.toml`, as specified in the [cargo-about book](https://embarkstudios.github.io/cargo-about/cli/generate/config.html#crate-configuration).

## Credits

All the hard work belongs to the [Zed Industries](https://zed.dev) team and contributors.
