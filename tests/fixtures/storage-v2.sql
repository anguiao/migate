CREATE TABLE identity (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    bridge_id TEXT NOT NULL CHECK (length(bridge_id) = 32),
    light_id TEXT NOT NULL CHECK (length(light_id) = 32 AND light_id <> bridge_id)
) STRICT;

CREATE TABLE blobs (
    key INTEGER PRIMARY KEY CHECK (key BETWEEN 0 AND 65535),
    value BLOB NOT NULL
) STRICT;

CREATE TABLE xiaomi_auth (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    uid TEXT NOT NULL,
    region TEXT NOT NULL,
    oauth_client_uuid TEXT NOT NULL,
    redirect_uri TEXT NOT NULL,
    access_token TEXT NOT NULL,
    refresh_token TEXT NOT NULL,
    expires_at INTEGER NOT NULL,
    refresh_at INTEGER NOT NULL,
    virtual_did TEXT NOT NULL,
    private_key_pem TEXT NOT NULL,
    certificate_pem TEXT NOT NULL
) STRICT;

CREATE TABLE device_cache (
    uid TEXT NOT NULL,
    source TEXT NOT NULL,
    device_id TEXT NOT NULL,
    home_id TEXT NOT NULL,
    group_id TEXT NOT NULL,
    model TEXT NOT NULL,
    instance_type TEXT NOT NULL,
    category TEXT NOT NULL,
    capabilities TEXT NOT NULL,
    siid INTEGER NOT NULL,
    piid INTEGER NOT NULL,
    PRIMARY KEY (uid, source, device_id)
) STRICT;

CREATE TABLE device_identity (
    source TEXT NOT NULL,
    device_id TEXT NOT NULL,
    unique_id TEXT NOT NULL UNIQUE,
    endpoint INTEGER NOT NULL,
    PRIMARY KEY (source, device_id)
) STRICT;

PRAGMA user_version = 2;
