use super::*;

agentkit_contracts::impl_json_via_serde!(ProviderContinuationRecord);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderContinuationRecord {
    pub run_id: String,
    pub anchor_message_id: String,
    #[serde(deserialize_with = "agentkit_contracts::deserialize_preserving")]
    pub state: Value,
}

impl SqliteStore {
    pub async fn bind_run(
        &self,
        run_id: &str,
        chat_id: &str,
        scope: Value,
        proof: LeaseFence,
        now: i64,
    ) -> Result<()> {
        let (run_id, chat_id) = (run_id.to_owned(), chat_id.to_owned());
        self.call(move |db| {
            super::continuation_validation::scope(&scope)?;
            if scope["chatId"] != chat_id {
                return Err(StoreError::InvalidInput(
                    "provider_continuation_scope_mismatch".into(),
                ));
            }
            let transaction = db.transaction()?;
            crate::tasks::fence(&transaction, &proof, now)?;
            if proof.task_id != run_id {
                return Err(StoreError::InvalidInput(
                    "continuation run does not match lease task".into(),
                ));
            }
            let task: TaskRecord = required(&transaction, "tasks", "id", &run_id)?;
            if task.scope_id != chat_id {
                return Err(StoreError::InvalidInput(
                    "continuation chat does not match task scope".into(),
                ));
            }
            let existing: Option<(String, String)> = transaction
                .query_row(
                    "SELECT chat_id,scope FROM run_bindings WHERE run_id=?",
                    [&run_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if let Some((bound_chat, bound_scope)) = existing {
                if bound_chat != chat_id
                    || !super::continuation_validation::scopes_equal(
                        &agentkit_contracts::parse_javascript_json(&bound_scope)?,
                        &scope,
                    )
                {
                    return Err(StoreError::InvalidInput(
                        "provider continuation scope mismatch".into(),
                    ));
                }
            } else {
                transaction.execute(
                    "INSERT INTO run_bindings(run_id,chat_id,scope) VALUES(?,?,?)",
                    params![run_id, chat_id, json(&transaction, &scope)?],
                )?;
            }
            transaction.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn get_run_scope(&self, run_id: &str) -> Result<Option<Value>> {
        let run_id = run_id.to_owned();
        self.call(move |db| {
            let scope: Option<String> = db
                .query_row(
                    "SELECT scope FROM run_bindings WHERE run_id=?",
                    [run_id],
                    |r| r.get(0),
                )
                .optional()?;
            scope
                .map(|s| {
                    let value = agentkit_contracts::parse_javascript_json(&s)?;
                    super::continuation_validation::scope(&value)?;
                    Ok(value)
                })
                .transpose()
        })
        .await
    }

    pub async fn put_continuation(
        &self,
        record: ProviderContinuationRecord,
        proof: LeaseFence,
        now: i64,
    ) -> Result<ProviderContinuationRecord> {
        self.call(move |db| {
            let transaction = db.transaction()?;
            crate::tasks::fence(&transaction, &proof, now)?;
            if proof.task_id != record.run_id {
                return Err(StoreError::InvalidInput(
                    "continuation run does not match lease task".into(),
                ));
            }
            super::continuation_validation::stored(&transaction, &record)?;
            transaction.execute(
                "INSERT INTO continuations VALUES(?,?,?) ON CONFLICT(run_id) \
                 DO UPDATE SET anchor_id=excluded.anchor_id,record=excluded.record",
                params![
                    record.run_id,
                    record.anchor_message_id,
                    json(&transaction, &record)?
                ],
            )?;
            transaction.execute(
                "UPDATE run_bindings SET state_required=1 WHERE run_id=?",
                [&record.run_id],
            )?;
            transaction.commit()?;
            Ok(record)
        })
        .await
    }

    pub async fn get_continuation(
        &self,
        run_id: &str,
    ) -> Result<Option<ProviderContinuationRecord>> {
        let run_id = run_id.to_owned();
        self.call(move |db| {
            let record: Option<ProviderContinuationRecord> =
                get(db, "continuations", "run_id", &run_id)?;
            let saved: bool = db.query_row(
                "SELECT EXISTS(SELECT 1 FROM run_bindings WHERE run_id=? AND state_required=1)",
                [&run_id],
                |r| r.get(0),
            )?;
            if record.is_none() && saved {
                return Err(StoreError::InvalidInput(
                    "provider_continuation_state_missing".into(),
                ));
            }
            if let Some(record) = &record {
                super::continuation_validation::stored(db, record)?;
            }
            Ok(record)
        })
        .await
    }

    pub async fn continuations_by_anchors(
        &self,
        chat_id: &str,
        anchor_ids: Vec<String>,
    ) -> Result<Vec<ProviderContinuationRecord>> {
        let chat_id = chat_id.to_owned();
        self.call(move |db| {
            if anchor_ids.len() > 1024
                || chat_id.is_empty()
                || anchor_ids.iter().any(String::is_empty)
            {
                return Err(StoreError::InvalidInput(
                    "at most 1024 anchors permitted".into(),
                ));
            }
            let mut found = Vec::new();
            for record in all::<ProviderContinuationRecord>(db, "continuations")? {
                let anchor: MessageRecord =
                    required(db, "messages", "id", &record.anchor_message_id)?;
                if anchor.chat_id == chat_id && anchor_ids.contains(&record.anchor_message_id) {
                    super::continuation_validation::stored(db, &record)?;
                    found.push(record);
                }
            }
            found.sort_by_key(|r| {
                (
                    anchor_ids
                        .iter()
                        .position(|id| *id == r.anchor_message_id)
                        .unwrap_or(usize::MAX),
                    r.run_id.clone(),
                )
            });
            Ok(found)
        })
        .await
    }
}
