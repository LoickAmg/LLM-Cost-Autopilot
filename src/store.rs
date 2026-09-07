//! Journal SQLite des décisions de routage, pour le suivi de budget et le
//! rapport d'économies (`autopilot report`).

use rusqlite::{params, Connection, OptionalExtension};
use std::time::{SystemTime, UNIX_EPOCH};

/// Un événement de routage journalisé après chaque requête traitée.
#[derive(Debug, Clone, PartialEq)]
pub struct RoutingEvent {
    pub timestamp: u64,
    pub route: String,
    pub complexity: f64,
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub cost_usd: f64,
    pub latency_ms: u64,
    pub success: bool,
    pub fallback: bool,
    pub decision_reason: String,
}

/// Nombre de secondes dans un jour, pour regrouper les dépenses par jour civil
/// (en UTC — suffisant pour un budget indicatif, pas un usage comptable strict).
const SECONDS_PER_DAY: u64 = 86_400;

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("l'horloge système est avant 1970")
        .as_secs()
}

/// Journal des événements de routage, adossé à SQLite (fichier ou `:memory:`).
pub struct EventStore {
    conn: Connection,
}

impl EventStore {
    pub fn open(path: &str) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        Self::init_schema(&conn)?;
        Ok(EventStore { conn })
    }

    pub fn open_in_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::init_schema(&conn)?;
        Ok(EventStore { conn })
    }

    fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS routing_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp INTEGER NOT NULL,
                route TEXT NOT NULL,
                complexity REAL NOT NULL,
                input_tokens INTEGER NOT NULL,
                output_tokens INTEGER NOT NULL,
                cost_usd REAL NOT NULL,
                latency_ms INTEGER NOT NULL,
                success INTEGER NOT NULL,
                fallback INTEGER NOT NULL,
                decision_reason TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_routing_events_timestamp
                ON routing_events(timestamp);
            CREATE INDEX IF NOT EXISTS idx_routing_events_route
                ON routing_events(route);",
        )
    }

    pub fn record(&self, event: &RoutingEvent) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO routing_events
                (timestamp, route, complexity, input_tokens, output_tokens,
                 cost_usd, latency_ms, success, fallback, decision_reason)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                event.timestamp as i64,
                event.route,
                event.complexity,
                event.input_tokens as i64,
                event.output_tokens as i64,
                event.cost_usd,
                event.latency_ms as i64,
                event.success as i64,
                event.fallback as i64,
                event.decision_reason,
            ],
        )?;
        Ok(())
    }

    /// Toutes les entrées, triées par horodatage croissant.
    pub fn all(&self) -> rusqlite::Result<Vec<RoutingEvent>> {
        self.query_since(0)
    }

    pub fn query_since(&self, since: u64) -> rusqlite::Result<Vec<RoutingEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT timestamp, route, complexity, input_tokens, output_tokens,
                    cost_usd, latency_ms, success, fallback, decision_reason
             FROM routing_events WHERE timestamp >= ?1 ORDER BY timestamp ASC",
        )?;
        let rows = stmt.query_map(params![since as i64], |row| {
            Ok(RoutingEvent {
                timestamp: row.get::<_, i64>(0)? as u64,
                route: row.get(1)?,
                complexity: row.get(2)?,
                input_tokens: row.get::<_, i64>(3)? as usize,
                output_tokens: row.get::<_, i64>(4)? as usize,
                cost_usd: row.get(5)?,
                latency_ms: row.get::<_, i64>(6)? as u64,
                success: row.get::<_, i64>(7)? != 0,
                fallback: row.get::<_, i64>(8)? != 0,
                decision_reason: row.get(9)?,
            })
        })?;
        rows.collect()
    }

    /// Somme des coûts enregistrés depuis le début du jour civil courant
    /// (fenêtre glissante de `SECONDS_PER_DAY` secondes alignée sur epoch).
    pub fn cost_today(&self, now: u64) -> rusqlite::Result<f64> {
        let day_start = (now / SECONDS_PER_DAY) * SECONDS_PER_DAY;
        let sum: Option<f64> = self
            .conn
            .query_row(
                "SELECT SUM(cost_usd) FROM routing_events WHERE timestamp >= ?1",
                params![day_start as i64],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        Ok(sum.unwrap_or(0.0))
    }

    pub fn count(&self) -> rusqlite::Result<u64> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM routing_events", [], |row| row.get(0))?;
        Ok(n as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_event(route: &str, cost: f64, timestamp: u64) -> RoutingEvent {
        RoutingEvent {
            timestamp,
            route: route.to_string(),
            complexity: 0.4,
            input_tokens: 100,
            output_tokens: 50,
            cost_usd: cost,
            latency_ms: 120,
            success: true,
            fallback: false,
            decision_reason: "test".to_string(),
        }
    }

    #[test]
    fn round_trips_a_single_event() {
        let store = EventStore::open_in_memory().unwrap();
        store.record(&sample_event("local", 0.0, 1000)).unwrap();
        let events = store.all().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].route, "local");
    }

    #[test]
    fn events_are_sorted_ascending_by_timestamp() {
        let store = EventStore::open_in_memory().unwrap();
        store.record(&sample_event("a", 0.0, 3000)).unwrap();
        store.record(&sample_event("b", 0.0, 1000)).unwrap();
        store.record(&sample_event("c", 0.0, 2000)).unwrap();
        let events = store.all().unwrap();
        let timestamps: Vec<u64> = events.iter().map(|e| e.timestamp).collect();
        assert_eq!(timestamps, vec![1000, 2000, 3000]);
    }

    #[test]
    fn cost_today_only_sums_events_within_the_current_day_bucket() {
        let store = EventStore::open_in_memory().unwrap();
        let now = 10 * SECONDS_PER_DAY + 500; // milieu du jour 10
        let yesterday = now - SECONDS_PER_DAY - 1;
        store
            .record(&sample_event("paid-cheap", 1.0, yesterday))
            .unwrap();
        store.record(&sample_event("paid-cheap", 2.5, now)).unwrap();
        let total = store.cost_today(now).unwrap();
        assert_eq!(
            total, 2.5,
            "la dépense d'hier ne doit pas compter aujourd'hui"
        );
    }

    #[test]
    fn cost_today_is_zero_with_no_events() {
        let store = EventStore::open_in_memory().unwrap();
        assert_eq!(store.cost_today(now_unix()).unwrap(), 0.0);
    }

    #[test]
    fn persists_across_reopen_of_the_same_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        let path_str = path.to_str().unwrap();
        {
            let store = EventStore::open(path_str).unwrap();
            store.record(&sample_event("local", 0.0, 1)).unwrap();
        }
        {
            let store = EventStore::open(path_str).unwrap();
            assert_eq!(store.count().unwrap(), 1);
        }
    }

    #[test]
    fn count_reflects_number_of_recorded_events() {
        let store = EventStore::open_in_memory().unwrap();
        assert_eq!(store.count().unwrap(), 0);
        store.record(&sample_event("local", 0.0, 1)).unwrap();
        store.record(&sample_event("local", 0.0, 2)).unwrap();
        assert_eq!(store.count().unwrap(), 2);
    }
}
