-- Only team members identify played agents. Bosses and bangboo also have IDs.
CREATE VIEW IF NOT EXISTS played_agents AS
SELECT r.id AS snapshot_id, r.uid, avatar.value ->> '$.id' AS avatar_id
FROM endgame_results AS r
CROSS JOIN json_each(CASE WHEN json_valid(r.data_json) THEN r.data_json END) AS section
CROSS JOIN json_each(CASE
    WHEN r.endgame_type = 'deadly_assault' AND section.key IN ('list', 'hard_list')
         AND section.type = 'array' THEN section.value
    WHEN r.endgame_type = 'shiyu_defense' AND section.type = 'object'
         THEN section.value -> '$.layer_challenge_info_list'
END) AS room
CROSS JOIN json_each(CASE WHEN room.type = 'object' THEN room.value END, '$.avatar_list') AS avatar
WHERE CASE WHEN avatar.type = 'object' THEN json_type(avatar.value, '$.id') END = 'integer'
  AND avatar_id > 0;

-- Battle records carry IDs. A later profile lookup can supply a display name.
INSERT OR IGNORE INTO avatar_cache(uid, avatar_id, name)
SELECT DISTINCT uid, avatar_id, '' FROM played_agents;

CREATE TRIGGER IF NOT EXISTS collect_played_agents AFTER INSERT ON endgame_results BEGIN
    INSERT OR IGNORE INTO avatar_cache(uid, avatar_id, name)
    SELECT uid, avatar_id, '' FROM played_agents WHERE snapshot_id = NEW.id;
END;
PRAGMA user_version = 17;
