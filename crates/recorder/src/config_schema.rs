//! The `schema_version` of `config.toml`, and the one place that validates and migrates it.
//!
//! Every reader of the file (the CLI, the desktop shell's section readers) goes through
//! [`parse`] or [`check`], so a file from a different layout is recognised the same way
//! everywhere instead of each reader guessing from whatever fields happen to be missing.
//!
//! * **Absent** means version 1: every file written before the field existed is v1, and the
//!   field is stamped on the next settings write.
//! * **Newer than [`CURRENT_SCHEMA_VERSION`]** is refused (fail closed). Reading a layout this
//!   build does not know would silently drop or misread settings, and writing it would
//!   destroy them.
//! * **Zero, negative, or not an integer** is refused as malformed.
//! * **Older** files are upgraded in memory, one step at a time, by [`MIGRATIONS`]; the file
//!   on disk is not rewritten by a read.
//!
//! To change the layout: bump [`CURRENT_SCHEMA_VERSION`], append one function to
//! [`MIGRATIONS`] that rewrites a version-`N` table into version `N+1`, and add a test.

use serde::de::DeserializeOwned;
use std::fmt;

/// The layout version this build reads and writes.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// The top-level key that carries the version.
pub const SCHEMA_VERSION_KEY: &str = "schema_version";

/// One upgrade step: rewrites a table of version `N` into version `N + 1`.
pub type Migration = fn(&mut toml::Table);

/// `MIGRATIONS[i]` upgrades version `i + 1` to `i + 2`, so its length is always
/// `CURRENT_SCHEMA_VERSION - 1`. Version 1 is the baseline, hence empty.
pub const MIGRATIONS: &[Migration] = &[];

/// Why a config file's version could not be accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaError {
    /// `schema_version` is present but is not a positive integer.
    Invalid(String),
    /// The file declares a layout newer than this build understands.
    TooNew { found: u32, supported: u32 },
}

impl fmt::Display for SchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SchemaError::Invalid(found) => write!(
                f,
                "`{SCHEMA_VERSION_KEY}` must be a positive whole number, found {found}"
            ),
            SchemaError::TooNew { found, supported } => write!(
                f,
                "the config file uses schema version {found}, but this version of replayed \
                 only understands up to {supported}; update replayed (the file was left \
                 untouched)"
            ),
        }
    }
}

impl std::error::Error for SchemaError {}

/// Why [`parse`] failed.
#[derive(Debug)]
pub enum ParseError {
    Schema(SchemaError),
    Toml(toml::de::Error),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Schema(err) => err.fmt(f),
            ParseError::Toml(err) => err.fmt(f),
        }
    }
}

impl std::error::Error for ParseError {}

impl From<SchemaError> for ParseError {
    fn from(err: SchemaError) -> Self {
        ParseError::Schema(err)
    }
}

/// The version `table` declares: [`SCHEMA_VERSION_KEY`] if present and valid, else 1.
pub fn declared_version(table: &toml::Table) -> Result<u32, SchemaError> {
    match table.get(SCHEMA_VERSION_KEY) {
        None => Ok(1),
        Some(toml::Value::Integer(n)) if *n >= 1 => {
            u32::try_from(*n).map_err(|_| SchemaError::TooNew {
                found: u32::MAX,
                supported: CURRENT_SCHEMA_VERSION,
            })
        }
        Some(other) => Err(SchemaError::Invalid(other.to_string())),
    }
}

/// Validate the declared version and upgrade `table` to `current` with `steps`, stamping the
/// result. Returns the version the table started at. Generic over `current`/`steps` so the
/// chain is testable before a real migration exists.
pub fn migrate_with(
    table: &mut toml::Table,
    current: u32,
    steps: &[Migration],
) -> Result<u32, SchemaError> {
    debug_assert_eq!(
        steps.len() as u32,
        current - 1,
        "one migration per version step"
    );
    let from = declared_version(table)?;
    if from > current {
        return Err(SchemaError::TooNew {
            found: from,
            supported: current,
        });
    }
    for step in &steps[(from - 1) as usize..] {
        step(table);
    }
    table.insert(
        SCHEMA_VERSION_KEY.into(),
        toml::Value::Integer(i64::from(current)),
    );
    Ok(from)
}

/// [`migrate_with`] for this build's layout.
pub fn migrate(table: &mut toml::Table) -> Result<u32, SchemaError> {
    migrate_with(table, CURRENT_SCHEMA_VERSION, MIGRATIONS)
}

/// Validate the version of config text without interpreting anything else. For readers that
/// walk the document themselves rather than deserialising it.
pub fn check(text: &str) -> Result<(), ParseError> {
    let mut table: toml::Table = text.parse().map_err(ParseError::Toml)?;
    migrate(&mut table)?;
    Ok(())
}

/// Parse config text into `T` after validating and migrating its schema version.
pub fn parse<T: DeserializeOwned>(text: &str) -> Result<T, ParseError> {
    let mut table: toml::Table = text.parse().map_err(ParseError::Toml)?;
    migrate(&mut table)?;
    table.try_into().map_err(ParseError::Toml)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(text: &str) -> toml::Table {
        text.parse().unwrap()
    }

    #[test]
    fn a_file_without_the_field_is_version_one_and_gets_stamped() {
        let mut t = table("[buffer]\npre_seconds = 30\n");
        assert_eq!(migrate(&mut t), Ok(1));
        assert_eq!(declared_version(&t), Ok(CURRENT_SCHEMA_VERSION));
    }

    #[test]
    fn the_current_version_is_accepted() {
        let mut t = table(&format!("schema_version = {CURRENT_SCHEMA_VERSION}\n"));
        assert_eq!(migrate(&mut t), Ok(CURRENT_SCHEMA_VERSION));
    }

    #[test]
    fn a_newer_version_is_refused_not_half_read() {
        let mut t = table(&format!(
            "schema_version = {}\n",
            CURRENT_SCHEMA_VERSION + 1
        ));
        let err = migrate(&mut t).unwrap_err();
        assert_eq!(
            err,
            SchemaError::TooNew {
                found: CURRENT_SCHEMA_VERSION + 1,
                supported: CURRENT_SCHEMA_VERSION
            }
        );
        assert!(err.to_string().contains("update replayed"));
        assert!(matches!(
            check("schema_version = 999999999999\n"),
            Err(ParseError::Schema(SchemaError::TooNew { .. }))
        ));
    }

    #[test]
    fn malformed_versions_are_refused() {
        for bad in ["0", "-1", "\"1\"", "1.5", "true", "[1]"] {
            let mut t = table(&format!("schema_version = {bad}\n"));
            assert!(
                matches!(migrate(&mut t), Err(SchemaError::Invalid(_))),
                "{bad} must be refused"
            );
        }
    }

    #[test]
    fn migrations_run_in_order_from_the_declared_version() {
        fn v1_to_v2(t: &mut toml::Table) {
            let old = t.remove("old_name").unwrap();
            t.insert("new_name".into(), old);
        }
        fn v2_to_v3(t: &mut toml::Table) {
            t.insert("added".into(), toml::Value::Boolean(true));
        }
        let steps: &[Migration] = &[v1_to_v2, v2_to_v3];

        let mut from_v1 = table("old_name = 7\n");
        assert_eq!(migrate_with(&mut from_v1, 3, steps), Ok(1));
        assert_eq!(from_v1["new_name"].as_integer(), Some(7));
        assert_eq!(from_v1["added"].as_bool(), Some(true));
        assert_eq!(from_v1[SCHEMA_VERSION_KEY].as_integer(), Some(3));

        let mut from_v2 = table("schema_version = 2\nnew_name = 7\n");
        assert_eq!(migrate_with(&mut from_v2, 3, steps), Ok(2));
        assert_eq!(
            from_v2["added"].as_bool(),
            Some(true),
            "only the remaining step ran"
        );

        let mut v4 = table("schema_version = 4\n");
        assert!(matches!(
            migrate_with(&mut v4, 3, steps),
            Err(SchemaError::TooNew { .. })
        ));
    }

    #[test]
    fn parse_deserialises_after_migrating_and_reports_toml_errors() {
        #[derive(serde::Deserialize)]
        struct S {
            x: u32,
        }
        assert_eq!(parse::<S>("x = 4\n").unwrap().x, 4);
        assert_eq!(parse::<S>("schema_version = 1\nx = 5\n").unwrap().x, 5);
        assert!(matches!(
            parse::<S>("x = \"no\"\n"),
            Err(ParseError::Toml(_))
        ));
        assert!(matches!(parse::<S>("x ="), Err(ParseError::Toml(_))));
        assert!(matches!(
            parse::<S>("schema_version = 2\nx = 5\n"),
            Err(ParseError::Schema(_))
        ));
    }

    #[test]
    fn the_example_config_declares_the_current_version() {
        let example = include_str!("../../../config.example.toml");
        let t = table(example);
        assert_eq!(
            t[SCHEMA_VERSION_KEY].as_integer(),
            Some(i64::from(CURRENT_SCHEMA_VERSION))
        );
    }
}
