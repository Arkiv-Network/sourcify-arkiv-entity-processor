-- The 11 Sourcify tables prepare.ts reads, with the column types, primary keys and unique (pseudo) keys of the
-- Sourcify database schema (services/database migrations of argotorg/sourcify). Unqualified: the tests create them in
-- a fresh schema of their own (tests/fixture/postgres.ts).
--
-- Left out: triggers, CHECK constraints (they need Sourcify's validation functions) and foreign keys. Without foreign
-- keys the fixture keeps rows whose references are missing (tests/fixture/fixture.sql), which a Sourcify database
-- does not allow; they exercise the fail-closed handling (--skip-missing-references).

CREATE TYPE signature_type_enum AS ENUM ('function', 'event', 'error');

CREATE TABLE code (
    code_hash bytea NOT NULL PRIMARY KEY,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    created_by character varying DEFAULT CURRENT_USER NOT NULL,
    updated_by character varying DEFAULT CURRENT_USER NOT NULL,
    code_hash_keccak bytea NOT NULL,
    code bytea
);

CREATE TABLE sources (
    source_hash bytea NOT NULL PRIMARY KEY,
    source_hash_keccak bytea NOT NULL,
    content character varying NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    created_by character varying DEFAULT CURRENT_USER NOT NULL,
    updated_by character varying DEFAULT CURRENT_USER NOT NULL
);

CREATE TABLE signatures (
    signature_hash_32 bytea NOT NULL PRIMARY KEY,
    signature_hash_4 bytea GENERATED ALWAYS AS (SUBSTRING(signature_hash_32 FROM 1 FOR 4)) STORED,
    signature character varying NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);

CREATE TABLE compiled_contracts (
    id uuid DEFAULT gen_random_uuid() NOT NULL PRIMARY KEY,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    created_by character varying DEFAULT CURRENT_USER NOT NULL,
    updated_by character varying DEFAULT CURRENT_USER NOT NULL,
    compiler character varying NOT NULL,
    version character varying NOT NULL,
    language character varying NOT NULL,
    name character varying NOT NULL,
    fully_qualified_name character varying NOT NULL,
    compiler_settings jsonb NOT NULL,
    compilation_artifacts jsonb NOT NULL,
    creation_code_hash bytea NOT NULL,
    creation_code_artifacts jsonb NOT NULL,
    runtime_code_hash bytea NOT NULL,
    runtime_code_artifacts jsonb NOT NULL,
    additional_input jsonb,
    CONSTRAINT compiled_contracts_pseudo_pkey UNIQUE (compiler, version, language, creation_code_hash, runtime_code_hash)
);

CREATE TABLE compiled_contracts_metadata (
    compilation_id uuid NOT NULL PRIMARY KEY,
    metadata json NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);

CREATE TABLE compiled_contracts_signatures (
    id uuid DEFAULT gen_random_uuid() NOT NULL PRIMARY KEY,
    compilation_id uuid NOT NULL,
    signature_hash_32 bytea NOT NULL,
    signature_type signature_type_enum NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT compiled_contracts_signatures_pseudo_pkey UNIQUE (compilation_id, signature_hash_32, signature_type)
);

CREATE TABLE compiled_contracts_sources (
    id uuid DEFAULT gen_random_uuid() NOT NULL PRIMARY KEY,
    compilation_id uuid NOT NULL,
    source_hash bytea NOT NULL,
    path character varying NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    CONSTRAINT compiled_contracts_sources_pseudo_pkey UNIQUE (compilation_id, path)
);

CREATE TABLE contracts (
    id uuid DEFAULT gen_random_uuid() NOT NULL PRIMARY KEY,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    created_by character varying DEFAULT CURRENT_USER NOT NULL,
    updated_by character varying DEFAULT CURRENT_USER NOT NULL,
    creation_code_hash bytea,
    runtime_code_hash bytea NOT NULL,
    CONSTRAINT contracts_pseudo_pkey UNIQUE (creation_code_hash, runtime_code_hash)
);

CREATE TABLE contract_deployments (
    id uuid DEFAULT gen_random_uuid() NOT NULL PRIMARY KEY,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    created_by character varying DEFAULT CURRENT_USER NOT NULL,
    updated_by character varying DEFAULT CURRENT_USER NOT NULL,
    chain_id bigint NOT NULL,
    address bytea NOT NULL,
    transaction_hash bytea,
    block_number numeric,
    transaction_index numeric,
    deployer bytea,
    contract_id uuid NOT NULL,
    CONSTRAINT contract_deployments_pseudo_pkey UNIQUE (chain_id, address, transaction_hash, contract_id)
);

CREATE TABLE verified_contracts (
    id bigint NOT NULL PRIMARY KEY,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    created_by character varying DEFAULT CURRENT_USER NOT NULL,
    updated_by character varying DEFAULT CURRENT_USER NOT NULL,
    deployment_id uuid NOT NULL,
    compilation_id uuid NOT NULL,
    creation_match boolean NOT NULL,
    creation_values jsonb,
    creation_transformations jsonb,
    creation_metadata_match boolean,
    runtime_match boolean NOT NULL,
    runtime_values jsonb,
    runtime_transformations jsonb,
    runtime_metadata_match boolean,
    CONSTRAINT verified_contracts_pseudo_pkey UNIQUE (compilation_id, deployment_id)
);

CREATE TABLE sourcify_matches (
    id bigint NOT NULL PRIMARY KEY,
    verified_contract_id bigint NOT NULL,
    creation_match character varying,
    runtime_match character varying,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    updated_at timestamp with time zone DEFAULT now() NOT NULL,
    chain_id bigint NOT NULL,
    CONSTRAINT sourcify_matches_pseudo_pkey UNIQUE (verified_contract_id)
);
