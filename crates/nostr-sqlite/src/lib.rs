// Copyright (c) 2022-2023 Yuki Kishimoto
// Copyright (c) 2023-2024 Rust Nostr Developers
// Distributed under the MIT software license

//! SQLite Storage backend for Nostr SDK

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(rustdoc::bare_urls)]
#![allow(clippy::mutable_key_type)] // TODO: remove when possible. Needed to suppress false positive for `BTreeSet<Event>`

use std::collections::{BTreeSet, HashSet};
use std::path::Path;
use std::sync::Arc;

pub extern crate nostr;
pub extern crate nostr_database as database;

use async_trait::async_trait;
use nostr::nips::nip01::Coordinate;
use nostr::{Event, EventId, Filter, Timestamp, Url};
use nostr_database::{
    Backend, DatabaseEventResult, DatabaseHelper, FlatBufferBuilder, FlatBufferDecode,
    FlatBufferEncode, NostrDatabase, Order,
};
use rusqlite::config::DbConfig;
use rusqlite::Connection;
use tokio::sync::RwLock;

mod error;
mod migration;
mod pool;

pub use self::error::Error;
use self::migration::STARTUP_SQL;
use self::pool::Pool;

/// SQLite Nostr Database
#[derive(Debug, Clone)]
pub struct SQLiteDatabase {
    pool: Pool,
    helper: DatabaseHelper,
    fbb: Arc<RwLock<FlatBufferBuilder<'static>>>,
}

impl SQLiteDatabase {
    async fn new<P>(path: P, helper: DatabaseHelper) -> Result<Self, Error>
    where
        P: AsRef<Path>,
    {
        let conn = Connection::open(path)?;
        let pool: Pool = Pool::new(conn);

        // Execute migrations
        migration::run(&pool).await?;

        let this = Self {
            pool,
            helper,
            fbb: Arc::new(RwLock::new(FlatBufferBuilder::with_capacity(70_000))),
        };

        this.bulk_load().await?;

        Ok(this)
    }

    /// Open database with **unlimited** capacity
    #[inline]
    pub async fn open<P>(path: P) -> Result<Self, Error>
    where
        P: AsRef<Path>,
    {
        Self::new(path, DatabaseHelper::unbounded()).await
    }

    /// Open database with **limited** capacity
    #[inline]
    pub async fn open_bounded<P>(path: P, max_capacity: usize) -> Result<Self, Error>
    where
        P: AsRef<Path>,
    {
        Self::new(path, DatabaseHelper::bounded(max_capacity)).await
    }

    #[tracing::instrument(skip_all)]
    async fn bulk_load(&self) -> Result<(), Error> {
        let events = self
            .pool
            .interact(move |conn| {
                // Query
                let mut stmt = conn.prepare("SELECT event FROM events;")?;
                let mut rows = stmt.query([])?;

                // Decode
                let mut events = BTreeSet::new();
                while let Ok(Some(row)) = rows.next() {
                    let buf: &[u8] = row.get_ref(0)?.as_bytes()?;
                    let event = Event::decode(buf)?;
                    events.insert(event);
                }
                Ok::<BTreeSet<Event>, Error>(events)
            })
            .await??;

        // Build indexes
        let to_discard: HashSet<EventId> = self.helper.bulk_load(events).await;

        // Discard events
        if !to_discard.is_empty() {
            self.pool
                .interact(move |conn| {
                    let mut stmt = conn.prepare_cached("DELETE FROM events WHERE event_id = ?;")?;
                    for id in to_discard.into_iter() {
                        stmt.execute([id.to_hex()])?;
                    }
                    Ok::<(), Error>(())
                })
                .await??;
        }
        Ok(())
    }
}

#[async_trait]
impl NostrDatabase for SQLiteDatabase {
    type Err = Error;

    fn backend(&self) -> Backend {
        Backend::SQLite
    }

    #[tracing::instrument(skip_all, level = "trace")]
    async fn save_event(&self, event: &Event) -> Result<bool, Self::Err> {
        // Index event
        let DatabaseEventResult {
            to_store,
            to_discard,
        } = self.helper.index_event(event).await;

        if !to_discard.is_empty() {
            self.pool
                .interact(move |conn| {
                    let mut stmt = conn.prepare_cached("DELETE FROM events WHERE event_id = ?;")?;
                    for id in to_discard.into_iter() {
                        stmt.execute([id.to_hex()])?;
                    }
                    Ok::<(), Error>(())
                })
                .await??;
        }

        if to_store {
            // Acquire FlatBuffers Builder
            let mut fbb = self.fbb.write().await;

            // Encode
            let event_id: EventId = event.id;
            let value: Vec<u8> = event.encode(&mut fbb).to_vec();

            // Save event
            self.pool
                .interact(move |conn| {
                    let mut stmt = conn.prepare_cached(
                        "INSERT OR IGNORE INTO events (event_id, event) VALUES (?, ?);",
                    )?;
                    stmt.execute((event_id.to_hex(), value))
                })
                .await??;

            Ok(true)
        } else {
            Ok(false)
        }
    }

    #[tracing::instrument(skip_all, level = "trace")]
    async fn bulk_import(&self, events: BTreeSet<Event>) -> Result<(), Self::Err> {
        // Acquire FlatBuffers Builder
        let mut fbb = self.fbb.write().await;

        // Events to store
        let events = self.helper.bulk_import(events).await;

        // Encode
        let events: Vec<(EventId, Vec<u8>)> = events
            .into_iter()
            .map(move |e| {
                let event_id: EventId = e.id;
                let value: Vec<u8> = e.encode(&mut fbb).to_vec();
                (event_id, value)
            })
            .collect();

        // Bulk save
        self.pool
            .interact(move |conn| {
                let tx = conn.transaction()?;

                for (event_id, value) in events.into_iter() {
                    tx.execute(
                        "INSERT OR IGNORE INTO events (event_id, event) VALUES (?, ?);",
                        (event_id.to_hex(), value),
                    )?;
                }

                tx.commit()
            })
            .await??;

        Ok(())
    }

    async fn has_event_already_been_saved(&self, event_id: &EventId) -> Result<bool, Self::Err> {
        if self.helper.has_event_id_been_deleted(event_id).await {
            Ok(true)
        } else {
            let event_id: String = event_id.to_hex();
            self.pool
                .interact(move |conn| {
                    let mut stmt = conn.prepare_cached(
                        "SELECT EXISTS(SELECT 1 FROM events WHERE event_id = ? LIMIT 1);",
                    )?;
                    let mut rows = stmt.query([event_id])?;
                    let exists: u8 = match rows.next()? {
                        Some(row) => row.get(0)?,
                        None => 0,
                    };
                    Ok(exists == 1)
                })
                .await?
        }
    }

    async fn has_event_already_been_seen(&self, event_id: &EventId) -> Result<bool, Self::Err> {
        let event_id: String = event_id.to_hex();
        self.pool
            .interact(move |conn| {
                let mut stmt = conn.prepare_cached(
                    "SELECT EXISTS(SELECT 1 FROM event_seen_by_relays WHERE event_id = ? LIMIT 1);",
                )?;
                let mut rows = stmt.query([event_id])?;
                let exists: u8 = match rows.next()? {
                    Some(row) => row.get(0)?,
                    None => 0,
                };
                Ok(exists == 1)
            })
            .await?
    }

    async fn has_event_id_been_deleted(&self, event_id: &EventId) -> Result<bool, Self::Err> {
        Ok(self.helper.has_event_id_been_deleted(event_id).await)
    }

    async fn has_coordinate_been_deleted(
        &self,
        coordinate: &Coordinate,
        timestamp: Timestamp,
    ) -> Result<bool, Self::Err> {
        Ok(self
            .helper
            .has_coordinate_been_deleted(coordinate, timestamp)
            .await)
    }

    async fn event_id_seen(&self, event_id: EventId, relay_url: Url) -> Result<(), Self::Err> {
        self.pool
            .interact(move |conn| {
                let mut stmt = conn.prepare_cached(
                "INSERT OR IGNORE INTO event_seen_by_relays (event_id, relay_url) VALUES (?, ?);",
            )?;
                stmt.execute((event_id.to_hex(), relay_url.to_string()))
            })
            .await??;
        Ok(())
    }

    async fn event_seen_on_relays(
        &self,
        event_id: EventId,
    ) -> Result<Option<HashSet<Url>>, Self::Err> {
        self.pool
            .interact(move |conn| {
                let mut stmt = conn.prepare_cached(
                    "SELECT relay_url FROM event_seen_by_relays WHERE event_id = ?;",
                )?;
                let mut rows = stmt.query([event_id.to_hex()])?;
                let mut relays = HashSet::new();
                while let Ok(Some(row)) = rows.next() {
                    let url: &str = row.get_ref(0)?.as_str()?;
                    relays.insert(Url::parse(url)?);
                }
                Ok(Some(relays))
            })
            .await?
    }

    #[tracing::instrument(skip_all, level = "trace")]
    async fn event_by_id(&self, event_id: EventId) -> Result<Event, Self::Err> {
        self.pool
            .interact(move |conn| {
                let mut stmt =
                    conn.prepare_cached("SELECT event FROM events WHERE event_id = ?;")?;
                let mut rows = stmt.query([event_id.to_hex()])?;
                let row = rows
                    .next()?
                    .ok_or_else(|| Error::NotFound("event".into()))?;
                let buf: &[u8] = row.get_ref(0)?.as_bytes()?;
                Ok(Event::decode(buf)?)
            })
            .await?
    }

    #[inline]
    #[tracing::instrument(skip_all, level = "trace")]
    async fn count(&self, filters: Vec<Filter>) -> Result<usize, Self::Err> {
        Ok(self.helper.count(filters).await)
    }

    #[inline]
    #[tracing::instrument(skip_all)]
    async fn query(&self, filters: Vec<Filter>, order: Order) -> Result<Vec<Event>, Self::Err> {
        Ok(self.helper.query(filters, order).await)
    }

    #[inline]
    async fn negentropy_items(
        &self,
        filter: Filter,
    ) -> Result<Vec<(EventId, Timestamp)>, Self::Err> {
        Ok(self.helper.negentropy_items(filter).await)
    }

    async fn delete(&self, filter: Filter) -> Result<(), Self::Err> {
        match self.helper.delete(filter).await {
            Some(ids) => {
                self.pool
                    .interact(move |conn| {
                        let mut stmt =
                            conn.prepare_cached("DELETE FROM events WHERE event_id = ?;")?;
                        for id in ids.into_iter() {
                            stmt.execute([id.to_hex()])?;
                        }
                        Ok::<(), Error>(())
                    })
                    .await??;
            }
            None => {
                self.pool
                    .interact(move |conn| conn.execute("DELETE FROM events;", []))
                    .await??;
            }
        };

        Ok(())
    }

    async fn wipe(&self) -> Result<(), Self::Err> {
        self.pool
            .interact(|conn| {
                // Reset DB
                conn.set_db_config(DbConfig::SQLITE_DBCONFIG_RESET_DATABASE, true)?;
                conn.execute("VACUUM;", [])?;
                conn.set_db_config(DbConfig::SQLITE_DBCONFIG_RESET_DATABASE, false)?;

                // Execute migrations
                conn.execute_batch(STARTUP_SQL)?;

                Ok::<(), Error>(())
            })
            .await??;

        migration::run(&self.pool).await?;

        self.helper.clear().await;

        Ok(())
    }
}
