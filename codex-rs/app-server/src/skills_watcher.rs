use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use crate::outgoing_message::OutgoingMessageSender;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::SkillsChangedNotification;
use codex_core::ThreadManager;
use codex_core::config::Config;
use codex_core::skills::SkillsLoadInput;
use codex_core::skills::SkillsService;
use codex_file_watcher::FileWatcher;
use codex_file_watcher::FileWatcherSubscriber;
use codex_file_watcher::Receiver;
use codex_file_watcher::ThrottledWatchReceiver;
use codex_file_watcher::WatchPath;
use codex_file_watcher::WatchRegistration;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_utils_absolute_path::AbsolutePathBuf;
use tokio_util::sync::CancellationToken;
use tokio_util::sync::DropGuard;
use tracing::warn;

#[cfg(not(test))]
const WATCHER_THROTTLE_INTERVAL: Duration = Duration::from_secs(10);
#[cfg(test)]
const WATCHER_THROTTLE_INTERVAL: Duration = Duration::from_millis(50);

pub(crate) struct SkillsWatcher {
    subscriber: FileWatcherSubscriber,
    runtime_extra_roots_registration: Mutex<WatchRegistration>,
    shutdown_token: CancellationToken,
    _shutdown_drop_guard: DropGuard,
}

impl SkillsWatcher {
    pub(crate) fn new(
        skills_service: Arc<SkillsService>,
        outgoing: Arc<OutgoingMessageSender>,
    ) -> Arc<Self> {
        let file_watcher = match FileWatcher::new() {
            Ok(file_watcher) => Arc::new(file_watcher),
            Err(err) => {
                warn!("failed to initialize skills file watcher: {err}");
                Arc::new(FileWatcher::noop())
            }
        };
        let (subscriber, rx) = file_watcher.add_subscriber();
        let shutdown_token = CancellationToken::new();
        let shutdown_drop_guard = shutdown_token.clone().drop_guard();
        Self::spawn_event_loop(rx, skills_service, outgoing, shutdown_token.child_token());
        Arc::new(Self {
            subscriber,
            runtime_extra_roots_registration: Mutex::new(WatchRegistration::default()),
            shutdown_token,
            _shutdown_drop_guard: shutdown_drop_guard,
        })
    }

    pub(crate) fn shutdown(&self) {
        self.shutdown_token.cancel();
    }

    pub(crate) fn register_runtime_extra_roots(&self, extra_roots: &[AbsolutePathBuf]) {
        let roots = extra_roots
            .iter()
            .map(|root| WatchPath {
                path: root.clone().into_path_buf(),
                recursive: safe_recursive(&root.clone().into_path_buf()),
            })
            .collect();
        let registration = self.subscriber.register_paths(roots);
        let mut guard = self
            .runtime_extra_roots_registration
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = registration;
    }

    pub(crate) async fn register_thread_config(
        &self,
        config: &Config,
        thread_manager: &ThreadManager,
        environments: &[TurnEnvironmentSelection],
    ) -> WatchRegistration {
        let Some(environment_selection) = environments.first() else {
            return WatchRegistration::default();
        };
        let Some(environment) = thread_manager
            .environment_manager()
            .get_environment(&environment_selection.environment_id)
        else {
            warn!(
                "failed to register skills watcher for unknown environment `{}`",
                environment_selection.environment_id
            );
            return WatchRegistration::default();
        };
        if environment.is_remote() {
            return WatchRegistration::default();
        }

        let plugins_input = config.plugins_config_input();
        let plugins_manager = thread_manager.plugins_manager();
        let plugin_outcome = plugins_manager.plugins_for_config(&plugins_input).await;
        let skills_input = SkillsLoadInput::new(
            config.cwd.clone(),
            plugin_outcome.effective_plugin_skill_roots(),
            config.config_layer_stack.clone(),
            config.bundled_skills_enabled(),
        );
        let roots = thread_manager
            .skills_service()
            .skill_roots_for_config(&skills_input, Some(environment.get_filesystem()))
            .await
            .into_iter()
            // Plugin roots are invalidated by plugin lifecycle operations.
            .filter(|root| root.plugin_id.is_none())
            .map(|root| {
                let path = root.path.into_path_buf();
                WatchPath {
                    recursive: safe_recursive(&path),
                    path,
                }
            })
            .collect();
        self.subscriber.register_paths(roots)
    }

    fn spawn_event_loop(
        rx: Receiver,
        skills_service: Arc<SkillsService>,
        outgoing: Arc<OutgoingMessageSender>,
        shutdown_token: CancellationToken,
    ) {
        let mut rx = ThrottledWatchReceiver::new(rx, WATCHER_THROTTLE_INTERVAL);
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            warn!("skills watcher listener skipped: no Tokio runtime available");
            return;
        };
        handle.spawn(async move {
            loop {
                let event = tokio::select! {
                    _ = shutdown_token.cancelled() => break,
                    event = rx.recv() => event,
                };
                if event.is_none() {
                    break;
                }
                skills_service.clear_cache();
                outgoing
                    .send_server_notification(ServerNotification::SkillsChanged(
                        SkillsChangedNotification {},
                    ))
                    .await;
            }
        });
    }
}

/// Decides whether a skill root may be watched recursively without following a
/// symlink into an external tree.
///
/// `notify` is configured with `follow_symlinks(false)`, which only prevents
/// following symlinks encountered *during* recursive traversal. When the root
/// itself is a directory symlink, [`FileWatcher`] canonicalizes it before
/// watching, so the recursion would still descend into the symlink target.
/// Detect that case here: if the root is a symlink whose resolved target
/// escapes the root's own parent directory, watch it non-recursively so only
/// the link itself (and direct children) are observed rather than an
/// arbitrarily large external tree.
///
/// Roots that are not symlinks, or whose target stays within their parent
/// directory, are watched recursively as before.
fn safe_recursive(path: &Path) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        // Missing path: let the watcher handle the fallback ancestor watch.
        // Default to recursive to preserve prior behavior for not-yet-created
        // skill roots.
        return true;
    };
    if !metadata.file_type().is_symlink() {
        return true;
    }
    let Some(parent) = path.parent() else {
        // A symlink with no parent (e.g. "/"); conservatively do not recurse.
        return false;
    };
    let Ok(real_target) = fs::canonicalize(path) else {
        // Broken symlink; nothing to recurse into anyway.
        return false;
    };
    let Ok(anchor) = fs::canonicalize(parent) else {
        return false;
    };
    real_target.starts_with(anchor)
}

#[cfg(test)]
mod safe_recursive_tests {
    use super::safe_recursive;
    use std::path::PathBuf;
    use tempfile::tempdir;

    #[test]
    fn real_directory_is_recursive() {
        let dir = tempdir().expect("tempdir");
        assert!(safe_recursive(dir.path()));
    }

    #[cfg(unix)]
    #[cfg(unix)]
    #[test]
    fn symlink_escaping_parent_is_not_recursive() {
        use std::fs;
        use std::os::unix::fs::symlink;
        let outer = tempdir().expect("tempdir");
        let escape = outer.path().join("escape_target");
        fs::create_dir_all(&escape).expect("create escape target");
        let link = outer.path().join("result");
        // Point the symlink outside of `outer` by targeting an absolute path
        // that does not live under `outer`.
        let elsewhere = tempdir().expect("tempdir elsewhere");
        let elsewhere_sub = elsewhere.path().join("big_tree");
        fs::create_dir_all(&elsewhere_sub).expect("create big tree");
        symlink(&elsewhere_sub, &link).expect("symlink");
        assert!(!safe_recursive(&link));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_within_parent_is_recursive() {
        use std::fs;
        use std::os::unix::fs::symlink;
        let outer = tempdir().expect("tempdir");
        let real = outer.path().join("real_skill");
        fs::create_dir_all(&real).expect("create real skill");
        let link = outer.path().join("alias");
        symlink(&real, &link).expect("symlink");
        assert!(safe_recursive(&link));
    }

    #[test]
    fn missing_path_defaults_recursive() {
        let missing = PathBuf::from("/this/path/does/not/exist/for/codex/test");
        assert!(safe_recursive(&missing));
    }
}
