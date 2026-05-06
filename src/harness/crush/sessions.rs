use anyhow::Result;

use crate::native_sessions::shared::{
    env_path, home_path, push_existing_path, query_sqlite_sessions, xdg_data_path, NativeSession,
};
use crate::native_sessions::NativeSessionScanner;
use crate::AgentKind;

pub struct CrushScanner;

impl NativeSessionScanner for CrushScanner {
    fn agent_kind(&self) -> AgentKind {
        AgentKind::Crush
    }

    fn scan(&self) -> Result<Vec<NativeSession>> {
        let mut dbs = Vec::new();
        push_existing_path(&mut dbs, env_path("CRUSH_DB"));
        push_existing_path(&mut dbs, env_path("CRUSH_DB_PATH"));
        push_existing_path(
            &mut dbs,
            env_path("CRUSH_DATA_DIR").map(|path| path.join("crush.db")),
        );
        push_existing_path(&mut dbs, xdg_data_path(&["crush", "crush.db"]));
        push_existing_path(
            &mut dbs,
            home_path(&[".local", "share", "crush", "crush.db"]),
        );
        let mut out = Vec::new();
        for db in dbs {
            out.extend(query_sqlite_sessions(
                AgentKind::Crush,
                &db,
                &["sessions", "session", "conversations", "conversation"],
            ));
        }
        Ok(out)
    }
}
