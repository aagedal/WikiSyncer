ALTER TABLE sync_runs ADD COLUMN configuration_hash TEXT
    CHECK (
        configuration_hash IS NULL
        OR (
            length(configuration_hash) = 67
            AND configuration_hash GLOB 'b3:[0-9a-f]*'
        )
    );
