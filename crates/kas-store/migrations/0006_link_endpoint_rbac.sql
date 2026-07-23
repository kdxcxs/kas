UPDATE roles
SET rules_json = '[{"resources":["manifests","resources/*","drivers","runs","links"],"verbs":["get","list","create","update","patch","delete","watch","link"]}]',
    updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
WHERE id = '/roles/system/editor'
  AND managed_by = 'system';
