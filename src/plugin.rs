//! The plugin itself: commands, status item and BUILD-file change tracking over [`crate::sync`].

use openidea_plugin::host::{self, Project};
use openidea_plugin::{
    Context, Error, Event, Invocation, LogLevel, NotificationAction, NotificationLevel, Plugin,
    StatusItem, register_plugin,
};

use crate::fxhash::FxHashSet;
use crate::ide;
use crate::sync::{self, SYNC_LOG, SyncError, SyncSummary};

/// Edits that change what a sync produces (IntelliJ's "BUILD files changed" check).
const BUILD_GLOBS: &[&str] = &[
    "**/BUILD",
    "**/BUILD.bazel",
    "**/*.bzl",
    "MODULE.bazel",
    "WORKSPACE",
    "WORKSPACE.bazel",
    ".bazelproject",
    ".bazelrc",
];
const STATUS: &str = "sync";
/// Setting: sync a Bazel workspace that was never synced when it opens.
const SYNC_ON_OPEN: &str = "syncOnOpen";

#[derive(Default)]
pub struct BazelPlugin {
    /// The granted Bazel launcher: `bazelisk` (preferred, honours `.bazelversion`) or `bazel`.
    bazel: Option<String>,
    /// Project roots already checked for a sync on open in this session.
    opened: FxHashSet<String>,
}

impl Plugin for BazelPlugin {
    fn activate(&mut self, ctx: &Context) -> Result<(), Error> {
        self.bazel = ["bazelisk", "bazel"]
            .into_iter()
            .find(|p| {
                ctx.capabilities
                    .iter()
                    .any(|c| c == &format!("process:{p}"))
            })
            .map(String::from);
        if ctx.capabilities.iter().any(|c| c == "events") {
            host::subscribe_paths("files-changed", BUILD_GLOBS)?;
        }
        // Projects already open when the plugin starts; the ones a window opens later come in
        // through its first `editor-focused` event (see `handle_events`).
        for project in host::projects().unwrap_or_default() {
            self.on_project_open(&project);
        }
        Ok(())
    }

    fn run_command(
        &mut self,
        id: &str,
        ctx: &Invocation,
        _arg: Option<&str>,
    ) -> Result<Option<String>, Error> {
        let project = ctx.project().ok_or("no project is open")?;
        match id {
            "sync" => self.sync(&project),
            "open-sync-log" => open(&project, SYNC_LOG),
            "test-file" => self.test_file(&project, ctx, false),
            "debug-test-file" => self.test_file(&project, ctx, true),
            "open-project-view" => {
                if !project.exists(sync::PROJECT_VIEW)? {
                    project.write_file(sync::PROJECT_VIEW, DEFAULT_PROJECT_VIEW)?;
                }
                open(&project, sync::PROJECT_VIEW)
            }
            _ => Err(Error::NotFound(format!("command {id}"))),
        }
    }

    fn handle_events(&mut self, events: &[Event], ctx: &Invocation) {
        // A window reports its focused editor (or none) as soon as it starts: its project is open.
        if events.iter().any(|e| matches!(e, Event::EditorFocused(_)))
            && let Some(project) = ctx.project()
        {
            self.on_project_open(&project);
        }
        let build_changed = events.iter().any(|e| match e {
            Event::FilesChanged(c) => c
                .created
                .iter()
                .chain(&c.modified)
                .chain(&c.removed)
                .any(|p| !p.contains("/.openidea/") && !p.contains("/bazel-")),
            _ => false,
        });
        if build_changed {
            set_status(
                "Bazel: BUILD files changed",
                "BUILD files changed since the last sync — click to sync",
            );
        }
    }
}

impl BazelPlugin {
    /// IntelliJ's `BlazeSyncStartupActivity`, once per project and session: a Bazel workspace
    /// without project data syncs when it opens; one with data only restores it, which here is
    /// already on disk (the Eclipse project).
    fn on_project_open(&mut self, project: &Project) {
        let Ok(root) = project.root() else { return };
        if !self.opened.insert(root) || !is_bazel_workspace(project) || has_project_data(project) {
            return;
        }
        if host::setting_bool(SYNC_ON_OPEN).unwrap_or(true) && self.bazel.is_some() {
            // A cancelled or failed sync has told the user already.
            if let Err(e) = self.sync(project) {
                host::log(LogLevel::Info, &format!("sync on open: {e}"));
            }
        } else {
            let _ = host::notify_with_actions(
                NotificationLevel::Info,
                "Bazel workspace",
                "Sync the project with its BUILD files to resolve Java code from Bazel.",
                &[NotificationAction::new("Sync", "sync")],
            );
        }
    }

    /// Runs the focused file's test target through Bazel in a terminal, as IntelliJ's Bazel plugin
    /// runs tests (`bazel test`); `debug` adds `--java_debug`, so the test JVM waits for a debugger
    /// on port 5005, which Attach to Process lists as a JDWP process.
    fn test_file(
        &mut self,
        project: &Project,
        ctx: &Invocation,
        debug: bool,
    ) -> Result<Option<String>, Error> {
        let bazel = self
            .bazel
            .clone()
            .ok_or_else(|| Error::Denied("process:bazelisk".into()))?;
        let editor = ctx
            .editor()
            .ok_or("open the test file in an editor first")?;
        let root = project.root()?;
        let path = editor.path();
        let rel = path
            .strip_prefix(&format!("{}/", root.trim_end_matches('/')))
            .unwrap_or(&path)
            .to_string();
        let index = project
            .read_file(sync::TARGET_INDEX)
            .map_err(|_| Error::NotFound("the target index (sync the project first)".into()))?;
        // The class the file declares, as the test target names it: its `package` and its name.
        let text = editor.document().map(|d| d.text).unwrap_or_default();
        let stem = rel
            .rsplit('/')
            .next()
            .unwrap_or(&rel)
            .trim_end_matches(".java");
        let class = match crate::model::declared_package(&text) {
            Some(pkg) if !pkg.is_empty() => format!("{pkg}.{stem}"),
            _ => stem.to_string(),
        };
        let label = sync::test_targets_of(&index, &rel, Some(&class))
            .first()
            .map(|l| l.to_string())
            .ok_or_else(|| {
                Error::NotFound(format!(
                    "a Bazel test target for {rel} (sync after adding it)"
                ))
            })?;
        let mut args = vec!["test".to_string(), label.clone(), "--color=yes".into()];
        args.push(if debug {
            "--java_debug".into()
        } else {
            "--test_output=streamed".into()
        });
        let verb = if debug { "Debug" } else { "Test" };
        host::open_terminal(&format!("Bazel: {verb} {label}"), &bazel, &args)?;
        if debug {
            let _ = host::notify_with_actions(
                NotificationLevel::Info,
                "Bazel test waiting for the debugger",
                "The test JVM listens on port 5005 once Bazel starts it: Run › Attach to Process… (⌥⇧F5) and pick it.",
                &[],
            );
        }
        Ok(None)
    }

    fn sync(&mut self, project: &Project) -> Result<Option<String>, Error> {
        let bazel = self
            .bazel
            .clone()
            .ok_or_else(|| Error::Denied("process:bazelisk".into()))?;
        set_status(
            "Bazel: syncing…",
            "Syncing the project with its BUILD files",
        );
        let task = host::begin_task("Bazel: Sync Project with BUILD Files", true);
        let mut ide = ide::IdeHost {
            project,
            task: &task,
            bazel: &bazel,
        };
        let result = sync::sync(&mut ide);
        drop(task);
        match result {
            Ok(summary) => {
                set_status(
                    "Bazel: synced",
                    "Synced with the BUILD files — click to sync again",
                );
                notify_done(&summary);
                Ok(None)
            }
            Err(SyncError::Cancelled) => {
                set_status(
                    "Bazel: sync cancelled",
                    "The last sync was cancelled — click to sync",
                );
                Err(Error::Cancelled)
            }
            Err(SyncError::Failed(message)) => {
                set_status(
                    "Bazel: sync failed",
                    "The last sync failed — click to sync again",
                );
                host::log(LogLevel::Error, &message);
                let _ = host::notify_with_actions(
                    NotificationLevel::Error,
                    "Bazel sync failed",
                    &first_lines(&message, 12),
                    &[NotificationAction::new("Show Log", "open-sync-log")],
                );
                Ok(None)
            }
        }
    }
}

fn notify_done(s: &SyncSummary) {
    let mut body = format!(
        "{} source roots and {} libraries from {} targets.",
        s.source_roots, s.libraries, s.targets
    );
    if !s.warnings.is_empty() {
        body.push_str(&format!(" {} warnings.", s.warnings.len()));
    }
    let actions = [NotificationAction::new("Show Log", "open-sync-log")];
    let level = if s.warnings.is_empty() {
        NotificationLevel::Info
    } else {
        NotificationLevel::Warning
    };
    // jdtls reads the Eclipse project when it starts, and the IDE reads `.openidea/settings.toml`
    // when the project opens; plugins can trigger neither.
    if s.settings_changed {
        body.push_str(" Restart OpenIDEA (Find Action › Restart IDE) so the Java language server loads the Bazel project.");
    } else if s.project_changed {
        body.push_str(" Restart the Java language server (language servers in the status bar › jdtls › Restart) to load the new classpath.");
    }
    let _ = host::notify_with_actions(level, "Bazel sync finished", &body, &actions);
}

/// Whether a sync already wrote the project (IntelliJ's "has project data").
fn has_project_data(project: &Project) -> bool {
    project
        .read_file(".project")
        .is_ok_and(|text| text.contains(crate::eclipse::GENERATED_MARKER))
}

fn is_bazel_workspace(project: &Project) -> bool {
    [
        "MODULE.bazel",
        "WORKSPACE",
        "WORKSPACE.bazel",
        sync::PROJECT_VIEW,
    ]
    .iter()
    .any(|f| project.exists(f).unwrap_or(false))
}

fn open(project: &Project, path: &str) -> Result<Option<String>, Error> {
    if !project.exists(path)? {
        return Err(Error::NotFound(format!("{path} (sync the project first)")));
    }
    host::open_location(
        &format!("{}/{path}", project.root()?.trim_end_matches('/')),
        0,
        0,
    )?;
    Ok(None)
}

fn set_status(text: &str, tooltip: &str) {
    host::set_status_item(
        STATUS,
        &StatusItem {
            text: text.into(),
            tooltip: Some(tooltip.into()),
            command: Some("sync".into()),
        },
    );
}

fn first_lines(text: &str, n: usize) -> String {
    text.lines().take(n).collect::<Vec<_>>().join("\n")
}

const DEFAULT_PROJECT_VIEW: &str = "directories:\n  .\n\nderive_targets_from_directories: true\n";

register_plugin!(BazelPlugin);

#[cfg(test)]
mod tests {
    use openidea_plugin::mock::MockHost;
    use openidea_plugin::{Context, Event, FilesChanged};

    use super::*;

    const ALL: &[&str] = &[
        "project.read",
        "project.write",
        "process:bazelisk",
        "fs.read:/private/var/tmp",
        "events",
    ];

    fn activated(caps: &[&str]) -> BazelPlugin {
        let mut plugin = BazelPlugin::default();
        let ctx = Context {
            plugin_id: "davidhsv.bazel".into(),
            plugin_version: "0.1.0".into(),
            api_version: "1.3".into(),
            capabilities: caps.iter().map(|c| c.to_string()).collect(),
        };
        plugin.activate(&ctx).unwrap();
        plugin
    }

    fn read(host: &MockHost, path: &str) -> Option<String> {
        host.invocation()
            .project()
            .and_then(|p| p.read_file(path).ok())
    }

    #[test]
    fn sync_needs_a_bazel_grant() {
        let host = MockHost::install().with_capabilities(&["project.read", "project.write"]);
        host.add_file(".bazelproject", "directories:\n  .\n");
        let mut plugin = activated(&["project.read", "project.write"]);
        assert_eq!(
            plugin.run_command("sync", &host.invocation(), None),
            Err(Error::Denied("process:bazelisk".into()))
        );
        assert!(host.tasks().is_empty(), "nothing started");
    }

    #[test]
    fn a_failed_sync_runs_in_a_task_and_points_at_its_log() {
        let host = MockHost::install().with_capabilities(ALL);
        host.add_file(
            ".bazelproject",
            "directories:\n  .\n\nderive_targets_from_directories: true\n",
        );
        let mut plugin = activated(ALL);
        // The mock's processes print no `bazel info` values, so the sync stops after it.
        assert_eq!(
            plugin.run_command("sync", &host.invocation(), None),
            Ok(None)
        );

        let tasks = host.tasks();
        assert_eq!(tasks[0].title, "Bazel: Sync Project with BUILD Files");
        assert!(
            tasks[0]
                .reports
                .iter()
                .any(|(_, _, m)| m.as_deref() == Some("bazel info"))
        );
        let failed = host
            .notifications_with_actions()
            .into_iter()
            .find(|n| n.notification.title == "Bazel sync failed")
            .expect("failure notification");
        assert!(
            failed
                .notification
                .body
                .contains("bazel info printed no execution_root"),
            "{}",
            failed.notification.body
        );
        assert_eq!(failed.actions[0].command, "open-sync-log");
        assert_eq!(host.status_items()[STATUS].text, "Bazel: sync failed");
        assert!(read(&host, SYNC_LOG).unwrap().contains("Sync failed"));
        assert!(
            host.logs()
                .iter()
                .any(|(level, _)| *level == LogLevel::Error)
        );
    }

    #[test]
    fn a_never_synced_workspace_syncs_when_it_opens() {
        let host = MockHost::install().with_capabilities(ALL);
        host.add_file("MODULE.bazel", "module(name = \"demo\")\n");
        activated(ALL);
        assert_eq!(
            host.tasks()[0].title,
            "Bazel: Sync Project with BUILD Files"
        );
        assert!(read(&host, SYNC_LOG).is_some());
    }

    #[test]
    fn a_window_opening_a_never_synced_workspace_syncs_it_once() {
        let host = MockHost::install().with_capabilities(ALL);
        host.set_project_root(None);
        let mut plugin = activated(ALL);
        assert!(host.tasks().is_empty(), "no project open yet");
        host.set_project_root(Some("/project"));
        host.add_file("MODULE.bazel", "");
        let focused = [Event::EditorFocused(None)];
        plugin.handle_events(&focused, &host.invocation());
        assert_eq!(host.tasks().len(), 1);
        plugin.handle_events(&focused, &host.invocation());
        assert_eq!(host.tasks().len(), 1, "once per project and session");
    }

    #[test]
    fn a_synced_workspace_only_loads_when_it_opens() {
        let host = MockHost::install().with_capabilities(ALL);
        host.add_file("MODULE.bazel", "");
        host.add_file(
            ".project",
            "<projectDescription><comment>Generated from Bazel by openidea-bazel</comment></projectDescription>",
        );
        activated(ALL);
        assert!(host.tasks().is_empty());
        assert!(host.notifications_with_actions().is_empty());
    }

    #[test]
    fn with_sync_on_open_off_or_no_bazel_grant_a_new_workspace_gets_a_hint() {
        for (setting, caps) in [
            ("false", ALL),
            ("true", &["project.read", "project.write"][..]),
        ] {
            let host = MockHost::install().with_capabilities(caps);
            host.set_setting(SYNC_ON_OPEN, setting);
            host.add_file("MODULE.bazel", "");
            activated(caps);
            assert!(host.tasks().is_empty());
            let hint = host.notifications_with_actions();
            assert!(hint.iter().any(
                |n| n.notification.title == "Bazel workspace" && n.actions[0].command == "sync"
            ));
        }
    }

    #[test]
    fn activation_prefers_bazelisk_and_watches_build_files() {
        let host = MockHost::install().with_capabilities(ALL);
        let plugin = activated(&[
            "project.read",
            "project.write",
            "process:bazel",
            "process:bazelisk",
            "events",
        ]);
        assert_eq!(plugin.bazel.as_deref(), Some("bazelisk"));
        assert_eq!(host.path_subscriptions()["files-changed"], BUILD_GLOBS);

        let plugin = activated(&["project.read", "project.write", "process:bazel"]);
        assert_eq!(plugin.bazel.as_deref(), Some("bazel"));
    }

    #[test]
    fn build_file_edits_mark_the_project_out_of_sync_but_its_own_files_do_not() {
        let host = MockHost::install().with_capabilities(ALL);
        let mut plugin = activated(ALL);
        let changed = |path: &str| {
            [Event::FilesChanged(FilesChanged {
                created: vec![],
                modified: vec![path.into()],
                removed: vec![],
            })]
        };
        plugin.handle_events(
            &changed("/project/.openidea/bazel/aspects/java_info.bzl"),
            &host.invocation(),
        );
        assert!(!host.status_items().contains_key(STATUS));
        plugin.handle_events(&changed("/project/app/BUILD.bazel"), &host.invocation());
        assert_eq!(
            host.status_items()[STATUS].text,
            "Bazel: BUILD files changed"
        );
        assert_eq!(host.status_items()[STATUS].command.as_deref(), Some("sync"));
    }

    #[test]
    fn open_project_view_creates_a_default_one() {
        let host = MockHost::install().with_capabilities(ALL);
        let mut plugin = activated(ALL);
        plugin
            .run_command("open-project-view", &host.invocation(), None)
            .unwrap();
        assert_eq!(
            read(&host, sync::PROJECT_VIEW).as_deref(),
            Some(DEFAULT_PROJECT_VIEW)
        );
        assert_eq!(host.opened_locations()[0].0, "/project/.bazelproject");
    }

    #[test]
    fn the_sync_log_needs_a_sync() {
        let host = MockHost::install().with_capabilities(ALL);
        let mut plugin = activated(ALL);
        assert!(matches!(
            plugin.run_command("open-sync-log", &host.invocation(), None),
            Err(Error::NotFound(m)) if m.contains("sync the project first")
        ));
    }
}
