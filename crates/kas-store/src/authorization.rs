//! Authorization invariants enforced transactionally by the Store.

use super::*;

/// RoleBinding Links are part of the authorization boundary, so the Store
/// validates and activates them synchronously instead of waiting for the
/// asynchronous relationship Driver. Authentication can therefore require an
/// explicitly available status without creating a bootstrap dependency on
/// that Driver.
pub(super) fn activate_role_binding_in(tx: &Transaction, path: &str) -> Result<(), StoreError> {
    let mut link = resource_in(tx, path)?;
    if link.manifest != LINK_MANIFEST || link.metadata.state == STATE_DELETED {
        return Ok(());
    }
    let spec: LinkSpec = decode(&link.spec, "Link spec")?;
    if spec.relation != ROLE_BINDING_RELATION {
        return Ok(());
    }

    let relation = resource_read_in(tx, &spec.relation)?;
    require_manifest(&relation, RELATION_MANIFEST)?;
    let relation_spec: RelationSpec = decode(&relation.spec, "RoleBinding Relation spec")?;
    if relation_spec.role != Some(RelationRole::RoleBinding) {
        return Err(StoreError::Invalid(format!(
            "Relation {} is not a RoleBinding Relation",
            relation.path
        )));
    }

    let source = resource_read_in(tx, &spec.source)?;
    let target = resource_read_in(tx, &spec.target)?;
    if source.metadata.state == STATE_DELETED || target.metadata.state == STATE_DELETED {
        return Err(StoreError::Invalid(
            "RoleBinding endpoints must not be deleted".into(),
        ));
    }
    if !relation_spec
        .sources
        .iter()
        .any(|selector| selector.matches(&source))
    {
        return Err(StoreError::Invalid(format!(
            "Resource {} is not an accepted RoleBinding subject",
            source.path
        )));
    }
    if !relation_spec
        .targets
        .iter()
        .any(|selector| selector.matches(&target))
    {
        return Err(StoreError::Invalid(format!(
            "Resource {} is not an accepted RoleBinding role",
            target.path
        )));
    }
    validate_json_schema(
        "RoleBinding metadata",
        &relation_spec.metadata_schema,
        &spec.metadata,
    )?;

    let observed = link.status.metadata.kas.observed.clone();
    let mut metadata = link.status_metadata(STATE_AVAILABLE);
    metadata.kas.observed = observed;
    link.status = ResourceStatus {
        metadata,
        spec: link.spec.clone(),
    };
    save_resource_in(tx, &link)
}
