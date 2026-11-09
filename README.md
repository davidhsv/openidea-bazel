# Bazel for OpenIDEA

An [OpenIDEA](https://github.com/l0ng-ai/openidea) plugin that opens Java Bazel workspaces from their BUILD files instead of Maven. It is a port of the Java *aspect sync* of the official Bazel plugin for IntelliJ ([bazelbuild/intellij](https://github.com/bazelbuild/intellij), branch `ijwb`) to OpenIDEA's WebAssembly plugin API (1.4). It needs OpenIDEA built from [davidhsv/openidea `plugin-test-runners`](https://github.com/davidhsv/openidea/tree/plugin-test-runners) (upstream `main` plus debuggable plugin run configurations and gutter menus) until those reach a release.

## How it works

**Bazel: Sync Project with BUILD Files** does what IntelliJ's sync does, step for step:

1. Reads the project view, `.bazelproject` (`directories`, `targets`, `derive_targets_from_directories`, `test_sources`, `java_language_level`, `build_flags`, `sync_flags`, `exclude_target`, `import_target_output`, `excluded_libraries`, `import`/`try_import`).
2. Runs `bazel info`, writes the IntelliJ aspect into `.openidea/bazel/aspects`, derives the targets with the same `bazel query`, and builds them with the aspect (`intellij-info-*` and `intellij-resolve-java` output groups).
3. Reads the aspect's `.intellij-info.txt` files, then computes the Java project as IntelliJ's `JavaSourceFilter`, `SourceDirectoryCalculator` and `BlazeJavaWorkspaceImporter` do: source folders with test flags, and libraries from jdeps, direct dependencies and generated jars.
4. Writes that project where OpenIDEA's Java language server (Eclipse jdtls) reads it, mirroring IntelliJ's single workspace module:
   - one root Eclipse project: `.project`, `.classpath`, `.settings/org.eclipse.jdt.core.prefs`;
   - `.openidea/settings.toml`, which turns jdtls' Maven and Gradle importers off and gives the workspace its own jdtls data directory.

Libraries are the jars Bazel built (class jars, so Run and Debug work), each with its source jar attached. Lombok is handled by OpenIDEA's own Lombok agent.

Like IntelliJ, the sync reads every source's `package` and gives each directory its sources' most common one. Two things IntelliJ expresses that an Eclipse project cannot are written as linked resources under the virtual folder `.openidea-src`: a source root with a package prefix (Allocadia's `tools/rules`, `com.allocadia.tools`) is linked at the prefix's path, and a source whose `package` differs from its directory's is linked at its package's path. IntelliJ flags such a source as misplaced; here it resolves.

The sync log (**Bazel: Show Last Sync Log**) ends with the time of each step: `timing: bazel info … · query … · build … · info files … · model … · project files …`.

Like IntelliJ's startup sync, a Bazel workspace that was never synced syncs by itself when it opens (setting **Sync a Bazel workspace that was never synced when it opens**, on by default); one that was synced just loads its project.

After the **first** sync, restart OpenIDEA (Find Action › *Restart IDE*), since the IDE reads `.openidea/settings.toml` when a project opens. After later syncs, restart the Java language server from the language servers popup in the status bar. jdtls reads `.classpath` when it starts. jdtls creates linked resources only when it imports a project, so a sync that changes the set of linked sources gives the workspace a new jdtls data directory, and the sync's notification asks for an OpenIDEA restart again.

Other features:
- **Starlark** (`BUILD`, `BUILD.bazel`, `*.bzl`, `WORKSPACE`, `MODULE.bazel`), **project view** and **bazelrc** languages.
- **Run configurations** `bazel build //...` and `bazel test //...`.
- A status bar item that says which workspaces' BUILD files changed since their last sync (an edit, a checkout or a branch switch, also one made while the IDE was closed); clicking it syncs those workspaces, whichever window it is clicked in.
- **Run and Debug in the gutter** of test classes and `@Test` methods, as IntelliJ's Bazel plugin offers them: *Run 'Bazel test IsoInstantTest.parses'* runs `bazel test <target> --test_filter=<class>#<method>$` in the Run tool window; *Debug* adds `--java_debug` and the Java debugger attaches to the test JVM by itself. **Bazel: Test File** and **Bazel: Debug Test File** (editor context menu › Bazel) do the same for the file, or for the method under the caret.
- A sync unlinks the workspace's root `pom.xml` from OpenIDEA's Maven model, as IntelliJ drops Maven in a Bazel project: the editor's test runs and run configurations stop building with Maven.
- **Bazel: Run Target…** and **Bazel: Debug Target…** (Tools menu) `bazel run` a binary of the synced directories in a log tab: type a label or part of one (it starts as the last target run in the workspace). Debug lists `java_binary` targets and passes their launcher `--debug`: the JVM waits on port 5005 for *Attach to Process*. The binaries start with the project's `runEnvironment` setting (`NAME=value` entries), as a run configuration's environment, e.g. in `.openidea/settings.toml`:

  ```toml
  [plugins."davidhsv.bazel"]
  runEnvironment = ["ALLOCADIA_ENV=local", "BACKEND_PORT=8888"]
  ```

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

- The whole sync runs in one plugin call, so a much larger workspace than the one above can exceed the CPU budget: OpenIDEA has no way for a plugin to continue work in a later call, which sharding (IntelliJ's `shard_sync`) needs.
- After the build, a sync reads every info file and source through the IDE, one call each: on Allocadia that takes about 7 s (measured 2026-11-08). IntelliJ's own log of an Allocadia sync (2026-10-13, another commit) shows about 4 s for the same step, which it runs multi-threaded. The plugin API has no batch read and no file times in directory listings, which a faster incremental sync needs.
- No test tree yet: Bazel's JUnit runner prints no per-test lines, so the Run tab shows the output; reading `bazel-testlogs/**/test.xml` through the API 1.4 `test-results` export is the next step.
- jdtls data directories of replaced link sets stay in the plugin's data directory (`…/plugins/.data/davidhsv.bazel/files/jdtls/<hash>`); plugins have no delete. Any of them except the one in `.openidea/settings.toml` can be removed while OpenIDEA is closed.
- The aspect ships without IntelliJ's tool jars (PackageParser, JarFilter, CreateAar). Packages are read from the sources instead, and a target that mixes hand-written and generated sources contributes its whole jar.
- Only Java is synced (no Kotlin, Scala, Python, C++, Go or Android).
- The plugin runs as one instance in OpenIDEA, so syncs of several windows (worktrees) run one after another. Each worktree keeps its own Bazel output base, jdtls data directory and scratch files, so worktrees never share state.
- A window that opens with no editor is synced when its first file opens: OpenIDEA reports a window's first (empty) focused editor before plugins load.

## License

Apache 2.0, like the code it ports; see `NOTICE`.
