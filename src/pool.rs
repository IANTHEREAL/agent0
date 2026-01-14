use crate::storage::TikvStore;
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::info;

pub struct TikvClientPool {
    pd_endpoints: Vec<String>,
    clients: RwLock<HashMap<String, Arc<TikvStore>>>,
}

impl TikvClientPool {
    pub fn new(pd_endpoints: Vec<String>) -> Self {
        Self {
            pd_endpoints,
            clients: RwLock::new(HashMap::new()),
        }
    }

    pub async fn get_client(&self, keyspace: Option<String>) -> Result<Arc<TikvStore>> {
        let key = keyspace.clone().unwrap_or_else(|| "default".to_string());

        {
            let clients = self.clients.read().await;
            if let Some(client) = clients.get(&key) {
                return Ok(client.clone());
            }
        }

        let mut clients = self.clients.write().await;

        if let Some(client) = clients.get(&key) {
            return Ok(client.clone());
        }

        info!("Creating new TiKV client for keyspace: {}", key);

        let actual_keyspace = if key == "default" {
            Some("DEFAULT".to_string())
        } else {
            keyspace
        };

        let result = TikvStore::new_with_keyspace(self.pd_endpoints.clone(), actual_keyspace).await;

        let store = match result {
            Ok(s) => s,
            Err(e) => {
                let err_str = format!("{:?}", e);
                if err_str.contains("keyspace does not exist") {
                    return Err(anyhow!("Tenant '{}' does not exist", key));
                }
                return Err(e);
            }
        };

        let store = Arc::new(store);
        clients.insert(key, store.clone());
        Ok(store)
    }

    #[allow(dead_code)]
    pub async fn client_count(&self) -> usize {
        self.clients.read().await.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_pool_creation() {
        let pool = TikvClientPool::new(vec!["127.0.0.1:2379".to_string()]);
        assert_eq!(pool.client_count().await, 0);
    }
}
