//! The beta list: one row per nostr pubkey.

use std::sync::Mutex;

use anyhow::Result;
use rusqlite::{Connection, params};

pub struct Store(Mutex<Connection>);

#[derive(Debug, Clone, serde::Serialize)]
pub struct Signup {
    pub pubkey: String,
    /// Signed for by the key itself (NIP-98), not just typed in.
    pub verified: bool,
    pub note: String,
    pub created_at: u64,
}

impl Store {
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Self::init(conn)
    }

    #[cfg(test)]
    pub fn in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS signups (
                 pubkey TEXT PRIMARY KEY,
                 verified INTEGER NOT NULL,
                 note TEXT NOT NULL DEFAULT '',
                 created_at INTEGER NOT NULL,
                 updated_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS mints (
                 name TEXT PRIMARY KEY,
                 owner TEXT NOT NULL,
                 enabled INTEGER NOT NULL DEFAULT 1,
                 commit_id TEXT,
                 created_at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS mints_owner ON mints (owner);",
        )?;
        // an operator lets a beta sign-up provision a mint
        let has_approved: bool = conn
            .prepare("SELECT 1 FROM pragma_table_info('signups') WHERE name = 'approved'")?
            .exists([])?;
        if !has_approved {
            conn.execute_batch(
                "ALTER TABLE signups ADD COLUMN approved INTEGER NOT NULL DEFAULT 0",
            )?;
        }
        Ok(Store(Mutex::new(conn)))
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Add `pubkey` to the list, or update its note. A verified sign-up
    /// replaces an unverified one; an unverified one never touches a row that
    /// exists. Returns the pubkey's place in the list (1-based) and whether
    /// its row is verified now.
    pub fn sign_up(
        &self,
        pubkey: &str,
        verified: bool,
        note: &str,
        now: u64,
    ) -> Result<(u64, bool)> {
        let conn = self.conn();
        if verified {
            conn.execute(
                "INSERT INTO signups (pubkey, verified, note, created_at, updated_at) VALUES (?1, 1, ?2, ?3, ?3)
                 ON CONFLICT(pubkey) DO UPDATE SET verified = 1, note = excluded.note, updated_at = excluded.updated_at",
                params![pubkey, note, now as i64],
            )?;
        } else {
            conn.execute(
                "INSERT OR IGNORE INTO signups (pubkey, verified, note, created_at, updated_at) VALUES (?1, 0, ?2, ?3, ?3)",
                params![pubkey, note, now as i64],
            )?;
        }
        let (rowid, verified): (i64, bool) = conn.query_row(
            "SELECT rowid, verified FROM signups WHERE pubkey = ?1",
            [pubkey],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let place: i64 = conn.query_row(
            "SELECT COUNT(*) FROM signups WHERE rowid <= ?1",
            [rowid],
            |r| r.get(0),
        )?;
        Ok((place as u64, verified))
    }

    /// Let `pubkey` provision mints. Whether it is on the list at all.
    pub fn approve(&self, pubkey: &str, approved: bool) -> Result<bool> {
        Ok(self.conn().execute(
            "UPDATE signups SET approved = ?2 WHERE pubkey = ?1",
            params![pubkey, approved],
        )? == 1)
    }

    /// Approved and verified: only a key that signed for itself provisions.
    pub fn may_provision(&self, pubkey: &str) -> Result<bool> {
        Ok(self
            .conn()
            .prepare("SELECT 1 FROM signups WHERE pubkey = ?1 AND approved = 1 AND verified = 1")?
            .exists([pubkey])?)
    }

    /// Claim `name` for `owner`, before its commit. Refuses a taken name.
    pub fn claim_mint(&self, name: &str, owner: &str, now: u64) -> Result<bool> {
        Ok(self.conn().execute(
            "INSERT OR IGNORE INTO mints (name, owner, created_at) VALUES (?1, ?2, ?3)",
            params![name, owner, now as i64],
        )? == 1)
    }

    /// A claim whose commit failed: give the name back.
    pub fn release_mint(&self, name: &str) -> Result<()> {
        self.conn().execute(
            "DELETE FROM mints WHERE name = ?1 AND commit_id IS NULL",
            [name],
        )?;
        Ok(())
    }

    pub fn mint_committed(&self, name: &str, commit: &str, enabled: bool) -> Result<()> {
        self.conn().execute(
            "UPDATE mints SET commit_id = ?2, enabled = ?3 WHERE name = ?1",
            params![name, commit, enabled],
        )?;
        Ok(())
    }

    pub fn mint(&self, name: &str) -> Result<Option<MintRow>> {
        use rusqlite::OptionalExtension;
        Ok(self
            .conn()
            .query_row(
                "SELECT name, owner, enabled, commit_id, created_at FROM mints WHERE name = ?1",
                [name],
                mint_row,
            )
            .optional()?)
    }

    /// Every mint, or `owner`'s.
    pub fn mints(&self, owner: Option<&str>) -> Result<Vec<MintRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT name, owner, enabled, commit_id, created_at FROM mints
             WHERE ?1 IS NULL OR owner = ?1 ORDER BY created_at",
        )?;
        let rows = stmt
            .query_map([owner], mint_row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    pub fn count(&self) -> Result<u64> {
        Ok(self
            .conn()
            .query_row("SELECT COUNT(*) FROM signups", [], |r| r.get::<_, i64>(0))?
            as u64)
    }

    #[cfg(test)]
    pub fn get(&self, pubkey: &str) -> Result<Option<Signup>> {
        use rusqlite::OptionalExtension;
        Ok(self
            .conn()
            .query_row(
                "SELECT pubkey, verified, note, created_at FROM signups WHERE pubkey = ?1",
                [pubkey],
                row,
            )
            .optional()?)
    }

    pub fn all(&self) -> Result<Vec<Signup>> {
        let conn = self.conn();
        let mut stmt =
            conn.prepare("SELECT pubkey, verified, note, created_at FROM signups ORDER BY rowid")?;
        let rows = stmt.query_map([], row)?.collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MintRow {
    pub name: String,
    pub owner: String,
    pub enabled: bool,
    /// The commit that last changed it; `None` while its first is pending.
    pub commit_id: Option<String>,
    pub created_at: u64,
}

fn mint_row(r: &rusqlite::Row) -> rusqlite::Result<MintRow> {
    Ok(MintRow {
        name: r.get(0)?,
        owner: r.get(1)?,
        enabled: r.get(2)?,
        commit_id: r.get(3)?,
        created_at: r.get::<_, i64>(4)? as u64,
    })
}

fn row(r: &rusqlite::Row) -> rusqlite::Result<Signup> {
    Ok(Signup {
        pubkey: r.get(0)?,
        verified: r.get(1)?,
        note: r.get(2)?,
        created_at: r.get::<_, i64>(3)? as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verified_sign_ups_win() {
        let store = Store::in_memory().unwrap();
        assert_eq!(store.sign_up("aa", false, "typed", 1).unwrap(), (1, false));
        assert_eq!(store.sign_up("bb", true, "signed", 2).unwrap(), (2, true));
        // an unverified repeat changes nothing
        assert_eq!(store.sign_up("bb", false, "spoofed", 3).unwrap(), (2, true));
        assert_eq!(store.get("bb").unwrap().unwrap().note, "signed");
        // signing for a typed-in key upgrades it, keeping its place
        assert_eq!(
            store.sign_up("aa", true, "now signed", 4).unwrap(),
            (1, true)
        );
        let a = store.get("aa").unwrap().unwrap();
        assert!(a.verified && a.note == "now signed" && a.created_at == 1);
        assert_eq!(store.count().unwrap(), 2);
    }

    #[test]
    fn only_approved_verified_keys_provision() {
        let store = Store::in_memory().unwrap();
        store.sign_up("aa", false, "", 1).unwrap();
        store.sign_up("bb", true, "", 1).unwrap();
        assert!(store.approve("aa", true).unwrap());
        assert!(!store.approve("zz", true).unwrap(), "not on the list");
        assert!(
            !store.may_provision("aa").unwrap(),
            "typed in, never signed"
        );
        assert!(!store.may_provision("bb").unwrap(), "not approved");
        store.approve("bb", true).unwrap();
        assert!(store.may_provision("bb").unwrap());

        assert!(store.claim_mint("satoshi", "bb", 5).unwrap());
        assert!(!store.claim_mint("satoshi", "aa", 6).unwrap());
        store.release_mint("satoshi").unwrap();
        assert!(store.claim_mint("satoshi", "bb", 7).unwrap());
        store.mint_committed("satoshi", "a3f9c1e", true).unwrap();
        store.release_mint("satoshi").unwrap();
        assert_eq!(
            store.mint("satoshi").unwrap().unwrap().commit_id.as_deref(),
            Some("a3f9c1e")
        );
        assert_eq!(store.mints(Some("bb")).unwrap().len(), 1);
        assert_eq!(store.mints(Some("aa")).unwrap().len(), 0);
        assert_eq!(store.mints(None).unwrap().len(), 1);
    }
}
