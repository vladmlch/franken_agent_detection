use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[cfg(any(
    feature = "cursor",
    feature = "opencode",
    feature = "devin",
    feature = "qoder-sqlite"
))]
use super::session::non_blank_str;

const QODER_TITLE_DB_NAME: &str = "main.sqlite";
const QODER_TITLE_LAYOUTS: [(&str, &str); 2] = [
    (".qoder", "com.qoder.app.stable"),
    (".qoder-cn", "com.qodercn.app.stable"),
];

fn qoder_title_database_candidates(
    home_dir: Option<&Path>,
    config_dir: Option<&Path>,
) -> Vec<(PathBuf, PathBuf)> {
    let (Some(home), Some(config)) = (home_dir, config_dir) else {
        return Vec::new();
    };
    if home.as_os_str().is_empty() || !home.is_absolute() {
        return Vec::new();
    }
    let mut pairs = Vec::with_capacity(QODER_TITLE_LAYOUTS.len() * 2);
    for (dot_dir, app_id) in QODER_TITLE_LAYOUTS {
        let unresolved = home.join(dot_dir).join("projects");
        let store = config.join(app_id).join(QODER_TITLE_DB_NAME);
        if let Ok(resolved) = fs::canonicalize(&unresolved)
            && resolved != unresolved
        {
            pairs.push((resolved, store.clone()));
        }
        pairs.push((unresolved, store));
    }
    pairs
}

#[cfg_attr(not(test), allow(dead_code))]
pub fn qoder_title_database_for_root_resolved(
    root: &Path,
    home_dir: Option<&Path>,
    config_dir: Option<&Path>,
) -> Option<PathBuf> {
    qoder_title_database_candidates(home_dir, config_dir)
        .into_iter()
        .find(|(alias, _)| alias == root)
        .map(|(_, store)| store)
}

pub fn qoder_title_database_for_path_resolved(
    path: &Path,
    home_dir: Option<&Path>,
    config_dir: Option<&Path>,
) -> Option<PathBuf> {
    qoder_title_database_candidates(home_dir, config_dir)
        .into_iter()
        .filter(|(alias, _)| path.starts_with(alias) && path != alias)
        .max_by_key(|(alias, _)| alias.as_os_str().len())
        .map(|(_, store)| store)
}

#[cfg_attr(
    not(any(
        feature = "cursor",
        feature = "opencode",
        feature = "devin",
        feature = "qoder-sqlite"
    )),
    allow(dead_code)
)]
pub(super) fn qoder_title_database_for_path(path: &Path) -> Option<PathBuf> {
    qoder_title_database_for_path_resolved(
        path,
        dirs::home_dir().as_deref(),
        dirs::config_dir().as_deref(),
    )
}

#[cfg(any(
    feature = "cursor",
    feature = "opencode",
    feature = "devin",
    feature = "qoder-sqlite"
))]
pub fn load_qoder_title_map_from_db(db_path: &Path) -> HashMap<String, String> {
    use crate::connectors::sqlite_sync::{
        ConnectionExt, open_read_only_with_snapshot_fallback, store_is_provably_empty,
    };
    use frankensqlite::compat::RowExt;

    if !fs::symlink_metadata(db_path).is_ok_and(|meta| meta.file_type().is_file())
        || store_is_provably_empty(db_path)
    {
        return HashMap::new();
    }

    let Ok(conn) = open_read_only_with_snapshot_fallback(db_path.to_string_lossy().as_ref()) else {
        tracing::debug!(
            db = %db_path.display(),
            "qoder could not open Electron main.sqlite title store"
        );
        return HashMap::new();
    };
    for pragma in [
        "PRAGMA busy_timeout = 2000;",
        "PRAGMA query_only = ON;",
        "PRAGMA trusted_schema = OFF;",
    ] {
        if let Err(err) = conn.execute(pragma) {
            tracing::debug!(db = %db_path.display(), %err, "qoder title store pragma failed");
        }
    }

    let is_table = conn
        .query_row_map(
            "SELECT type FROM sqlite_master WHERE name = 'chat_sessions'",
            &[],
            |row| row.get_typed::<Option<String>>(0),
        )
        .ok()
        .flatten()
        .is_some_and(|kind| kind == "table");
    if !is_table {
        return HashMap::new();
    }

    let Ok(rows) =
        conn.query_map_collect("SELECT session_id, title FROM chat_sessions", &[], |row| {
            Ok((
                row.get_typed::<Option<String>>(0)?,
                row.get_typed::<Option<String>>(1)?,
            ))
        })
    else {
        tracing::debug!(
            db = %db_path.display(),
            "qoder could not query chat_sessions from main.sqlite"
        );
        return HashMap::new();
    };

    let mut titles = HashMap::new();
    for (session_id, title) in rows {
        let Some(session_id) = non_blank_str(session_id.as_deref()) else {
            continue;
        };
        let Some(title) = non_blank_str(title.as_deref()) else {
            continue;
        };
        titles.insert(session_id, title);
    }
    titles
}

#[cfg(not(any(
    feature = "cursor",
    feature = "opencode",
    feature = "devin",
    feature = "qoder-sqlite"
)))]
pub fn load_qoder_title_map_from_db(_db_path: &Path) -> HashMap<String, String> {
    HashMap::new()
}
