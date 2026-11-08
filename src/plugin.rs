//! The plugin itself: commands, status item and BUILD-file change tracking over [`crate::sync`].

use openidea_plugin::host::{self, Project};
use openidea_plugin::{
    Context, Error, Event, Invocation, LogLevel, NotificationAction, NotificationLevel, Plugin,
    StatusItem, register_plugin,
};

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

#[derive(Default)]
pub struct BazelPlugin {
    /// The granted Bazel launcher: `bazelisk` (preferred, honours `.bazelversion`) or `bazel`.
    bazel: Option<String>,
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
        // On start-up, point Bazel workspaces that were never synced at the sync command.
        if let Ok(projects) = host::projects() {
            for project in projects {
                if is_bazel_workspace(&project) && !project.exists(".classpath").unwrap_or(true) {
                    let _ = host::notify_with_actions(
                        NotificationLevel::Info,
                        "Bazel workspace",
                        "Sync the project with its BUILD files to resolve Java code from Bazel.",
                        &[NotificationAction::new("Sync", "sync")],
                    );
                }
            }
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
            "open-project-view" => {
                if !project.exists(sync::PROJECT_VIEW)? {
                    project.write_file(sync::PROJECT_VIEW, DEFAULT_PROJECT_VIEW)?;
                }
                open(&project, sync::PROJECT_VIEW)
            }
            "reopen" => {
                host::open_project(&project.root()?, false)?;
                Ok(None)
            }
            _ => Err(Error::NotFound(format!("command {id}"))),
        }
    }

    fn handle_events(&mut self, events: &[Event], _ctx: &Invocation) {
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
    let mut actions = vec![NotificationAction::new("Show Log", "open-sync-log")];
    let level = if s.warnings.is_empty() {
        NotificationLevel::Info
    } else {
        NotificationLevel::Warning
    };
    if s.project_changed || s.settings_changed {
        // jdtls only reads Eclipse metadata and its settings when it starts.
        body.push_str(" Reopen the project so the Java language server loads it.");
        actions.insert(0, NotificationAction::new("Reopen Project", "reopen"));
    }
    let _ = host::notify_with_actions(level, "Bazel sync finished", &body, &actions);
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
