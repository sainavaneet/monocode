//! Task events, their pending deliveries, and resource leases.
//!
//! Delivery is at least once: an event keeps its stable id, so appending it
//! again returns the stored copy instead of a second event, and a delivery is
//! only final once its destination acknowledges it (as `follow_up` receipts).
use super::states::state_enum;
use super::store::{guard, ms, read_ms, TaskStore};
use super::TaskError;
use rusqlite::{params, Connection, OptionalExtension, Row};

#[derive(Clone, Debug, PartialEq)]
pub struct NewEvent {
    /// Stable across retries: the same id is the same event.
    pub id: String,
    pub task_id: String,
    pub assignment_id: Option<String>,
    pub attempt_id: Option<String>,
    pub kind: String,
    pub payload: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    /// Append order across all tasks. Events are never deleted, so it only grows.
    pub seq: i64,
    pub id: String,
    pub task_id: String,
    pub assignment_id: Option<String>,
    pub attempt_id: Option<String>,
    pub kind: String,
    pub payload: serde_json::Value,
    pub created_at: u64,
}

state_enum!(
    DeliveryState {
        Pending => "pending",
        /// Handed to the destination; waiting for its acknowledgment.
        Delivered => "delivered",
        Acknowledged => "acknowledged",
    }
);

/// An event owed to a destination such as `mono:<id>` or `attempt:<id>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingDelivery {
    pub id: String,
    pub event_id: String,
    pub destination: String,
    /// What to deliver, by reference; the payload itself stays in its record.
    pub payload_ref: String,
    pub state: DeliveryState,
    pub retries: u32,
    pub last_error: Option<String>,
    pub revision: u64,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeliveryOutcome {
    Delivered,
    Acknowledged,
    /// The attempt failed; the delivery stays pending and counts a retry.
    Failed(String),
}

/// A database row with expiry, so two processes sharing the file cannot both
/// coordinate one resource. `generation` grows each time the holder changes
/// and can fence writes from a holder that lost the lease.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lease {
    pub resource: String,
    pub holder: String,
    pub expires_at: u64,
    pub generation: u64,
}

const EVENT: &str = "seq,id,task_id,assignment_id,attempt_id,kind,payload,created_at";
const DELIVERY: &str =
    "id,event_id,destination,payload_ref,state,retries,last_error,revision,created_at,updated_at";

fn event(row: &Row<'_>) -> rusqlite::Result<Event> {
    let payload: String = row.get(6)?;
    Ok(Event {
        seq: row.get(0)?,
        id: row.get(1)?,
        task_id: row.get(2)?,
        assignment_id: row.get(3)?,
        attempt_id: row.get(4)?,
        kind: row.get(5)?,
        payload: serde_json::from_str(&payload).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, Box::new(e))
        })?,
        created_at: read_ms(row, 7)?,
    })
}

fn delivery(row: &Row<'_>) -> rusqlite::Result<PendingDelivery> {
    let state: String = row.get(4)?;
    Ok(PendingDelivery {
        id: row.get(0)?,
        event_id: row.get(1)?,
        destination: row.get(2)?,
        payload_ref: row.get(3)?,
        state: DeliveryState::parse(&state).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                4,
                rusqlite::types::Type::Text,
                format!("unknown delivery state {state}").into(),
            )
        })?,
        retries: row.get(5)?,
        last_error: row.get(6)?,
        revision: read_ms(row, 7)?,
        created_at: read_ms(row, 8)?,
        updated_at: read_ms(row, 9)?,
    })
}

fn lease(row: &Row<'_>) -> rusqlite::Result<Lease> {
    Ok(Lease {
        resource: row.get(0)?,
        holder: row.get(1)?,
        expires_at: read_ms(row, 2)?,
        generation: read_ms(row, 3)?,
    })
}

/// Appends `new` unless an event with its id exists; returns the stored event.
pub(super) fn append(conn: &Connection, new: &NewEvent, now: u64) -> Result<Event, TaskError> {
    conn.execute(
        "INSERT INTO native_task_events(id,task_id,assignment_id,attempt_id,kind,payload,created_at)
         VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(id) DO NOTHING",
        params![
            new.id,
            new.task_id,
            new.assignment_id,
            new.attempt_id,
            new.kind,
            new.payload.to_string(),
            ms(now)
        ],
    )?;
    Ok(conn.query_row(
        &format!("SELECT {EVENT} FROM native_task_events WHERE id=?1"),
        [&new.id],
        event,
    )?)
}

impl TaskStore<'_> {
    pub fn append_event(&self, new: &NewEvent, now: u64) -> Result<Event, TaskError> {
        append(self.conn, new, now)
    }

    /// Events after `seq` in order, for replay from a remembered position.
    pub fn events_after(&self, seq: i64, limit: usize) -> Result<Vec<Event>, TaskError> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {EVENT} FROM native_task_events WHERE seq>?1 ORDER BY seq LIMIT ?2"
        ))?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let events = statement.query_map(params![seq, limit], event)?;
        Ok(events.collect::<Result<_, _>>()?)
    }

    /// Records that `event_id` is owed to `destination`. Enqueueing the same
    /// delivery id again returns the existing record unchanged.
    pub fn enqueue_delivery(
        &self,
        id: &str,
        event_id: &str,
        destination: &str,
        payload_ref: &str,
        now: u64,
    ) -> Result<PendingDelivery, TaskError> {
        self.conn.execute(
            "INSERT INTO native_task_deliveries(id,event_id,destination,payload_ref,state,retries,revision,created_at,updated_at)
             VALUES(?1,?2,?3,?4,'pending',0,1,?5,?5) ON CONFLICT(id) DO NOTHING",
            params![id, event_id, destination, payload_ref, ms(now)],
        )?;
        self.delivery(id)?
            .ok_or_else(|| TaskError::NotFound(format!("delivery:{id}")))
    }

    pub fn delivery(&self, id: &str) -> Result<Option<PendingDelivery>, TaskError> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {DELIVERY} FROM native_task_deliveries WHERE id=?1"),
                [id],
                delivery,
            )
            .optional()?)
    }

    /// Deliveries still owed to `destination`, oldest first.
    pub fn pending_deliveries(&self, destination: &str) -> Result<Vec<PendingDelivery>, TaskError> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {DELIVERY} FROM native_task_deliveries
             WHERE destination=?1 AND state='pending' ORDER BY created_at,rowid"
        ))?;
        let rows = statement.query_map([destination], delivery)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Records one delivery attempt's outcome; returns the new revision.
    pub fn record_delivery(
        &self,
        id: &str,
        expected_revision: u64,
        outcome: DeliveryOutcome,
        now: u64,
    ) -> Result<u64, TaskError> {
        let tx = self.begin()?;
        let key = super::store::Key::new("native_task_deliveries", "id=?", vec![id], "delivery");
        let state = guard(&tx, &key, "state", expected_revision)?;
        use DeliveryState::{Acknowledged, Delivered, Pending};
        let from = DeliveryState::parse(&state);
        let (to, error) = match (&outcome, from) {
            (DeliveryOutcome::Delivered, Some(Pending)) => (Delivered, None),
            (DeliveryOutcome::Acknowledged, Some(Pending | Delivered)) => (Acknowledged, None),
            (DeliveryOutcome::Failed(error), Some(Pending | Delivered)) => {
                (Pending, Some(error.as_str()))
            }
            _ => {
                return Err(TaskError::InvalidState {
                    subject: key.label,
                    state,
                })
            }
        };
        let retried = i64::from(error.is_some());
        let revision = key.bump(
            &tx,
            expected_revision,
            "state=?,retries=retries+?,last_error=COALESCE(?,last_error)",
            params![to.as_str(), retried, error].to_vec(),
            now,
        )?;
        tx.commit()?;
        Ok(revision)
    }

    /// Takes or renews `resource` for `holder` until `now + ttl_ms`. Succeeds
    /// when the lease is free, expired, or already held by `holder`.
    pub fn acquire_lease(
        &self,
        resource: &str,
        holder: &str,
        ttl_ms: u64,
        now: u64,
    ) -> Result<Lease, TaskError> {
        if holder.is_empty() {
            return Err(TaskError::Invalid("A lease holder needs a name.".into()));
        }
        let taken = self.conn.execute(
            "INSERT INTO native_task_leases(resource,holder,expires_at,generation) VALUES(?1,?2,?3,1)
             ON CONFLICT(resource) DO UPDATE SET holder=excluded.holder, expires_at=excluded.expires_at,
               generation=CASE WHEN holder=excluded.holder THEN generation ELSE generation+1 END
             WHERE holder=excluded.holder OR expires_at<=?4",
            params![resource, holder, ms(now.saturating_add(ttl_ms)), ms(now)],
        )?;
        let current = self
            .lease(resource)?
            .ok_or_else(|| TaskError::NotFound(format!("lease:{resource}")))?;
        if taken == 0 {
            return Err(TaskError::LeaseHeld {
                resource: current.resource,
                holder: current.holder,
                expires_at: current.expires_at,
            });
        }
        Ok(current)
    }

    /// Releases `resource` if `holder` holds it; returns whether it did. The
    /// row stays, without a holder, so the next holder gets a new generation.
    pub fn release_lease(&self, resource: &str, holder: &str) -> Result<bool, TaskError> {
        Ok(self.conn.execute(
            "UPDATE native_task_leases SET holder='',expires_at=0 WHERE resource=?1 AND holder=?2",
            [resource, holder],
        )? == 1)
    }

    pub fn lease(&self, resource: &str) -> Result<Option<Lease>, TaskError> {
        Ok(self
            .conn
            .query_row(
                "SELECT resource,holder,expires_at,generation FROM native_task_leases WHERE resource=?1 AND holder<>''",
                [resource],
                lease,
            )
            .optional()?)
    }
}
