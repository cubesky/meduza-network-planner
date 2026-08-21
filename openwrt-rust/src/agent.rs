use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::time::{Instant, sleep};

use crate::command::Runner;
use crate::config::Settings;
use crate::etcd::Etcd;
use crate::reconciler::Reconciler;
use crate::report;
use crate::state::{Paths, Snapshot, read_manifest, regular_file_exists};

pub struct Agent<R: Runner> {
    paths: Paths,
    runner: R,
    settings: Settings,
    local_initialized: bool,
    commit: Option<String>,
    pending_last_ack: Option<String>,
}

impl<R: Runner> Agent<R> {
    pub fn load(paths: Paths, runner: R) -> Result<Self> {
        let settings = Settings::load(&runner)?;
        Ok(Self {
            paths,
            runner,
            settings,
            local_initialized: false,
            commit: None,
            pending_last_ack: None,
        })
    }

    pub async fn serve(mut self) -> Result<()> {
        if !self.settings.enabled {
            Reconciler::new(self.paths, self.runner).purge()?;
            return Ok(());
        }
        Reconciler::new(self.paths.clone(), self.runner.clone()).prepare()?;
        self.persist_etcd_state("waiting");

        let mut delay = 1u64;
        let mut next_report = Instant::now();
        let mut etcd: Option<Etcd> = None;
        // Create and poll exactly one signal future for the daemon lifetime.
        // Recreating ctrl_c() around a synchronous local restore can leave a
        // window with Tokio's global handler installed but no live receiver,
        // causing the first SIGINT to be lost.
        let shutdown = shutdown_signal();
        tokio::pin!(shutdown);

        loop {
            let Some(result) = run_or_shutdown(
                shutdown.as_mut(),
                self.iteration(&mut etcd, &mut next_report),
            )
            .await
            else {
                self.persist_etcd_state("stopped");
                tracing::info!("shutdown requested");
                return Ok(());
            };
            let wait = match result {
                Ok(()) => {
                    delay = 1;
                    5
                }
                Err(error) => {
                    self.persist_etcd_state("error");
                    tracing::error!(retry_seconds = delay, "operation failed: {error:#}");
                    etcd = None;
                    let wait = delay;
                    delay = (delay * 2).min(60);
                    wait
                }
            };
            if run_or_shutdown(shutdown.as_mut(), sleep(Duration::from_secs(wait)))
                .await
                .is_none()
            {
                self.persist_etcd_state("stopped");
                tracing::info!("shutdown requested");
                return Ok(());
            }
        }
    }

    /// Run the local boot/recovery barrier before an iteration is allowed to
    /// touch etcd. `true` means that this iteration was consumed by local
    /// initialization and the caller must return before connecting to etcd.
    fn restore_lkg(&mut self) -> Result<bool> {
        let apply_paths = self.paths.clone();
        let apply_runner = self.runner.clone();
        let apply_settings = self.settings.clone();
        let stop_paths = self.paths.clone();
        let stop_runner = self.runner.clone();
        self.restore_lkg_with(
            move |source| {
                let snapshot = Snapshot::read_from(source)?;
                Reconciler::new(apply_paths, apply_runner).apply(&apply_settings, &snapshot)?;
                Ok(snapshot)
            },
            move || Reconciler::new(stop_paths, stop_runner).runtime_stop(),
        )
    }

    fn restore_lkg_with<A, S>(&mut self, apply: A, runtime_stop: S) -> Result<bool>
    where
        A: FnOnce(&std::path::Path) -> Result<Snapshot>,
        S: FnOnce() -> Result<()>,
    {
        if self.local_initialized {
            return Ok(false);
        }
        let source = if regular_file_exists(&self.paths.cache)? {
            Some(self.paths.cache.clone())
        } else if regular_file_exists(&self.paths.cache_pending)? {
            Some(self.paths.cache_pending.clone())
        } else {
            None
        };
        let Some(source) = source else {
            tracing::info!("no persistent configuration cache; waiting for etcd");
            // A successful safety stop is a one-shot barrier. A transient stop
            // failure must be retried before etcd is allowed to drive a new
            // apply over an unknown local runtime state.
            runtime_stop().context("initial runtime safety stop failed")?;
            self.local_initialized = true;
            return Ok(true);
        };
        match apply(&source) {
            Ok(snapshot) => {
                self.commit = Some(snapshot.commit);
                self.pending_last_ack = Some(if snapshot.applied_at.is_empty() {
                    report::timestamp()
                } else {
                    snapshot.applied_at
                });
                self.local_initialized = true;
                tracing::info!("restored persistent last-known-good configuration");
                Ok(true)
            }
            Err(error) => {
                tracing::error!("persistent configuration cache was not restored: {error:#}");
                // Leave local_initialized false. The next exponentially
                // backed-off iteration must retry the durable cache before it
                // is allowed to attempt an etcd connection.
                if let Err(stop_error) = runtime_stop() {
                    tracing::error!("runtime safety stop failed: {stop_error:#}");
                }
                Err(error.context("persistent configuration cache was not restored"))
            }
        }
    }

    async fn iteration(
        &mut self,
        etcd: &mut Option<Etcd>,
        next_report: &mut Instant,
    ) -> Result<()> {
        if self.restore_lkg()? {
            return Ok(());
        }
        // Once a generation is locally committed, supervise its native
        // runtimes before touching etcd. This keeps VPN and FRR recovery
        // working while the server or management path is unavailable.
        if self.commit.is_some() {
            Reconciler::new(self.paths.clone(), self.runner.clone()).ensure_runtime()?;
        }
        if etcd.is_none() {
            self.persist_etcd_state("connecting");
            *etcd = Some(Etcd::connect(&self.settings).await?);
        }
        let client = etcd.as_mut().expect("connected above");
        let (commit, revision) = client.get_with_revision("/commit").await?;
        crate::state::validate_commit(&commit)?;
        self.persist_etcd_state_with_commit("connected", Some(commit.clone()));
        if self.commit.as_deref() != Some(commit.as_str()) {
            let snapshot = self
                .fetch_snapshot(client, commit.clone(), revision)
                .await?;
            let stable_before =
                regular_file_exists(&self.paths.cache)?.then(|| self.paths.cache.clone());
            if let Err(error) = Reconciler::new(self.paths.clone(), self.runner.clone())
                .apply(&self.settings, &snapshot)
            {
                tracing::error!(commit = ?commit, "configuration apply failed: {error:#}");
                self.rollback(stable_before.as_deref());
                return Err(error);
            }
            self.commit = Some(commit);
            self.pending_last_ack = Some(snapshot.applied_at);
        }

        self.flush_last_ack(client).await;
        if Instant::now() >= *next_report {
            self.publish_report(client).await?;
            *next_report = Instant::now() + Duration::from_secs(15);
        }
        Ok(())
    }

    fn persist_etcd_state(&self, state: &str) {
        self.persist_etcd_state_with_commit(state, self.commit.clone());
    }

    fn persist_etcd_state_with_commit(&self, state: &str, commit: Option<String>) {
        let value = report::EtcdStatus::new(state, &self.settings.node_id, commit);
        if let Err(error) = report::persist_etcd_status(&self.paths, &value) {
            tracing::warn!("could not persist local etcd status: {error:#}");
        }
    }

    async fn fetch_snapshot(
        &self,
        client: &mut Etcd,
        commit: String,
        revision: i64,
    ) -> Result<Snapshot> {
        let mut budget = crate::model::FlattenedBudget::default();
        let node = client
            .get_prefix_at(
                &format!("/nodes/{}/", self.settings.node_id),
                revision,
                &mut budget,
            )
            .await?;
        let global = client
            .get_prefix_at("/global/", revision, &mut budget)
            .await?;
        let all_nodes = client
            .get_prefix_at("/nodes/", revision, &mut budget)
            .await?;
        let confirmed = client.get("/commit").await?;
        if confirmed != commit {
            anyhow::bail!("etcd commit changed while the snapshot was being read");
        }
        let snapshot = Snapshot {
            version: 1,
            node_id: self.settings.node_id.clone(),
            commit,
            applied_at: report::timestamp(),
            node,
            global,
            all_nodes,
        };
        // Enforce the aggregate budget across all three prefix responses
        // before rendering or persisting any part of this generation.
        snapshot.validate()?;
        Ok(snapshot)
    }

    fn rollback(&mut self, stable: Option<&std::path::Path>) {
        let result: Result<Snapshot> = (|| {
            let stable = stable.context("stable cache disappeared")?;
            let snapshot = Snapshot::read_from(stable)?;
            Reconciler::new(self.paths.clone(), self.runner.clone())
                .apply(&self.settings, &snapshot)?;
            Ok(snapshot)
        })();
        match result {
            Ok(snapshot) => {
                self.commit = Some(snapshot.commit);
                self.pending_last_ack = Some(snapshot.applied_at);
                tracing::info!("rolled back to stable last-known-good configuration");
            }
            Err(_) => {
                self.commit = None;
                if let Err(error) =
                    Reconciler::new(self.paths.clone(), self.runner.clone()).runtime_stop()
                {
                    tracing::error!("runtime stop after failed first apply also failed: {error:#}");
                }
            }
        }
    }

    async fn flush_last_ack(&mut self, client: &mut Etcd) {
        let Some(value) = self.pending_last_ack.clone() else {
            return;
        };
        match client
            .put(&format!("/updated/{}/last", self.settings.node_id), &value)
            .await
        {
            Ok(()) => self.pending_last_ack = None,
            Err(error) => {
                tracing::warn!("local apply is durable; etcd last ACK will retry: {error:#}")
            }
        }
    }

    async fn publish_report(&self, client: &mut Etcd) -> Result<()> {
        let status = report::collect(&self.paths, &self.runner)?;
        client
            .put_with_lease(
                &format!("/updated/{}/online", self.settings.node_id),
                "1",
                60,
            )
            .await?;
        let mut current = Vec::new();
        for (path, state) in &status.interfaces {
            let Some((kind, name)) = path.split_once('/') else {
                continue;
            };
            client
                .put(
                    &format!("/updated/{}/{kind}/{name}/status", self.settings.node_id),
                    &format!("{state} {}", status.observed_at),
                )
                .await?;
            if matches!(kind, "openvpn" | "wireguard") {
                current.push((kind.to_owned(), name.to_owned()));
            }
        }
        client
            .put(
                &format!("/updated/{}/frr/default/status", self.settings.node_id),
                &format!("{} {}", status.frr, status.observed_at),
            )
            .await?;
        let current_set = current
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        for (kind, name) in
            report::read_reported(&self.paths, &self.settings.node_id)?.difference(&current_set)
        {
            client
                .put(
                    &format!("/updated/{}/{kind}/{name}/status", self.settings.node_id),
                    &format!("down {}", status.observed_at),
                )
                .await?;
        }
        report::persist_reported(&self.paths, &self.settings.node_id, &current)?;
        Ok(())
    }
}

#[cfg(unix)]
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut terminate = signal(SignalKind::terminate()).expect("SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = terminate.recv() => {},
    }
}

async fn run_or_shutdown<S, W, T>(mut shutdown: Pin<&mut S>, work: W) -> Option<T>
where
    S: Future<Output = ()> + ?Sized,
    W: Future<Output = T>,
{
    tokio::select! {
        biased;
        _ = shutdown.as_mut() => None,
        result = work => Some(result),
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

#[allow(dead_code)]
fn _manifest_is_readable(paths: &Paths) -> Result<()> {
    let _ = read_manifest(&paths.manifest)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::ffi::OsStr;
    use std::future::Future;
    use std::pin::Pin;
    use std::process::Output;
    use std::task::{Context as TaskContext, Poll};

    use anyhow::bail;

    use super::*;

    #[derive(Clone, Copy)]
    struct NoCommands;

    impl Runner for NoCommands {
        fn output<I, S>(&self, _program: &str, _args: I) -> Result<Output>
        where
            I: IntoIterator<Item = S>,
            S: AsRef<OsStr>,
        {
            panic!("local bootstrap test unexpectedly executed a command")
        }
    }

    fn settings() -> Settings {
        Settings {
            enabled: true,
            node_id: "router-01".into(),
            endpoints: vec!["https://127.0.0.1:2379".into()],
            ca: None,
            cert: None,
            key: None,
            user: None,
            password: None,
        }
    }

    fn snapshot() -> Snapshot {
        Snapshot {
            version: 1,
            node_id: "router-01".into(),
            commit: "generation-1".into(),
            applied_at: "2026-08-21T00:00:00Z".into(),
            node: Default::default(),
            global: Default::default(),
            all_nodes: Default::default(),
        }
    }

    fn agent(paths: Paths) -> Agent<NoCommands> {
        Agent {
            paths,
            runner: NoCommands,
            settings: settings(),
            local_initialized: false,
            commit: None,
            pending_last_ack: None,
        }
    }

    #[test]
    fn local_lkg_retries_before_etcd_and_succeeds_on_second_attempt() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(Some(temp.path()));
        std::fs::create_dir_all(&paths.state).unwrap();
        std::fs::write(&paths.cache, b"test cache marker").unwrap();
        let mut agent = agent(paths);
        let apply_attempts = Cell::new(0usize);
        let safety_stops = Cell::new(0usize);
        let etcd_connections = Cell::new(0usize);

        let first = agent.restore_lkg_with(
            |_| {
                apply_attempts.set(apply_attempts.get() + 1);
                bail!("injected first local apply failure")
            },
            || {
                safety_stops.set(safety_stops.get() + 1);
                Ok(())
            },
        );
        if first.as_ref().is_ok_and(|consumed| !consumed) {
            etcd_connections.set(etcd_connections.get() + 1);
        }
        assert!(first.is_err());
        assert!(!agent.local_initialized);
        assert_eq!(safety_stops.get(), 1);
        assert_eq!(etcd_connections.get(), 0);

        let second = agent
            .restore_lkg_with(
                |_| {
                    apply_attempts.set(apply_attempts.get() + 1);
                    Ok(snapshot())
                },
                || panic!("a successful local retry must not run the safety stop"),
            )
            .unwrap();
        if !second {
            etcd_connections.set(etcd_connections.get() + 1);
        }
        assert!(second, "the successful local retry consumes this iteration");
        assert!(agent.local_initialized);
        assert_eq!(agent.commit.as_deref(), Some("generation-1"));
        assert_eq!(apply_attempts.get(), 2);
        assert_eq!(etcd_connections.get(), 0);

        assert!(
            !agent
                .restore_lkg_with(
                    |_| panic!("initialized local state must not be applied again"),
                    || panic!("initialized local state must not be stopped again"),
                )
                .unwrap()
        );
    }

    #[test]
    fn missing_cache_retries_a_failed_stop_then_consumes_it_once() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(Some(temp.path()));
        let mut agent = agent(paths);
        let safety_stops = Cell::new(0usize);

        assert!(
            agent
                .restore_lkg_with(
                    |_| panic!("no cache must not be applied"),
                    || {
                        safety_stops.set(safety_stops.get() + 1);
                        bail!("injected safety-stop diagnostic")
                    },
                )
                .is_err()
        );
        assert!(!agent.local_initialized);
        assert_eq!(safety_stops.get(), 1);

        assert!(
            agent
                .restore_lkg_with(
                    |_| panic!("no cache must not be applied"),
                    || {
                        safety_stops.set(safety_stops.get() + 1);
                        Ok(())
                    },
                )
                .unwrap()
        );
        assert!(agent.local_initialized);
        assert_eq!(safety_stops.get(), 2);

        assert!(
            !agent
                .restore_lkg_with(
                    |_| panic!("initialized no-cache state must not apply"),
                    || panic!("no-cache safety stop must not repeat"),
                )
                .unwrap()
        );
        assert_eq!(safety_stops.get(), 2);
    }

    struct PendingOnce(bool);

    impl Future for PendingOnce {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, context: &mut TaskContext<'_>) -> Poll<Self::Output> {
            if self.0 {
                Poll::Ready(())
            } else {
                self.0 = true;
                context.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }

    #[tokio::test]
    async fn one_shutdown_future_survives_a_completed_iteration() {
        let mut shutdown = Box::pin(PendingOnce(false));
        assert_eq!(
            run_or_shutdown(shutdown.as_mut(), std::future::ready(7)).await,
            Some(7)
        );
        assert_eq!(
            run_or_shutdown(shutdown.as_mut(), std::future::pending::<u8>()).await,
            None
        );
    }
}
