//! Per-actor bearer tokens. A token is shown once, when it is made; the database keeps only
//! its SHA-256 hash, the actor it speaks for, and its scope.

use std::fmt;

use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};

use super::db::DbError;

/// What a token may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Push ops and read.
    Write,
    /// Read the snapshot and the op log only: the PWA's scope.
    Read,
}

impl Scope {
    fn name(self) -> &'static str {
        match self {
            Self::Write => "write",
            Self::Read => "read",
        }
    }

    fn parse(name: &str) -> Result<Self, DbError> {
        match name {
            "write" => Ok(Self::Write),
            "read" => Ok(Self::Read),
            other => Err(DbError::Corrupt(format!("unknown token scope `{other}`"))),
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Who a request speaks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub actor: String,
    pub scope: Scope,
}

/// The stored form of a token.
fn hash(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Make a token for `actor` with `scope` and return it; it is never readable again.
pub fn add(db: &Connection, actor: &str, scope: Scope) -> Result<String, DbError> {
    let actor = actor.trim();
    if actor.is_empty() || actor.chars().any(char::is_control) {
        return Err(DbError::Corrupt(
            "an actor is a name with no control characters".into(),
        ));
    }
    // Two v4 UUIDs: 244 random bits from the OS generator.
    let token = format!(
        "tsk_{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    db.execute(
        "INSERT INTO tokens (hash, actor, scope) VALUES (?1, ?2, ?3)",
        params![hash(&token), actor, scope.name()],
    )?;
    Ok(token)
}

/// The grant a presented token carries, if it is one of ours.
pub fn verify(db: &Connection, token: &str) -> Result<Option<Grant>, DbError> {
    let row: Option<(String, String)> = db
        .query_row(
            "SELECT actor, scope FROM tokens WHERE hash = ?1",
            [hash(token)],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    row.map(|(actor, scope)| {
        Ok(Grant {
            actor,
            scope: Scope::parse(&scope)?,
        })
    })
    .transpose()
}

/// Every actor with a token, with its scopes and how many tokens.
pub fn list(db: &Connection) -> Result<Vec<(String, Scope, i64)>, DbError> {
    let mut statement = db.prepare(
        "SELECT actor, scope, count(*) FROM tokens GROUP BY actor, scope ORDER BY actor, scope",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get(2)?,
        ))
    })?;
    rows.map(|row| {
        let (actor, scope, count) = row?;
        Ok((actor, Scope::parse(&scope)?, count))
    })
    .collect()
}

/// Revoke every token `actor` has; returns how many went.
pub fn revoke(db: &Connection, actor: &str) -> Result<usize, DbError> {
    Ok(db.execute("DELETE FROM tokens WHERE actor = ?1", [actor])?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let mut db = Connection::open_in_memory().unwrap();
        super::super::db::migrate(&mut db).unwrap();
        db
    }

    #[test]
    fn a_token_verifies_to_its_actor_and_scope_and_only_its_hash_is_kept() {
        let db = db();
        let write = add(&db, "mark", Scope::Write).unwrap();
        let read = add(&db, "phone", Scope::Read).unwrap();
        assert_ne!(write, read);
        assert_eq!(
            verify(&db, &write).unwrap(),
            Some(Grant {
                actor: "mark".into(),
                scope: Scope::Write,
            })
        );
        assert_eq!(verify(&db, &read).unwrap().unwrap().scope, Scope::Read);
        assert_eq!(verify(&db, "tsk_wrong").unwrap(), None);
        let stored: Vec<String> = db
            .prepare("SELECT hash FROM tokens")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(stored
            .iter()
            .all(|hash| hash.len() == 64 && !hash.contains("tsk_")));
        assert!(!stored.contains(&write));
    }

    #[test]
    fn revoke_removes_every_token_of_an_actor_and_scopes_are_checked() {
        let db = db();
        let one = add(&db, "pi", Scope::Write).unwrap();
        let two = add(&db, "pi", Scope::Write).unwrap();
        add(&db, "mark", Scope::Read).unwrap();
        assert_eq!(
            list(&db).unwrap(),
            [
                ("mark".to_string(), Scope::Read, 1),
                ("pi".to_string(), Scope::Write, 2)
            ]
        );
        assert_eq!(revoke(&db, "pi").unwrap(), 2);
        assert_eq!(verify(&db, &one).unwrap(), None);
        assert_eq!(verify(&db, &two).unwrap(), None);
        assert!(db
            .execute(
                "INSERT INTO tokens (hash, actor, scope) VALUES ('x', 'y', 'admin')",
                []
            )
            .is_err());
        assert!(add(&db, "  ", Scope::Write).is_err());
    }
}
