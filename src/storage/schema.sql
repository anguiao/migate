CREATE TABLE identity (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    bridge_id TEXT NOT NULL CHECK (length(bridge_id) = 32)
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
    uid TEXT NOT NULL, source TEXT NOT NULL, device_id TEXT NOT NULL,
    home_id TEXT NOT NULL, group_id TEXT NOT NULL, model TEXT NOT NULL, instance_type TEXT NOT NULL,
    category TEXT NOT NULL, capabilities TEXT NOT NULL, functions TEXT NOT NULL,
    refresh_siid INTEGER, refresh_piid INTEGER,
    PRIMARY KEY (uid, home_id, source, device_id)
) STRICT;
CREATE TABLE device_identity (
    source TEXT NOT NULL, device_id TEXT NOT NULL, unique_id TEXT NOT NULL UNIQUE,
    endpoint INTEGER NOT NULL CHECK (endpoint BETWEEN 2 AND 65534), label TEXT,
    published INTEGER NOT NULL CHECK (published IN (0,1)), PRIMARY KEY (source, device_id)
) STRICT;
CREATE TABLE device_function (
    source TEXT NOT NULL, device_id TEXT NOT NULL, function_id TEXT NOT NULL,
    endpoint INTEGER NOT NULL CHECK (endpoint BETWEEN 2 AND 65534), label TEXT NOT NULL,
    category TEXT NOT NULL, capabilities TEXT NOT NULL, PRIMARY KEY (source,device_id,function_id)
) STRICT;
CREATE TABLE endpoint_allocation (
    endpoint INTEGER PRIMARY KEY CHECK (endpoint BETWEEN 2 AND 65534),
    source TEXT NOT NULL, device_id TEXT NOT NULL, slot TEXT NOT NULL,
    UNIQUE (source,device_id,slot)
) STRICT;
CREATE TABLE xiaomi_scope (
    uid TEXT PRIMARY KEY NOT NULL, home_id TEXT NOT NULL, group_id TEXT NOT NULL
) STRICT;

PRAGMA user_version = 4;
