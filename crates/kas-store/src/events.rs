//! Transactional Resource event journal and dependency invalidation.

use super::*;

pub(super) fn event_from_row(row: &Row<'_>) -> database::Result<Event> {
    let event_type: String = row.get(1)?;
    Ok(Event {
        sequence: row.get(0)?,
        event_type: match event_type.as_str() {
            "created" => EventType::Created,
            "updated" => EventType::Updated,
            "deleted" => EventType::Deleted,
            other => {
                return Err(from_sql(
                    1,
                    std::io::Error::other(format!("invalid event type {other}")),
                ));
            }
        },
        resource_path: row.get(2)?,
        revision: row.get(3)?,
        value: json_from_row(row, 4)?,
        created_at: time_from_row(row, 5)?,
    })
}

pub(super) fn append_event(
    tx: &Transaction,
    event_type: EventType,
    resource: &Resource,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    append_event_row(tx, event_type, resource, now)?;
    touch_dependent_links(tx, &resource.path, now)
}

/// Record a Driver's observation without invalidating Links that reference the
/// Resource. Link validity is derived from desired metadata and spec; status is
/// an observation of that desired document and must not feed back into relation
/// reconciliation.
pub(super) fn append_status_event(
    tx: &Transaction,
    event_type: EventType,
    resource: &Resource,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    append_event_row(tx, event_type, resource, now)
}

fn append_event_row(
    tx: &Transaction,
    event_type: EventType,
    resource: &Resource,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    tx.execute(
        "INSERT INTO events(event_type,resource_path,revision,value_json,created_at)
         VALUES (?,?,?,?,?)",
        db_params![
            event_type_name(event_type),
            resource.path,
            resource.revision,
            serde_json::to_value(resource)?,
            now
        ],
    )?;
    Ok(())
}

pub(super) fn append_deleted_event(
    tx: &Transaction,
    resource: &Resource,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    append_event_row(tx, EventType::Deleted, resource, now)?;
    touch_dependent_links(tx, &resource.path, now)
}

/// A Link's validity depends on its Relation and both endpoints. When one of
/// those Resources changes, advance only the affected Link revisions. This
/// makes the observation queue deliver those Links without a wildcard watch
/// or a full Resource scan.
fn touch_dependent_links(
    tx: &Transaction,
    dependency_path: &str,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    let sql = format!(
        "{RESOURCE_SELECT}
         WHERE json_extract(metadata,'$.manifest')=?
           AND path<>?
           AND (json_extract(spec,'$.source')=?
             OR json_extract(spec,'$.target')=?
             OR json_extract(spec,'$.relation')=?)
         ORDER BY path"
    );
    let dependent_links = {
        let mut statement = tx.prepare(&sql)?;
        let rows = statement.query_map(
            db_params![
                LINK_MANIFEST,
                dependency_path,
                dependency_path,
                dependency_path,
                dependency_path
            ],
            resource_from_row,
        )?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    for mut link in dependent_links {
        if link.metadata.state == STATE_DELETED {
            continue;
        }
        link.metadata.kas.revision += 1;
        link.metadata.kas.updated_at = now;
        save_resource_in(tx, &link)?;
        enqueue_if_drifted(tx, &link, "link_dependency_changed", now)?;
        let link = resource_in(tx, &link.path)?;
        append_event_row(tx, EventType::Updated, &link, now)?;
    }
    Ok(())
}

pub(super) fn current_event_sequence_in(tx: &Transaction) -> Result<u64, StoreError> {
    tx.query_row(
        "SELECT COALESCE(MAX(sequence),0) FROM events",
        db_params![],
        |row| row.get(0),
    )
    .map_err(StoreError::from)
}

pub(super) fn event_paths_since(tx: &Transaction, cursor: u64) -> Result<Vec<String>, StoreError> {
    let mut statement = tx.prepare(
        "SELECT DISTINCT resource_path FROM events
         WHERE sequence>? ORDER BY resource_path",
    )?;
    statement
        .query_map(db_params![cursor], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(StoreError::from)
}

fn event_type_name(event_type: EventType) -> &'static str {
    match event_type {
        EventType::Created => "created",
        EventType::Updated => "updated",
        EventType::Deleted => "deleted",
    }
}
