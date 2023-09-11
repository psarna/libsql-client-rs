use crate::client::Config;
use anyhow::Result;
use hyper::Uri;
use hyper::client::HttpConnector;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tower::Service;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;

use crate::{utils, BatchResult, ResultSet, Statement};

/// Database client. This is the main structure used to
/// communicate with the database.
pub struct Client<C = HttpConnector> {
    url: String,
    token: Option<String>,

    client: hrana_client::Client,
    client_future: hrana_client::ConnFut,
    streams_for_transactions: RwLock<HashMap<u64, Arc<hrana_client::Stream>>>,
    connector: C,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("url", &self.url)
            .field("token", &self.token)
            .finish()
    }
}

impl<C> Client<C>
where 
    C: Service<Uri> + Send + Clone + Sync + 'static,
    C::Response: hyper::client::connect::Connection + AsyncRead + AsyncWrite + Send + Unpin + 'static,
    C::Future: Send + 'static,
    C::Error: std::error::Error + Sync + Send + 'static,
{
    /// Same as `new`, but uses `connector` to create connections.
    pub async fn new_with_connector(url: impl Into<String>, token: impl Into<String>, connector: C) -> Result<Self>

    {
        let token = token.into();
        let token = if token.is_empty() { None } else { Some(token) };
        let url = url.into();

        let (client, client_future) = hrana_client::Client::with_connector(&url, token.clone(), connector.clone()).await?;

        Ok(Self {
            url,
            token,
            client,
            client_future,
            streams_for_transactions: RwLock::new(HashMap::new()),
            connector,
        })
    }

    pub async fn reconnect(&mut self) -> Result<()> {
        let (client, client_future) =
            hrana_client::Client::with_connector(&self.url, self.token.clone(), self.connector.clone()).await?;
        self.client = client;
        self.client_future = client_future;
        Ok(())
    }
}

impl Client {
    /// Creates a database client with JWT authentication.
    ///
    /// # Arguments
    /// * `url` - URL of the database endpoint
    /// * `token` - auth token
    pub async fn new(url: impl Into<String>, token: impl Into<String>) -> Result<Self> {
        let connector = HttpConnector::new();
        Self::new_with_connector(url, token, connector).await
    }

    /// Creates a database client, given a `Url`
    ///
    /// # Arguments
    /// * `url` - `Url` object of the database endpoint. This cannot be a relative URL;
    ///
    /// # Examples
    ///
    /// ```
    /// # async fn f() -> anyhow::Result<()> {
    /// # use libsql_client::hrana::Client;
    /// use url::Url;
    ///
    /// let url = Url::parse("https://localhost:8080?authToken=<access token>").unwrap();
    /// let db = Client::from_url(url).await.unwrap();
    /// # Ok(())
    /// # }
    /// ```
    pub async fn from_url<T: TryInto<url::Url>>(url: T) -> anyhow::Result<Client>
    where
        <T as TryInto<url::Url>>::Error: std::fmt::Display,
    {
        let mut url: url::Url = url
            .try_into()
            .map_err(|e| anyhow::anyhow!(format!("{e}")))?;
        // remove the auth token from the URL so that it doesn't get logged anywhere
        let token = utils::pop_query_param(&mut url, "authToken".to_string());
        let url_str = if url.scheme() == "libsql" {
            let new_url = format!("wss://{}", url.as_str().strip_prefix("libsql://").unwrap());
            url::Url::parse(&new_url).unwrap().to_string()
        } else {
            url.to_string()
        };
        if let Some(token) = token {
            Client::new(url_str, token).await
        } else {
            Client::new(url_str, "").await
        }
    }

    /// Creates a database client from a `Config` object.
    pub async fn from_config(config: Config) -> Result<Self> {
        Self::new(config.url, config.auth_token.unwrap_or_default()).await
    }

    pub async fn shutdown(self) -> Result<()> {
        self.client.shutdown().await?;
        self.client_future.await?;
        Ok(())
    }

    // Find an existing stream for given transaction id, or create a new one.
    async fn stream_for_transaction(&self, tx_id: u64) -> Result<Arc<hrana_client::Stream>> {
        // Fast path, transaction exists and has a stream.
        {
            let streams = self.streams_for_transactions.read().unwrap();
            if streams.contains_key(&tx_id) {
                tracing::trace!("Found stream for transaction {tx_id}");
                return Ok(streams.get(&tx_id).unwrap().clone()); //NOTICE: safe to unwrap, it was either found or just inserted
            }
        }
        // Pessimistic path - let's drop the mutex, create the stream and try to reinsert it.
        // Another way out of this situation is an async mutex, but I don't want to rely on Tokio or any other specific runtime
        // unless absolutely necessary.
        let stream = Arc::new(self.client.open_stream().await?);
        tracing::trace!("Created new stream");
        let mut streams = self.streams_for_transactions.write().unwrap();
        if let std::collections::hash_map::Entry::Vacant(e) = streams.entry(tx_id) {
            e.insert(stream.clone());
        }
        Ok(stream)
    }

    // Drop the stream for given transaction id.
    fn drop_stream_for_transaction(&self, tx_id: u64) {
        let mut streams = self.streams_for_transactions.write().unwrap();
        tracing::trace!("Dropping stream for transaction {tx_id}");
        streams.remove(&tx_id);
    }

    fn into_hrana(stmt: Statement) -> hrana_client::proto::Stmt {
        let mut hrana_stmt = hrana_client::proto::Stmt::new(stmt.sql, true);
        for param in stmt.args {
            hrana_stmt.bind(param);
        }
        hrana_stmt
    }
}

impl Client {
    pub async fn raw_batch(
        &self,
        stmts: impl IntoIterator<Item = impl Into<Statement>>,
    ) -> anyhow::Result<BatchResult> {
        let mut batch = hrana_client::proto::Batch::new();
        for stmt in stmts.into_iter() {
            let stmt: Statement = stmt.into();
            let mut hrana_stmt = hrana_client::proto::Stmt::new(stmt.sql, true);
            for param in stmt.args {
                hrana_stmt.bind(param);
            }
            batch.step(None, hrana_stmt);
        }

        let stream = self.client.open_stream().await?;
        stream
            .execute_batch(batch)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    pub async fn execute(&self, stmt: impl Into<Statement>) -> Result<ResultSet> {
        let stmt = Self::into_hrana(stmt.into());

        let stream = self.client.open_stream().await?;
        stream
            .execute(stmt)
            .await
            .map(ResultSet::from)
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    pub async fn execute_in_transaction(&self, tx_id: u64, stmt: Statement) -> Result<ResultSet> {
        let stmt = Self::into_hrana(stmt);
        tracing::trace!("Transaction {tx_id} executing {}", stmt.sql);
        let stream = self.stream_for_transaction(tx_id).await?;
        stream
            .execute(stmt)
            .await
            .map(ResultSet::from)
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    pub async fn commit_transaction(&self, tx_id: u64) -> Result<()> {
        tracing::trace!("Transaction {tx_id} commit");
        let stream = self.stream_for_transaction(tx_id).await?;
        self.drop_stream_for_transaction(tx_id);
        stream
            .execute(Self::into_hrana(Statement::from("COMMIT")))
            .await
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    pub async fn rollback_transaction(&self, tx_id: u64) -> Result<()> {
        tracing::trace!("Transaction {tx_id} rollback");
        let stream = self.stream_for_transaction(tx_id).await?;
        self.drop_stream_for_transaction(tx_id);
        stream
            .execute(Self::into_hrana(Statement::from("ROLLBACK")))
            .await
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("{}", e))
    }
}
