use crate::{Result, model::*};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{path::Path, time::Duration};

pub struct Store {
    pub db: Connection,
}
impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let db = Connection::open(path)?;
        db.busy_timeout(Duration::from_secs(5))?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL;",
        )?;
        let version: u32 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version > 2 {
            return Err("database belongs to a newer service version".into());
        }
        db.execute_batch("BEGIN;
          CREATE TABLE IF NOT EXISTS campaigns(id TEXT PRIMARY KEY, data TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS runs(id TEXT PRIMARY KEY, campaign_id TEXT NOT NULL REFERENCES campaigns(id), agent TEXT NOT NULL, token TEXT NOT NULL, data TEXT NOT NULL, UNIQUE(campaign_id,agent));
          CREATE TABLE IF NOT EXISTS submissions(id TEXT PRIMARY KEY, run_id TEXT NOT NULL REFERENCES runs(id), data TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS judgments(id TEXT PRIMARY KEY, run_id TEXT NOT NULL UNIQUE REFERENCES runs(id), token TEXT, data TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS reports(run_id TEXT PRIMARY KEY REFERENCES runs(id), data TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS games(campaign_id TEXT NOT NULL REFERENCES campaigns(id), number INTEGER NOT NULL, data TEXT NOT NULL, PRIMARY KEY(campaign_id,number));
          CREATE TABLE IF NOT EXISTS events(id INTEGER PRIMARY KEY, at_ms INTEGER NOT NULL, run_id TEXT, kind TEXT NOT NULL, data TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS iterations(run_id TEXT NOT NULL REFERENCES runs(id), number INTEGER NOT NULL, digest TEXT NOT NULL, data TEXT NOT NULL, PRIMARY KEY(run_id,number));
          CREATE TABLE IF NOT EXISTS workflows(run_id TEXT PRIMARY KEY REFERENCES runs(id), data TEXT NOT NULL);
          PRAGMA user_version=2; COMMIT;")?;
        Ok(Self { db })
    }
    fn one<T: DeserializeOwned>(&self, sql: &str, id: &str) -> Result<Option<T>> {
        let raw: Option<String> = self.db.query_row(sql, [id], |r| r.get(0)).optional()?;
        raw.map(|s| serde_json::from_str(&s).map_err(Into::into))
            .transpose()
    }
    fn many<T: DeserializeOwned>(&self, sql: &str, args: &[&str]) -> Result<Vec<T>> {
        self.db
            .prepare(sql)?
            .query_map(rusqlite::params_from_iter(args), |r| r.get::<_, String>(0))?
            .map(|row| Ok(serde_json::from_str(&row?)?))
            .collect()
    }
    pub fn campaign(&self, id: &str) -> Result<Campaign> {
        self.one("SELECT data FROM campaigns WHERE id=?", id)?
            .ok_or_else(|| "campaign not found".into())
    }
    pub fn campaigns(&self) -> Result<Vec<Campaign>> {
        self.many("SELECT data FROM campaigns ORDER BY rowid", &[])
    }
    pub fn iterations(&self, run: &str) -> Result<Vec<Value>> {
        self.many(
            "SELECT data FROM iterations WHERE run_id=? ORDER BY number",
            &[run],
        )
    }
    pub fn iteration(&self, run: &str, number: u64, digest: &str) -> Result<Option<Value>> {
        let saved: Option<(String, String)> = self
            .db
            .query_row(
                "SELECT digest,data FROM iterations WHERE run_id=?1 AND number=?2",
                params![run, i64::try_from(number)?],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        saved
            .map(|(hash, data)| {
                if hash != digest {
                    return Err("iteration already submitted with different content".into());
                }
                Ok(serde_json::from_str(&data)?)
            })
            .transpose()
    }
    pub fn save_iteration(
        &self,
        run: &str,
        number: u64,
        digest: &str,
        value: &Value,
    ) -> Result<()> {
        self.db.execute(
            "INSERT INTO iterations VALUES(?1,?2,?3,?4)",
            params![run, i64::try_from(number)?, digest, value.to_string()],
        )?;
        Ok(())
    }
    pub fn workflow(&self, run: &str) -> Result<Option<Value>> {
        self.one("SELECT data FROM workflows WHERE run_id=?", run)
    }
    pub fn workflows(&self) -> Result<Vec<Value>> {
        self.many("SELECT data FROM workflows ORDER BY rowid", &[])
    }
    pub fn save_workflow(&self, run: &str, value: &Value) -> Result<()> {
        self.db.execute("INSERT INTO workflows VALUES(?1,?2) ON CONFLICT(run_id) DO UPDATE SET data=excluded.data", params![run, value.to_string()])?;
        Ok(())
    }
    pub fn save_campaign(&self, c: &Campaign) -> Result<()> {
        self.db.execute(
            "INSERT INTO campaigns VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET data=excluded.data",
            params![c.id, serde_json::to_string(c)?],
        )?;
        Ok(())
    }
    pub fn run(&self, id: &str) -> Result<Run> {
        self.one("SELECT data FROM runs WHERE id=?", id)?
            .ok_or_else(|| "run not found".into())
    }
    pub fn runs(&self, campaign: &str) -> Result<Vec<Run>> {
        self.many(
            "SELECT data FROM runs WHERE campaign_id=? ORDER BY rowid",
            &[campaign],
        )
    }
    pub fn all_runs(&self) -> Result<Vec<Run>> {
        self.many("SELECT data FROM runs ORDER BY rowid", &[])
    }
    pub fn insert_run(&self, r: &Run, token: &str) -> Result<()> {
        self.db.execute(
            "INSERT INTO runs VALUES(?1,?2,?3,?4,?5)",
            params![
                r.id,
                r.campaign_id,
                r.agent,
                hash(token),
                serde_json::to_string(r)?
            ],
        )?;
        Ok(())
    }
    pub fn save_run(&self, r: &Run) -> Result<()> {
        self.db.execute(
            "UPDATE runs SET data=?2 WHERE id=?1",
            params![r.id, serde_json::to_string(r)?],
        )?;
        Ok(())
    }
    pub fn run_auth(&self, id: &str, token: &str) -> Result<bool> {
        let expected: Option<String> = self
            .db
            .query_row("SELECT token FROM runs WHERE id=?", [id], |r| r.get(0))
            .optional()?;
        Ok(expected.is_some_and(|t| t == hash(token)))
    }
    pub fn artifact(&self, id: &str) -> Result<Artifact> {
        self.one("SELECT data FROM submissions WHERE id=?", id)?
            .ok_or_else(|| "submission not found".into())
    }
    pub fn save_artifact(&self, run: &str, a: &Artifact) -> Result<()> {
        self.db.execute(
            "INSERT INTO submissions VALUES(?1,?2,?3)",
            params![a.id, run, serde_json::to_string(a)?],
        )?;
        Ok(())
    }
    pub fn judgment(&self, id: &str) -> Result<Judgment> {
        self.one("SELECT data FROM judgments WHERE id=?", id)?
            .ok_or_else(|| "judgment not found".into())
    }
    pub fn judgments(&self) -> Result<Vec<Judgment>> {
        self.many("SELECT data FROM judgments ORDER BY rowid", &[])
    }
    pub fn save_judgment(&self, j: &Judgment) -> Result<()> {
        self.db.execute("INSERT INTO judgments(id,run_id,data) VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET data=excluded.data", params![j.id,j.run_id,serde_json::to_string(j)?])?;
        Ok(())
    }
    pub fn set_judge_token(&self, id: &str, token: &str) -> Result<()> {
        self.db.execute(
            "UPDATE judgments SET token=?2 WHERE id=?1",
            params![id, hash(token)],
        )?;
        Ok(())
    }
    pub fn judge_auth(&self, id: &str, token: &str) -> Result<bool> {
        let expected: Option<Option<String>> = self
            .db
            .query_row("SELECT token FROM judgments WHERE id=?", [id], |r| r.get(0))
            .optional()?;
        Ok(expected.flatten().is_some_and(|t| t == hash(token)))
    }
    pub fn report(&self, run: &str) -> Result<Option<Value>> {
        self.one("SELECT data FROM reports WHERE run_id=?", run)
    }
    pub fn save_report(&self, run: &str, report: &Value) -> Result<()> {
        self.db.execute("INSERT INTO reports VALUES(?1,?2) ON CONFLICT(run_id) DO UPDATE SET data=excluded.data", params![run, report.to_string()])?;
        Ok(())
    }
    pub fn game(&self, campaign: &str, number: usize, data: &impl Serialize) -> Result<()> {
        self.db.execute(
            "INSERT INTO games VALUES(?1,?2,?3)",
            params![campaign, number as i64, serde_json::to_string(data)?],
        )?;
        Ok(())
    }
    pub fn games(&self, campaign: &str) -> Result<Vec<Value>> {
        self.many(
            "SELECT data FROM games WHERE campaign_id=? ORDER BY number",
            &[campaign],
        )
    }
    pub fn event(&self, run: Option<&str>, kind: &str, data: &impl Serialize) -> Result<()> {
        self.db.execute(
            "INSERT INTO events(at_ms,run_id,kind,data) VALUES(?1,?2,?3,?4)",
            params![crate::now_ms(), run, kind, serde_json::to_string(data)?],
        )?;
        Ok(())
    }
    pub fn events(&self, run: &str) -> Result<Vec<Value>> {
        let mut stmt = self.db.prepare(
            "SELECT at_ms,kind,data FROM events WHERE run_id=? ORDER BY id DESC LIMIT 1000",
        )?;
        stmt.query_map([run], |r| Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?)))?
            .map(|row| { let (at,kind,data)=row?; Ok(serde_json::json!({"at_ms":at,"kind":kind,"data":serde_json::from_str::<Value>(&data)?})) }).collect()
    }
}
pub fn hash(s: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(s.as_bytes()))
}
