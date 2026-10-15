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
    published INTEGER NOT NULL CHECK (published IN (0,1)),
    configuration_version INTEGER NOT NULL DEFAULT 1 CHECK (configuration_version > 0),
    PRIMARY KEY (source, device_id)
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
