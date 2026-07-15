use serde::{Deserialize, Serialize};

/// Configuration for one downloadable public reference database.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PublicDatabase {
    /// Public object URL or object-prefix URL, such as `s3://bucket/path/`.
    pub s3_url: String,
    /// Human-readable description written to progress logs.
    pub description: String,
    /// Whether `s3_url` addresses one object or a prefix to enumerate.
    #[serde(alias = "db_type")]
    pub database_type: DatabaseType,
    /// Optional wildcard exclusion pattern applied before `include`.
    pub exclude: Option<String>,
    /// Optional wildcard inclusion pattern applied after `exclude`.
    pub include: Option<String>,
}

/// The shape of the public database source in the YAML configuration.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DatabaseType {
    Folder,
    File,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn deserializes_folder_database_from_yaml() {
        let databases: HashMap<String, PublicDatabase> = serde_yaml::from_str(
            r#"
ncbi_nt:
  s3_url: s3://ncbi-blast-databases/current/
  description: NCBI nt database
  database_type: folder
  exclude: "*"
  include: "nt.*"
"#,
        )
        .unwrap();

        let database = databases.get("ncbi_nt").unwrap();
        assert_eq!(database.database_type, DatabaseType::Folder);
        assert_eq!(database.include.as_deref(), Some("nt.*"));
    }
}
