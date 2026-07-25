use std::collections::HashMap;
use std::sync::OnceLock;

/// Static mapping of all known ZZZ agent IDs → names.
fn static_agent_map() -> &'static HashMap<i64, &'static str> {
    static MAP: OnceLock<HashMap<i64, &str>> = OnceLock::new();
    MAP.get_or_init(|| {
        HashMap::from([
            /*// S-Rank
            (1011, "Anby"),
            (1021, "Nekomata"),
            (1031, "Nicole"),
            (1041, "Soldier 11"),
            (1051, "Yidhari"),
            (1061, "Corin"),
            (1071, "Billy"),
            (1081, "Anton"),
            (1091, "Miyabi"),
            (1101, "Koleda"),
            (1111, "Ben"),
            (1121, "Soukaku"),
            (1131, "Lycaon"),
            (1141, "Lucy"),
            (1151, "Piper"),
            (1161, "Grace"),
            (1171, "Rina"),
            (1181, "Yanagi"),
            (1191, "Harumasa"),
            (1201, "Astra Yao"),
            (1211, "Evelyn"),
            (1221, "Yanagi"),
            (1241, "Zhu Yuan"),
            (1251, "Qingyi"),
            (1261, "Jane Doe"),
            (1271, "Seth"),
            (1281, "Piper"),
            (1291, "Caesar"),
            (1301, "Orfea & Magus"),
            // Note: 1311 doesn't exist in standard numbering... check
            (1311, "Burnice"),
            (1331, "Vivian"),
            (1341, "Zhao"),
            (1351, "Pulchra"),
            (1361, "Trigger"),
            (1371, "Yixuan"),
            (1381, "Soldier 0 Anby"),
            (1411, "Yuzuha"),
            (1421, "Pan Yinhui"),
            (1431, "Ye Shunguang"),
            (1441, "Manato"),
            (1451, "Lucia"),
            (1481, "Dialyn"),
            (1491, "Sunna"),
            (1501, "Aria"),
            (1511, "Nangong Yu"),
            (1521, "Cissia"),
            (1531, "Hoshino Masa"),
            (1541, "Promeia"),
            (1551, "Pyrois"),
            (1561, "Velina"),
            // A-Rank*/
        ])
    })
}

/// Try to resolve an agent ID to a name using static DB.
pub fn static_resolve(id: i64) -> Option<&'static str> {
    static_agent_map().get(&id).copied()
}
