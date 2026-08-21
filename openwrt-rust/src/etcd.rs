use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use etcd_client::{
    Certificate, Client, ConnectOptions, GetOptions, Identity, KvClient, PutOptions, TlsOptions,
};

use crate::config::Settings;
use crate::model::FlattenedBudget;

const MAX_ETCD_RESPONSE_BYTES: usize = 6 * 1024 * 1024;

pub struct Etcd {
    client: Client,
    kv: KvClient,
}

impl Etcd {
    pub async fn connect(settings: &Settings) -> Result<Self> {
        let mut options = ConnectOptions::new()
            .with_connect_timeout(Duration::from_secs(10))
            .with_timeout(Duration::from_secs(10))
            .with_tcp_keepalive(Duration::from_secs(30));
        if let (Some(user), Some(password)) = (&settings.user, &settings.password) {
            options = options.with_user(user.clone(), password.clone());
        }
        if let Some((ca, identity)) = settings.tls_material()? {
            let mut tls = TlsOptions::new().ca_certificate(Certificate::from_pem(ca));
            if let Some((cert, key)) = identity {
                tls = tls.identity(Identity::from_pem(cert, key));
            }
            options = options.with_tls(tls);
        }
        let client = Client::connect(settings.endpoints.clone(), Some(options))
            .await
            .context("could not connect to any etcd endpoint")?;
        // Tonic otherwise accepts its library default independently from the
        // flattened snapshot budget. Keep a small protobuf-overhead margin
        // above the decoded 4 MiB map ceiling.
        let kv = client
            .kv_client()
            .max_decoding_message_size(MAX_ETCD_RESPONSE_BYTES);
        Ok(Self { client, kv })
    }

    pub async fn get(&mut self, key: &str) -> Result<String> {
        self.get_with_revision(key).await.map(|(value, _)| value)
    }

    pub async fn get_with_revision(&mut self, key: &str) -> Result<(String, i64)> {
        let response = self.kv.get(key, None).await?;
        let entry = response
            .kvs()
            .first()
            .with_context(|| format!("required etcd key is missing: {key}"))?;
        // For /commit, mod_revision is the immutable MVCC snapshot boundary:
        // later writers may already be staging the next generation while the
        // commit value is unchanged. Reading at the response header revision
        // would include those staged values; reading at the marker's own
        // modification revision cannot.
        let revision = entry.mod_revision();
        let value = entry.value_str()?;
        if key == "/commit" {
            crate::state::validate_commit(value)?;
        }
        let value = value.to_owned();
        Ok((value, revision))
    }

    pub async fn get_prefix(&mut self, prefix: &str) -> Result<BTreeMap<String, String>> {
        let (_, revision) = self.get_with_revision("/commit").await?;
        let mut budget = FlattenedBudget::default();
        self.get_prefix_at(prefix, revision, &mut budget).await
    }

    pub async fn get_prefix_at(
        &mut self,
        prefix: &str,
        revision: i64,
        budget: &mut FlattenedBudget,
    ) -> Result<BTreeMap<String, String>> {
        let response = self
            .kv
            .get(
                prefix,
                Some(GetOptions::new().with_prefix().with_revision(revision)),
            )
            .await?;
        let mut values = BTreeMap::new();
        for value in response.kvs() {
            let key = value.key_str()?;
            let value = value.value_str()?;
            budget.include(key, value)?;
            if values.insert(key.to_owned(), value.to_owned()).is_some() {
                bail!("etcd prefix response contains duplicate key: {key}");
            }
        }
        Ok(values)
    }

    pub async fn put(&mut self, key: &str, value: &str) -> Result<()> {
        self.kv.put(key, value, None).await?;
        Ok(())
    }

    pub async fn put_with_lease(&mut self, key: &str, value: &str, ttl: i64) -> Result<()> {
        let lease = self.client.lease_grant(ttl, None).await?;
        self.kv
            .put(key, value, Some(PutOptions::new().with_lease(lease.id())))
            .await?;
        Ok(())
    }
}
