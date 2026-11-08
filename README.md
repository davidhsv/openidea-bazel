# Bazel for OpenIDEA

An [OpenIDEA](https://github.com/l0ng-ai/openidea) plugin that opens Java Bazel workspaces from their BUILD files instead of Maven. It is a port of the Java *aspect sync* of the official Bazel plugin for IntelliJ ([bazelbuild/intellij](https://github.com/bazelbuild/intellij), branch `ijwb`) to OpenIDEA's WebAssembly plugin API (1.3, OpenIDEA 0.1.39+).

## How it works

**Bazel: Sync Project with BUILD Files** does what IntelliJ's sync does, step for step:

1. Reads the project view, `.bazelproject` (`directories`, `targets`, `derive_targets_from_directories`, `test_sources`, `java_language_level`, `build_flags`, `sync_flags`, `exclude_target`, `import_target_output`, `excluded_libraries`, `import`/`try_import`).
2. Runs `bazel info`, writes the IntelliJ aspect into `.openidea/bazel/aspects`, derives the targets with the same `bazel query`, and builds them with the aspect (`intellij-info-*` and `intellij-resolve-java` output groups).
3. Reads the aspect's `.intellij-info.txt` files, then computes the Java project as IntelliJ's `JavaSourceFilter`, `SourceDirectoryCalculator` and `BlazeJavaWorkspaceImporter` do: source folders with test flags, and libraries from jdeps, direct dependencies and generated jars.
4. Writes that project where OpenIDEA's Java language server (Eclipse jdtls) reads it, mirroring IntelliJ's single workspace module:
   - one root Eclipse project: `.project`, `.classpath`, `.settings/org.eclipse.jdt.core.prefs`;
   - `.openidea/settings.toml`, which turns jdtls' Maven and Gradle importers off and gives the workspace its own jdtls data directory.

Libraries are the jars Bazel built (class jars, so Run and Debug work), each with its source jar attached. Lombok is handled by OpenIDEA's own Lombok agent.

Like IntelliJ's startup sync, a Bazel workspace that was never synced syncs by itself when it opens (setting **Sync a Bazel workspace that was never synced when it opens**, on by default); one that was synced just loads its project.

After the **first** sync, restart OpenIDEA (Find Action › *Restart IDE*), since the IDE reads `.openidea/settings.toml` when a project opens. After later syncs, restart the Java language server from the language servers popup in the status bar. jdtls reads `.classpath` when it starts.

Other features:
- **Starlark** (`BUILD`, `BUILD.bazel`, `*.bzl`, `WORKSPACE`, `MODULE.bazel`), **project view** and **bazelrc** languages.
- **Run configurations** `bazel build //...` and `bazel test //...`.
- A status bar item that says when BUILD files changed since the last sync.

## Install

```sh
rustup target add wasm32-wasip2
cargo build --release --target wasm32-wasip2 --lib
mkdir -p dist && cp target/wasm32-wasip2/release/openidea_bazel.wasm dist/plugin.wasm
```

Then *Settings › Plugins › Install Plugin from Disk…* (this folder), or link it for development with `openidea-plugin link .`. Approve the capabilities the plugin asks for:

| Capability | Why |
| --- | --- |
| `project.read`, `project.write` | Read the project view and the Java sources; write the aspect, the Eclipse project and `.openidea/settings.toml`. |
| `process:bazelisk` or `process:bazel` | Run Bazel. |
| `fs.read:/private/var/tmp` (macOS) / `fs.read:~/.cache/bazel` (Linux) | Read the aspect's output in Bazel's output base. With `--output_user_root` elsewhere, the sync reports which path it was refused. |
| `events` | Notice edits to BUILD files. |

The files the plugin writes are kept out of Git: `.openidea/bazel/.gitignore` ignores the plugin's own directory, and `/.openidea/settings.toml`, `/.project`, `/.classpath` and `/.settings/` are added to `.git/info/exclude`.

## Development

```sh
cargo test                                   # unit tests, against real aspect output in tests/data
cargo run --release --example sync_cli -- /path/to/workspace   # the whole sync natively, outside the IDE
```

`examples/fuel_sync.rs` measures a sync's CPU cost under wasmtime against the per-call budget OpenIDEA gives a plugin (5·10⁹ fuel). On Allocadia, a sync of 2,595 targets and 3,068 info files costs about 3.2·10⁹ and fits in 128 MiB of memory.

## Limitations

- Source folders with a package prefix (a folder whose files' package doesn't follow its path) can't be expressed in `.classpath` and are left out with a warning.
- The whole sync runs in one plugin call, so a much larger workspace than the one above can exceed the CPU budget. Sharding, as IntelliJ's `shard_sync` does, would be the next step.
- The aspect ships without IntelliJ's tool jars (PackageParser, JarFilter, CreateAar). Packages are read from the sources instead, and a target that mixes hand-written and generated sources contributes its whole jar.
- Only Java is synced (no Kotlin, Scala, Python, C++, Go or Android).
- The plugin runs as one instance in OpenIDEA, so syncs of several windows (worktrees) run one after another.

## License

Apache 2.0, like the code it ports; see `NOTICE`.
