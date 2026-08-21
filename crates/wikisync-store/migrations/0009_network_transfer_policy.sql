CREATE TABLE network_transfer_policy (
    singleton INTEGER PRIMARY KEY NOT NULL CHECK (singleton = 1),
    max_concurrent_requests INTEGER NOT NULL
        CHECK (max_concurrent_requests BETWEEN 1 AND 256),
    max_download_bytes_per_second INTEGER
        CHECK (
            max_download_bytes_per_second IS NULL
            OR max_download_bytes_per_second > 0
        ),
    avoid_metered_networks INTEGER NOT NULL
        CHECK (avoid_metered_networks IN (0, 1))
) STRICT;

INSERT INTO network_transfer_policy (
    singleton,
    max_concurrent_requests,
    max_download_bytes_per_second,
    avoid_metered_networks
) VALUES (1, 4, NULL, 0);
