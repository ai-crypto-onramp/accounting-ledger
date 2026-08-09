use crate::store::{now_iso, AuditEvent, Store};

pub fn emit_audit_event(store: &Store, event: &AuditEvent) {
    store.record_audit_event(event.clone());
}

pub fn build_event(posting_id: &str, entry_ids: &[String], hash_head: &str) -> AuditEvent {
    AuditEvent {
        event_id: uuid::Uuid::now_v7().to_string(),
        posting_id: posting_id.to_string(),
        entry_ids: entry_ids.to_vec(),
        hash_head: hash_head.to_string(),
        created_at: now_iso(),
    }
}

#[derive(Debug, Clone)]
pub struct AuditSink {
    pub kafka: Option<std::sync::Arc<ArcKafkaAuditSink>>,
}

pub struct ArcKafkaAuditSink {
    pub producer: rdkafka::producer::FutureProducer,
}

impl std::fmt::Debug for ArcKafkaAuditSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArcKafkaAuditSink").finish()
    }
}

impl Clone for ArcKafkaAuditSink {
    fn clone(&self) -> Self {
        Self {
            producer: self.producer.clone(),
        }
    }
}

impl AuditSink {
    pub fn new(kafka: Option<ArcKafkaAuditSink>) -> Self {
        Self {
            kafka: kafka.map(std::sync::Arc::new),
        }
    }

    pub fn from_env() -> anyhow::Result<Self> {
        let brokers = std::env::var("KAFKA_BROKERS")
            .ok()
            .filter(|v| !v.is_empty());
        let dev_mode = std::env::var("DEV_MODE")
            .ok()
            .is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"));
        match brokers {
            Some(b) => {
                let producer: rdkafka::producer::FutureProducer = rdkafka::ClientConfig::new()
                    .set("bootstrap.servers", b)
                    .set("message.timeout.ms", "5000")
                    .create()?;
                Ok(Self {
                    kafka: Some(std::sync::Arc::new(ArcKafkaAuditSink { producer })),
                })
            }
            None => {
                if dev_mode {
                    eprintln!("[audit] KAFKA_BROKERS unset and DEV_MODE=1; audit records will be logged to stderr only");
                    Ok(Self { kafka: None })
                } else {
                    Err(anyhow::anyhow!(
                        "KAFKA_BROKERS unset and DEV_MODE not set; cannot start audit producer"
                    ))
                }
            }
        }
    }

    pub fn emit(&self, _store: &Store, event: &AuditEvent) {
        if let Some(k) = &self.kafka {
            let payload = serde_json::to_value(event).unwrap_or(serde_json::Value::Null);
            let payload_bytes = serde_json::to_vec(&payload).unwrap_or_default();
            let payload_hash = format!(
                "sha256:{}",
                hex::encode(<sha2::Sha256 as sha2::Digest>::digest(&payload_bytes))
            );
            let envelope = serde_json::json!({
                "schema_version": "1",
                "id": event.event_id,
                "ts": event.created_at,
                "source_service": "ledger-accounting",
                "actor_id": "ledger-accounting",
                "action": "ledger.posting",
                "target_type": "posting",
                "target_id": event.posting_id,
                "payload_hash": payload_hash,
                "payload": payload,
            });
            let key = event.event_id.clone();
            let producer = k.producer.clone();
            let envelope_bytes = serde_json::to_vec(&envelope).unwrap_or_default();
            tokio::spawn(async move {
                use rdkafka::producer::FutureRecord;
                use std::time::Duration;
                let _ = producer
                    .send(
                        FutureRecord::to("audit.v1")
                            .key(&key)
                            .payload(&envelope_bytes),
                        Duration::from_secs(5),
                    )
                    .await;
            });
        } else if std::env::var("DEV_MODE")
            .ok()
            .is_some_and(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
        {
            eprintln!(
                "[audit] would post event {} for posting {} to audit.v1",
                event.event_id, event.posting_id
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_event_populates_fields() {
        let ids = vec!["e1".to_string(), "e2".to_string()];
        let ev = build_event("p1", &ids, "head123");
        assert_eq!(ev.posting_id, "p1");
        assert_eq!(ev.entry_ids, ids);
        assert_eq!(ev.hash_head, "head123");
        assert!(!ev.event_id.is_empty());
        assert!(!ev.created_at.is_empty());
    }

    #[test]
    fn emit_audit_event_records_into_store() {
        let store = Store::new();
        let ev = build_event("p2", &["e1".to_string()], "h");
        emit_audit_event(&store, &ev);
        assert_eq!(store.audit_events().len(), 1);
        assert_eq!(store.audit_events()[0].posting_id, "p2");
    }

    #[test]
    fn audit_sink_emit_no_kafka_does_not_panic() {
        let store = Store::new();
        let sink = AuditSink::new(None);
        let ev = build_event("p3", &["e1".to_string()], "h");
        sink.emit(&store, &ev);
    }
}
