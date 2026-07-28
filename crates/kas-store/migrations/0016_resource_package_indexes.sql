CREATE INDEX resources_by_package
ON resources(
    json_extract(metadata,'$."[kas]".package'),
    path
);

CREATE INDEX resources_by_status_package
ON resources(
    json_extract(status,'$.metadata."[kas]".package'),
    path
);
