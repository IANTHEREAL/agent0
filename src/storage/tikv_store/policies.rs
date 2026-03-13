use super::*;
use crate::storage::backpressure::tikv_op;

impl TikvStore {
    /// Store a new RLS policy in TiKV. Assigns an OID if not set.
    pub async fn create_policy(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        mut policy: RlsPolicy,
    ) -> Result<RlsPolicy> {
        if policy.oid == 0 {
            policy.oid = self.next_policy_oid(txn, db_id).await?;
        }

        let key = self.key(&encode_policy_key_v2(db_id, policy.table_id, &policy.name));
        if tikv_op!(txn.get(key.clone()).await)?.is_some() {
            return Err(anyhow!(
                "policy \"{}\" for table already exists",
                policy.name
            ));
        }
        let data = bincode::serialize(&policy).context("Failed to serialize RLS policy")?;
        txn_put(txn, key, data).await?;
        Ok(policy)
    }

    /// List all RLS policies for a specific table.
    pub async fn list_policies_for_table(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_id: u64,
    ) -> Result<Vec<RlsPolicy>> {
        let prefix = encode_policy_table_prefix_v2(db_id, table_id);
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix..end).into();
        let pairs = tikv_op!(txn.scan(range, SCAN_LIMIT).await)?;

        let mut policies = Vec::new();
        for pair in pairs {
            let policy: RlsPolicy =
                bincode::deserialize(pair.value()).context("Failed to deserialize RLS policy")?;
            policies.push(policy);
        }
        Ok(policies)
    }

    /// List all RLS policies in a database (across all tables).
    pub async fn list_all_policies(
        &self,
        txn: &mut Transaction,
        db_id: u64,
    ) -> Result<Vec<RlsPolicy>> {
        let prefix = encode_policy_prefix_v2(db_id);
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix..end).into();
        let pairs = tikv_op!(txn.scan(range, SCAN_LIMIT).await)?;

        let mut policies = Vec::new();
        for pair in pairs {
            let policy: RlsPolicy =
                bincode::deserialize(pair.value()).context("Failed to deserialize RLS policy")?;
            policies.push(policy);
        }
        Ok(policies)
    }

    /// Drop a specific policy by name. Returns true if it existed.
    pub async fn drop_policy(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_id: u64,
        policy_name: &str,
    ) -> Result<bool> {
        let key = self.key(&encode_policy_key_v2(db_id, table_id, policy_name));
        if tikv_op!(txn.get(key.clone()).await)?.is_some() {
            txn_delete(txn, key).await?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Drop all policies for a given table (used when dropping a table).
    pub async fn drop_policies_for_table(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_id: u64,
    ) -> Result<u64> {
        let prefix = encode_policy_table_prefix_v2(db_id, table_id);
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix..end).into();
        let pairs: Vec<_> = tikv_op!(txn.scan(range, SCAN_LIMIT).await)?.collect();

        let count = pairs.len() as u64;
        for pair in pairs {
            txn_delete(txn, pair.key().clone().into()).await?;
        }
        Ok(count)
    }
}
