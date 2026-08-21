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

    /// Clear any prior runtime before the first etcd connection. Generated
    /// configurations live on tmpfs and are rebuilt only from the current
    /// etcd generation; a persistent LKG remains available for explicit
    /// rollback/recovery but is never auto-applied at daemon startup.
    fn initialize_local_runtime(&mut self) -> Result<()> {
        let stop_paths = self.paths.clone();
        let stop_runner = self.runner.clone();
        self.initialize_local_runtime_with(move || {
            Reconciler::new(stop_paths, stop_runner).runtime_stop()
        })
    }

    fn initialize_local_runtime_with<S>(&mut self, runtime_stop: S) -> Result<()>
    where
        S: FnOnce() -> Result<()>,
    {
        if self.local_initialized {
            return Ok(());
        }
        // A successful safety stop is a one-shot barrier. A transient stop
        // failure must be retried before etcd is allowed to drive a new apply
        // over an unknown local runtime state.
        runtime_stop().context("initial runtime safety stop failed")?;
        self.local_initialized = true;
        tracing::info!("volatile runtime initialized; waiting for etcd");
        Ok(())
    }

    async fn iteration(
        &mut self,
        etcd: &mut Option<Etcd>,
        next_report: &mut Instant,
    ) -> Result<()> {
        self.initialize_local_runtime()?;
        // Once a generation is locally committed, supervise its native
        // runtimes before touching etcd. This keeps VPN and FRR recovery
        // working while the server or management path is unavailable.
        if self.commit.is_some() {
            Reconciler::new(self.paths.clone(), self.runner.clone())
                .ensure_runtime(&self.settings)?;
        }
        if etcd.is_none() {
            self.persist_etcd_state("connecting");
            *etcd = Some(Etcd::connect(&self.settings).await?);
        }
        let client = etcd.as_mut().expect("connected above");
        let operation = self.reconcile_generation(client).await;

        // Reporting describes reachability and the currently observed local
        // runtime, not whether the newest desired generation was accepted.
        // Keep it independent from snapshot validation/apply so a broken
        // generation still exposes the node as online and its VPNs as
        // down/connecting/unavailable in etcd.
        self.flush_last_ack(client).await;
        let report = if Instant::now() >= *next_report {
            match self.publish_report(client).await {
                Ok(()) => {
                    *next_report = Instant::now() + Duration::from_secs(15);
                    Ok(())
                }
                Err(error) => Err(error),
            }
        } else {
            Ok(())
        };

        match (operation, report) {
            (Err(operation), Err(report)) => {
                tracing::warn!("status report also failed: {report:#}");
                Err(operation)
            }
            (Err(operation), Ok(())) => Err(operation),
            (Ok(()), Err(report)) => Err(report),
            (Ok(()), Ok(())) => Ok(()),
        }
    }

    async fn reconcile_generation(&mut self, client: &mut Etcd) -> Result<()> {
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
        // Reachability must not depend on a readable local manifest or on
        // individual VPN probes. Publish the lease first, then enrich the
        // report with local runtime state.
        client
            .put_with_lease(
                &format!("/updated/{}/online", self.settings.node_id),
                "1",
                60,
            )
            .await?;
        let status = report::collect(&self.paths, &self.runner)?;
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
            firewall_zone: None,
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
    fn startup_does_not_auto_apply_a_persistent_lkg() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(Some(temp.path()));
        std::fs::create_dir_all(&paths.state).unwrap();
        std::fs::write(&paths.cache, b"test cache marker").unwrap();
        let mut agent = agent(paths);
        let safety_stops = Cell::new(0usize);

        agent
            .initialize_local_runtime_with(|| {
                safety_stops.set(safety_stops.get() + 1);
                Ok(())
            })
            .unwrap();
        assert!(agent.local_initialized);
        assert!(agent.commit.is_none());
        assert!(agent.pending_last_ack.is_none());
        assert_eq!(safety_stops.get(), 1);
        agent
            .initialize_local_runtime_with(|| {
                safety_stops.set(safety_stops.get() + 1);
                Ok(())
            })
            .unwrap();
        assert_eq!(safety_stops.get(), 1);
    }

    #[test]
    fn startup_retries_a_failed_runtime_stop_then_runs_it_once() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(Some(temp.path()));
        let mut agent = agent(paths);
        let safety_stops = Cell::new(0usize);

        assert!(
            agent
                .initialize_local_runtime_with(|| {
                    safety_stops.set(safety_stops.get() + 1);
                    bail!("injected safety-stop diagnostic")
                })
                .is_err()
        );
        assert!(!agent.local_initialized);
        assert_eq!(safety_stops.get(), 1);

        agent
            .initialize_local_runtime_with(|| {
                safety_stops.set(safety_stops.get() + 1);
                Ok(())
            })
            .unwrap();
        assert!(agent.local_initialized);
        assert_eq!(safety_stops.get(), 2);

        agent
            .initialize_local_runtime_with(|| panic!("safety stop must not repeat"))
            .unwrap();
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
