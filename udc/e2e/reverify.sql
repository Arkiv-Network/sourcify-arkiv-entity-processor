-- A re-verification for the UDC sync's end-to-end test (docs/udc-proposal.md, "Contract updates"): Sourcify verifies
-- 0x36F3FD68E7325a35EB768F1AedaAe9EA0689d723 (fixture match 468227, partial) again, now as a perfect match with a
-- new compilation, under the newer match ID 60000001. Apply to a disposable copy of tests/fixture (never the mirror).
-- The compilation is copied under a new id; its compiler name differs, so its content hash (and entity) differ.
INSERT INTO compiled_contracts (id, compiler, version, language, name, fully_qualified_name, compiler_settings, compilation_artifacts,
  creation_code_hash, creation_code_artifacts, runtime_code_hash, runtime_code_artifacts, additional_input)
SELECT '00000000-0000-4000-8000-000000000001', 'solc-reverify-test', version, language, name, fully_qualified_name, compiler_settings,
  compilation_artifacts, creation_code_hash, creation_code_artifacts, runtime_code_hash, runtime_code_artifacts, additional_input
FROM compiled_contracts WHERE id = 'd71dc513-984d-4d41-99ca-f678f34fedad';
INSERT INTO compiled_contracts_metadata (compilation_id, metadata)
SELECT '00000000-0000-4000-8000-000000000001', metadata FROM compiled_contracts_metadata WHERE compilation_id = 'd71dc513-984d-4d41-99ca-f678f34fedad';
INSERT INTO compiled_contracts_sources (compilation_id, source_hash, path)
SELECT '00000000-0000-4000-8000-000000000001', source_hash, path FROM compiled_contracts_sources WHERE compilation_id = 'd71dc513-984d-4d41-99ca-f678f34fedad';
INSERT INTO compiled_contracts_signatures (compilation_id, signature_hash_32, signature_type)
SELECT '00000000-0000-4000-8000-000000000001', signature_hash_32, signature_type FROM compiled_contracts_signatures WHERE compilation_id = 'd71dc513-984d-4d41-99ca-f678f34fedad';
INSERT INTO verified_contracts (id, deployment_id, compilation_id, creation_match, creation_values, creation_transformations,
  creation_metadata_match, runtime_match, runtime_values, runtime_transformations, runtime_metadata_match)
SELECT 60000001, deployment_id, '00000000-0000-4000-8000-000000000001', true, creation_values, creation_transformations, true,
  true, runtime_values, runtime_transformations, true
FROM verified_contracts WHERE id = 54953643;
INSERT INTO sourcify_matches (id, verified_contract_id, creation_match, runtime_match, created_at, updated_at, chain_id)
VALUES (60000001, 60000001, 'perfect', 'perfect', '2026-10-28 13:40:00+00', '2026-10-28 13:40:00+00', 1);
