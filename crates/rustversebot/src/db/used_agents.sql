CREATE TABLE IF NOT EXISTS used_agents (
    uid TEXT NOT NULL,
    avatar_id INTEGER NOT NULL,
    PRIMARY KEY (uid, avatar_id)
) WITHOUT ROWID;

-- A battle confirms both use and roster membership. Profile updates only supply
-- roster membership and names. Keep both facts after the source snapshot expires.
CREATE TRIGGER IF NOT EXISTS cache_used_agent AFTER INSERT ON used_agents BEGIN
    INSERT OR IGNORE INTO avatar_cache(uid, avatar_id, name)
    VALUES (NEW.uid, NEW.avatar_id, '');
END;

INSERT OR IGNORE INTO used_agents(uid, avatar_id)
SELECT DISTINCT uid, avatar_id FROM played_agents;

DROP TRIGGER collect_played_agents;
CREATE TRIGGER collect_played_agents AFTER INSERT ON endgame_results BEGIN
    INSERT OR IGNORE INTO used_agents(uid, avatar_id)
    SELECT uid, avatar_id FROM played_agents WHERE snapshot_id = NEW.id;
END;
PRAGMA user_version = 18;
